//! `EventSource` JS class backed by [`crate::sse::EventSourceConnection`].
//!
//! Constructor opens the SSE stream synchronously and hands JS a
//! handle with `__es_idx` pointing at a per-thread registry. The
//! engine drains inbound events each tick and dispatches them as:
//!   * `onmessage` (and any matching event-name listeners installed
//!     via `addEventListener('foo', cb)`)
//!   * `onopen` on the first `open` event
//!   * `onerror` on transport failures
//!
//! `readyState` reflects the spec constants:
//!   * 0 — connecting
//!   * 1 — open
//!   * 2 — closed

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::sse::{EventSourceConnection, SseInbound};

pub struct EsEntry {
    pub connection: EventSourceConnection,
    /// JS handle so we can read `onfoo` properties at dispatch time.
    pub handle: Option<boa_engine::JsObject>,
    /// Per-event-name listener arrays added via addEventListener.
    pub listeners: HashMap<String, Vec<JsFunction>>,
}

pub type EsRegistry = Rc<RefCell<Vec<Option<EsEntry>>>>;

thread_local! {
    pub(crate) static JS_ES_REGISTRY: RefCell<Option<EsRegistry>> =
        const { RefCell::new(None) };
}

const ES_IDX_KEY: &str = "__es_idx";

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("EventSource"),
        1,
        NativeFunction::from_fn_ptr(es_constructor),
    )
    .ok();
}

fn es_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(url_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let url = url_val.to_string(ctx)?.to_std_string_escaped();
    let Some(registry) = JS_ES_REGISTRY.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::null());
    };
    let conn = match EventSourceConnection::connect(&url) {
        Some(c) => c,
        None => {
            return Err(boa_engine::JsNativeError::error()
                .with_message(format!("EventSource: invalid URL {url}"))
                .into());
        }
    };
    let idx = {
        let mut reg = registry.borrow_mut();
        reg.push(Some(EsEntry {
            connection: conn,
            handle: None,
            listeners: HashMap::new(),
        }));
        reg.len() - 1
    };

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(ES_IDX_KEY),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.property(
        js_string!("url"),
        JsValue::from(js_string!(url)),
        Attribute::READONLY,
    );
    b.property(
        js_string!("readyState"),
        JsValue::from(0_u32),
        Attribute::all(),
    );
    b.property(
        js_string!("withCredentials"),
        JsValue::from(false),
        Attribute::READONLY,
    );
    for name in ["onopen", "onmessage", "onerror"] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    b.function(
        NativeFunction::from_fn_ptr(es_close),
        js_string!("close"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(es_add_event_listener),
        js_string!("addEventListener"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(es_remove_event_listener),
        js_string!("removeEventListener"),
        2,
    );
    let handle = b.build();
    if let Some(slot) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        slot.handle = Some(handle.clone());
    }
    Ok(JsValue::from(handle))
}

fn es_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(ES_IDX_KEY), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn es_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = es_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(registry) = JS_ES_REGISTRY.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::undefined());
    };
    if let Some(Some(entry)) = registry.borrow().get(idx) {
        entry.connection.close();
    }
    // Update the JS-visible readyState mirror to 2 (closed).
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("readyState"),
            JsValue::from(2_u32),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn es_add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(idx) = es_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let Some(handler_val) = args.get(1) else {
        return Ok(JsValue::undefined());
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    let Some(registry) = JS_ES_REGISTRY.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::undefined());
    };
    if let Some(slot) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        slot.listeners.entry(name).or_default().push(handler);
    }
    Ok(JsValue::undefined())
}

fn es_remove_event_listener(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(idx) = es_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let Some(registry) = JS_ES_REGISTRY.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::undefined());
    };
    if let Some(slot) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        slot.listeners.remove(&name);
    }
    Ok(JsValue::undefined())
}

/// Drain inbound events from every registered EventSource and
/// dispatch them. Called alongside the WebSocket / RTC / observer
/// pumps each engine tick.
pub fn drain_sse_events(ctx: &mut Context) {
    let Some(registry) = JS_ES_REGISTRY.with(|r| r.borrow().clone()) else {
        return;
    };
    let snapshot: Vec<(usize, Vec<SseInbound>)> = {
        let reg = registry.borrow();
        reg.iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|e| (i, e.connection.drain())))
            .filter(|(_, evs)| !evs.is_empty())
            .collect()
    };
    for (idx, events) in snapshot {
        for ev in events {
            dispatch(ctx, idx, &registry, ev);
        }
    }
}

fn dispatch(ctx: &mut Context, idx: usize, registry: &EsRegistry, ev: SseInbound) {
    let handle = registry
        .borrow()
        .get(idx)
        .and_then(|s| s.as_ref())
        .and_then(|e| e.handle.clone());
    let Some(handle) = handle else { return };
    match ev {
        SseInbound::Open => {
            // Sync readyState=1.
            let _ = handle.set(js_string!("readyState"), JsValue::from(1_u32), false, ctx);
            if let Some(f) = read_handler(&handle, "onopen", ctx) {
                let event = build_simple_event(ctx, "open", None);
                let _ = f.call(&JsValue::from(handle.clone()), &[event], ctx);
            }
        }
        SseInbound::Message(msg) => {
            // Update last-event-id on the handle so JS can read it.
            if let Some(id) = &msg.id {
                let _ = handle.set(
                    js_string!("lastEventId"),
                    JsValue::from(js_string!(id.clone())),
                    false,
                    ctx,
                );
            }
            let event = build_message_event(ctx, &msg.event, &msg.data, msg.id.as_deref());
            // Default "message" event goes to `onmessage` plus any
            // `addEventListener('message', ...)`. Named events go to
            // matching listeners only.
            if msg.event == "message" {
                if let Some(f) = read_handler(&handle, "onmessage", ctx) {
                    let _ = f.call(
                        &JsValue::from(handle.clone()),
                        &[event.clone()],
                        ctx,
                    );
                }
            }
            let listeners: Vec<JsFunction> = registry
                .borrow()
                .get(idx)
                .and_then(|s| s.as_ref())
                .and_then(|e| e.listeners.get(&msg.event).cloned())
                .unwrap_or_default();
            for f in listeners {
                let _ = f.call(&JsValue::from(handle.clone()), &[event.clone()], ctx);
            }
        }
        SseInbound::Error(reason) => {
            if let Some(f) = read_handler(&handle, "onerror", ctx) {
                let event = build_simple_event(ctx, "error", Some(&reason));
                let _ = f.call(&JsValue::from(handle.clone()), &[event], ctx);
            }
        }
        SseInbound::Closed => {
            let _ = handle.set(js_string!("readyState"), JsValue::from(2_u32), false, ctx);
        }
    }
}

fn read_handler(obj: &boa_engine::JsObject, name: &str, ctx: &mut Context) -> Option<JsFunction> {
    let v = obj.get(js_string!(name), ctx).ok()?;
    let o = v.as_object()?;
    JsFunction::from_object(o.clone())
}

fn build_simple_event(ctx: &mut Context, kind: &str, reason: Option<&str>) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("type"),
        JsValue::from(js_string!(kind.to_string())),
        Attribute::READONLY,
    );
    if let Some(r) = reason {
        b.property(
            js_string!("message"),
            JsValue::from(js_string!(r.to_string())),
            Attribute::READONLY,
        );
    }
    JsValue::from(b.build())
}

fn build_message_event(
    ctx: &mut Context,
    kind: &str,
    data: &str,
    last_event_id: Option<&str>,
) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("type"),
        JsValue::from(js_string!(kind.to_string())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("data"),
        JsValue::from(js_string!(data.to_string())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("lastEventId"),
        JsValue::from(js_string!(last_event_id.unwrap_or("").to_string())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("origin"),
        JsValue::from(js_string!("")),
        Attribute::READONLY,
    );
    let _ = FunctionObjectBuilder::new; // satisfy unused import
    JsValue::from(b.build())
}
