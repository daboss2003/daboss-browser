//! `WebSocket` JS class backed by [`crate::ws::WebSocketConnection`].
//!
//! Constructor opens the connection synchronously (handshake blocks);
//! incoming frames stream into the connection's `inbound` queue from
//! a reader thread and we drain that queue on each engine tick,
//! dispatching `onopen` / `onmessage` / `onclose` / `onerror`.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer,
    property::Attribute, Context, JsResult, JsValue, NativeFunction,
};

use crate::ws::{WebSocketConnection, WsInbound};

pub struct WsEntry {
    pub connection: WebSocketConnection,
    /// JS handle so we can read `onfoo` properties at dispatch time.
    pub handle: Option<boa_engine::JsObject>,
}

pub type WsRegistry = Rc<RefCell<Vec<Option<WsEntry>>>>;

thread_local! {
    pub(crate) static JS_WS_REGISTRY: RefCell<Option<WsRegistry>> = const { RefCell::new(None) };
}

const WS_IDX_KEY: &str = "__ws_idx";

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("WebSocket"),
        1,
        NativeFunction::from_fn_ptr(ws_constructor),
    )
    .ok();
}

fn ws_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(url_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let url = url_val.to_string(ctx)?.to_std_string_escaped();
    let Some(registry) = JS_WS_REGISTRY.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::null());
    };
    let conn = match WebSocketConnection::connect(&url) {
        Some(c) => c,
        None => {
            return Err(boa_engine::JsNativeError::error()
                .with_message(format!("WebSocket: failed to connect to {url}"))
                .into());
        }
    };
    let idx = {
        let mut reg = registry.borrow_mut();
        reg.push(Some(WsEntry {
            connection: conn,
            handle: None,
        }));
        reg.len() - 1
    };

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(WS_IDX_KEY),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.property(
        js_string!("url"),
        JsValue::from(js_string!(url)),
        Attribute::READONLY,
    );
    b.property(js_string!("readyState"), JsValue::from(1_u32), Attribute::all());
    for name in ["onopen", "onmessage", "onclose", "onerror"] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    b.function(
        NativeFunction::from_fn_ptr(ws_send),
        js_string!("send"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(ws_close),
        js_string!("close"),
        0,
    );
    let handle = b.build();
    if let Some(slot) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        slot.handle = Some(handle.clone());
    }
    Ok(JsValue::from(handle))
}

fn ws_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WS_IDX_KEY), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn ws_send(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = ws_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(payload) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let text = payload.to_string(ctx)?.to_std_string_escaped();
    if let Some(registry) = JS_WS_REGISTRY.with(|r| r.borrow().clone()) {
        if let Some(Some(entry)) = registry.borrow().get(idx) {
            entry.connection.send_text(&text);
        }
    }
    Ok(JsValue::undefined())
}

fn ws_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = ws_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    if let Some(registry) = JS_WS_REGISTRY.with(|r| r.borrow().clone()) {
        if let Some(Some(entry)) = registry.borrow().get(idx) {
            entry.connection.close();
        }
    }
    Ok(JsValue::undefined())
}

/// Drain inbound queues from every live WebSocket and dispatch to JS
/// handlers. Engine calls this alongside microtask + observer +
/// RTC-event draining.
pub fn drain_ws_inbound(ctx: &mut Context) {
    let Some(registry) = JS_WS_REGISTRY.with(|r| r.borrow().clone()) else {
        return;
    };
    // Snapshot to avoid holding the registry borrow over callback
    // invocation.
    let snapshots: Vec<(usize, Vec<WsInbound>)> = {
        let reg = registry.borrow();
        reg.iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|e| (i, e.connection.drain_inbound())))
            .filter(|(_, msgs)| !msgs.is_empty())
            .collect()
    };
    for (idx, messages) in snapshots {
        for msg in messages {
            dispatch(ctx, &registry, idx, msg);
        }
    }
}

fn dispatch(ctx: &mut Context, registry: &WsRegistry, idx: usize, msg: WsInbound) {
    let handle = {
        let borrow = registry.borrow();
        borrow.get(idx).and_then(|slot| slot.as_ref()).and_then(|e| e.handle.clone())
    };
    let Some(handle) = handle else { return };
    let (handler_name, value) = match msg {
        WsInbound::Open => ("onopen", JsValue::null()),
        WsInbound::Text(t) => {
            let obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("data"),
                    JsValue::from(js_string!(t)),
                    Attribute::READONLY,
                )
                .build();
            ("onmessage", JsValue::from(obj))
        }
        WsInbound::Binary(bytes) => {
            // Expose binary as a JS array of bytes (Uint8Array would
            // be more spec-correct, but harder to construct in boa).
            use boa_engine::object::builtins::JsArray;
            let arr = JsArray::new(ctx);
            for b in bytes {
                let _ = arr.push(JsValue::from(b as u32), ctx);
            }
            let obj = ObjectInitializer::new(ctx)
                .property(js_string!("data"), JsValue::from(arr), Attribute::READONLY)
                .build();
            ("onmessage", JsValue::from(obj))
        }
        WsInbound::Closed => {
            // Stamp readyState=3 so JS-side reads see CLOSED.
            let _ = handle.set(
                js_string!("readyState"),
                JsValue::from(3_u32),
                false,
                ctx,
            );
            ("onclose", JsValue::null())
        }
        WsInbound::Error(e) => {
            let obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("message"),
                    JsValue::from(js_string!(e)),
                    Attribute::READONLY,
                )
                .build();
            ("onerror", JsValue::from(obj))
        }
    };
    let Ok(handler_val) = handle.get(js_string!(handler_name), ctx) else {
        return;
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return;
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return;
    };
    let _ = handler.call(&JsValue::from(handle), &[value], ctx);
}
