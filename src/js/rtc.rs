//! `RTCPeerConnection` and `RTCDataChannel` JS bindings backed by
//! [`crate::webrtc::PeerConnection`].
//!
//! Each `new RTCPeerConnection()` call creates a Rust PeerConnection,
//! stashes it in a per-thread registry, and hands JS a handle object
//! with a `__pc_idx` property. Methods (`createOffer`,
//! `setRemoteDescription`, `createDataChannel`, etc.) look up the
//! underlying object by that index.
//!
//! Event delivery: each engine tick (after script execution / event
//! dispatch / timer / rAF), we drain queued `PcEvent`s and invoke the
//! corresponding `onicecandidate` / `ondatachannel` / channel
//! `onmessage` handler.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer,
    property::Attribute, Context, JsResult, JsValue, NativeFunction,
};

use crate::webrtc::{data_channel_send, PcEvent, PeerConnection};
use webrtc::data_channel::RTCDataChannel;

pub struct RtcEntry {
    pub pc: PeerConnection,
    /// JS handle objects that registered handlers, keyed by event
    /// type. We re-resolve `onfoo` properties at dispatch time so
    /// late-bound assignments work.
    pub handle: Option<boa_engine::JsObject>,
    /// Local data channels, keyed by label. `dc.send()` calls go
    /// through `webrtc::data_channel_send`.
    pub channels: std::collections::HashMap<String, Arc<RTCDataChannel>>,
    /// Per-channel JS handles so `channel.onmessage` / `onopen` can
    /// be wired by label.
    pub channel_handles: std::collections::HashMap<String, boa_engine::JsObject>,
}

pub type RtcRegistry = Rc<RefCell<Vec<Option<RtcEntry>>>>;

thread_local! {
    pub(crate) static JS_RTC_REGISTRY: RefCell<Option<RtcRegistry>> =
        const { RefCell::new(None) };

    pub(crate) static JS_RTC_RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> =
        const { RefCell::new(None) };
}

const PC_IDX_KEY: &str = "__pc_idx";

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("RTCPeerConnection"),
        1,
        NativeFunction::from_fn_ptr(pc_constructor),
    )
    .ok();
}

fn pc_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let runtime = match JS_RTC_RUNTIME.with(|r| r.borrow().clone()) {
        Some(rt) => rt,
        None => return Ok(JsValue::null()),
    };
    let registry = match JS_RTC_REGISTRY.with(|r| r.borrow().clone()) {
        Some(reg) => reg,
        None => return Ok(JsValue::null()),
    };

    // Parse optional configuration `{ iceServers: [{ urls: "..." }, ...] }`.
    let mut ice_urls: Vec<String> = Vec::new();
    if let Some(cfg) = args.first() {
        if let Some(obj) = cfg.as_object() {
            if let Ok(servers) = obj.get(js_string!("iceServers"), ctx) {
                if let Some(arr_obj) = servers.as_object() {
                    if let Ok(arr) =
                        boa_engine::object::builtins::JsArray::from_object(arr_obj.clone())
                    {
                        let len = arr.length(ctx).unwrap_or(0);
                        for i in 0..len {
                            if let Ok(item) = arr.get(i, ctx) {
                                if let Some(item_obj) = item.as_object() {
                                    if let Ok(urls) = item_obj.get(js_string!("urls"), ctx) {
                                        if urls.is_string() {
                                            ice_urls.push(
                                                urls.to_string(ctx)?.to_std_string_escaped(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let pc = match PeerConnection::new(runtime, ice_urls) {
        Some(p) => p,
        None => {
            return Err(boa_engine::JsNativeError::error()
                .with_message("RTCPeerConnection: failed to construct")
                .into());
        }
    };
    let idx = {
        let mut reg = registry.borrow_mut();
        reg.push(Some(RtcEntry {
            pc,
            handle: None,
            channels: std::collections::HashMap::new(),
            channel_handles: std::collections::HashMap::new(),
        }));
        reg.len() - 1
    };

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(PC_IDX_KEY),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    // Event-handler properties (writable so JS can assign).
    for name in [
        "onicecandidate",
        "ondatachannel",
        "onconnectionstatechange",
        "oniceconnectionstatechange",
    ] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    b.function(
        NativeFunction::from_fn_ptr(pc_create_offer),
        js_string!("createOffer"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_create_answer),
        js_string!("createAnswer"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_set_local_description),
        js_string!("setLocalDescription"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_set_remote_description),
        js_string!("setRemoteDescription"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_add_ice_candidate),
        js_string!("addIceCandidate"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_create_data_channel),
        js_string!("createDataChannel"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(pc_close),
        js_string!("close"),
        0,
    );
    let handle = b.build();
    // Store the JS handle so we can call `onicecandidate` etc. later.
    if let Some(entry) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        entry.handle = Some(handle.clone());
    }
    Ok(JsValue::from(handle))
}

fn pc_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(PC_IDX_KEY), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn with_entry<R>(this: &JsValue, ctx: &mut Context, f: impl FnOnce(&mut RtcEntry) -> R) -> Option<R> {
    let idx = pc_idx(this, ctx)?;
    let reg = JS_RTC_REGISTRY.with(|r| r.borrow().clone())?;
    let mut borrow = reg.borrow_mut();
    let slot = borrow.get_mut(idx)?;
    slot.as_mut().map(f)
}

fn pc_create_offer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let sdp = with_entry(this, ctx, |e| e.pc.create_offer()).flatten();
    use boa_engine::object::builtins::JsPromise;
    let value = match sdp {
        Some(sdp) => {
            let obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(js_string!("offer")),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("sdp"),
                    JsValue::from(js_string!(sdp)),
                    Attribute::READONLY,
                )
                .build();
            JsValue::from(obj)
        }
        None => JsValue::null(),
    };
    Ok(JsPromise::resolve(value, ctx).into())
}

fn pc_create_answer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let sdp = with_entry(this, ctx, |e| e.pc.create_answer()).flatten();
    use boa_engine::object::builtins::JsPromise;
    let value = match sdp {
        Some(sdp) => {
            let obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(js_string!("answer")),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("sdp"),
                    JsValue::from(js_string!(sdp)),
                    Attribute::READONLY,
                )
                .build();
            JsValue::from(obj)
        }
        None => JsValue::null(),
    };
    Ok(JsPromise::resolve(value, ctx).into())
}

fn pc_set_local_description(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // createOffer / createAnswer already set the local description via
    // the Rust wrapper. This method is a no-op for the toy.
    use boa_engine::object::builtins::JsPromise;
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn pc_set_remote_description(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let Some(desc) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let (ty, sdp) = match desc.as_object() {
        Some(obj) => {
            let ty = obj
                .get(js_string!("type"), ctx)
                .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            let sdp = obj
                .get(js_string!("sdp"), ctx)
                .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            (ty, sdp)
        }
        None => return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into()),
    };
    with_entry(this, ctx, |e| {
        e.pc.set_remote_description(&ty, &sdp);
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn pc_add_ice_candidate(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let Some(cand) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let candidate_str = match cand.as_object() {
        Some(obj) => obj
            .get(js_string!("candidate"), ctx)
            .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => cand.to_string(ctx)?.to_std_string_escaped(),
    };
    with_entry(this, ctx, |e| {
        e.pc.add_ice_candidate(&candidate_str);
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn pc_create_data_channel(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(label_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let label = label_val.to_string(ctx)?.to_std_string_escaped();
    let dc_opt = with_entry(this, ctx, |e| {
        let dc = e.pc.create_data_channel(&label)?;
        e.channels.insert(label.clone(), dc.clone());
        Some(dc)
    })
    .flatten();
    let Some(_dc) = dc_opt else {
        return Ok(JsValue::null());
    };
    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(f),
        )
        .build()
    };
    let _ = getter;
    let parent_idx = pc_idx(this, ctx).unwrap_or(0) as u32;
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(PC_IDX_KEY),
        JsValue::from(parent_idx),
        Attribute::READONLY,
    );
    b.property(
        js_string!("label"),
        JsValue::from(js_string!(label.clone())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("onopen"),
        JsValue::null(),
        Attribute::all(),
    );
    b.property(
        js_string!("onmessage"),
        JsValue::null(),
        Attribute::all(),
    );
    b.property(
        js_string!("onerror"),
        JsValue::null(),
        Attribute::all(),
    );
    b.function(
        NativeFunction::from_fn_ptr(dc_send),
        js_string!("send"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(dc_close),
        js_string!("close"),
        0,
    );
    let handle = b.build();
    with_entry(this, ctx, |e| {
        e.channel_handles.insert(label.clone(), handle.clone());
    });
    Ok(JsValue::from(handle))
}

fn dc_send(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let runtime = JS_RTC_RUNTIME.with(|r| r.borrow().clone());
    let registry = JS_RTC_REGISTRY.with(|r| r.borrow().clone());
    let (Some(runtime), Some(registry)) = (runtime, registry) else {
        return Ok(JsValue::undefined());
    };
    let obj = match this.as_object() {
        Some(o) => o,
        None => return Ok(JsValue::undefined()),
    };
    let idx = obj
        .get(js_string!(PC_IDX_KEY), ctx)?
        .to_u32(ctx)? as usize;
    let label = obj
        .get(js_string!("label"), ctx)?
        .to_string(ctx)?
        .to_std_string_escaped();
    let Some(payload_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let payload = payload_val.to_string(ctx)?.to_std_string_escaped();
    let dc = {
        let borrow = registry.borrow();
        borrow
            .get(idx)
            .and_then(|s| s.as_ref())
            .and_then(|e| e.channels.get(&label).cloned())
    };
    if let Some(dc) = dc {
        data_channel_send(&runtime, &dc, &payload);
    }
    Ok(JsValue::undefined())
}

fn dc_close(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn pc_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    with_entry(this, ctx, |e| e.pc.close());
    Ok(JsValue::undefined())
}

/// Drain queued PeerConnection events and dispatch them to JS
/// handlers. Called by the engine alongside microtasks / timers.
pub fn drain_rtc_events(ctx: &mut Context) {
    let Some(registry) = JS_RTC_REGISTRY.with(|r| r.borrow().clone()) else {
        return;
    };
    // Take a snapshot to avoid holding the registry borrow across
    // callback invocations.
    let entries_with_events: Vec<(usize, Vec<PcEvent>)> = {
        let reg = registry.borrow();
        reg.iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|e| (i, e.pc.drain_events())))
            .filter(|(_, evs)| !evs.is_empty())
            .collect()
    };
    for (idx, events) in entries_with_events {
        for ev in events {
            dispatch_event(ctx, idx, &registry, ev);
        }
    }
}

fn dispatch_event(ctx: &mut Context, idx: usize, registry: &RtcRegistry, ev: PcEvent) {
    // Look up the JS handle to read the handler properties from.
    let (handle, channel_handle) = {
        let borrow = registry.borrow();
        let entry = match borrow.get(idx).and_then(|s| s.as_ref()) {
            Some(e) => e,
            None => return,
        };
        let pc_handle = entry.handle.clone();
        let chan_handle = match &ev {
            PcEvent::DataMessage(label, _)
            | PcEvent::DataChannelOpen(label) => entry.channel_handles.get(label).cloned(),
            _ => None,
        };
        (pc_handle, chan_handle)
    };
    match ev {
        PcEvent::IceCandidate(payload) => {
            let Some(handle) = handle else { return };
            let Some(handler) = read_function(&handle, "onicecandidate", ctx) else {
                return;
            };
            // Build the inner candidate object first so the outer
            // ObjectInitializer doesn't double-borrow ctx.
            let candidate_val = match payload {
                Some(s) => {
                    let inner = ObjectInitializer::new(ctx)
                        .property(
                            js_string!("candidate"),
                            JsValue::from(js_string!(s)),
                            Attribute::READONLY,
                        )
                        .build();
                    JsValue::from(inner)
                }
                None => JsValue::null(),
            };
            let event_obj = ObjectInitializer::new(ctx)
                .property(js_string!("candidate"), candidate_val, Attribute::READONLY)
                .build();
            let _ = handler.call(
                &JsValue::from(handle),
                &[JsValue::from(event_obj)],
                ctx,
            );
        }
        PcEvent::ConnectionState(state) => {
            let Some(handle) = handle else { return };
            // Stamp readable state on the handle so JS-side reads work.
            let _ = handle.set(
                js_string!("connectionState"),
                JsValue::from(js_string!(state.clone())),
                false,
                ctx,
            );
            if let Some(handler) = read_function(&handle, "onconnectionstatechange", ctx) {
                let _ = handler.call(&JsValue::from(handle), &[], ctx);
            }
        }
        PcEvent::DataChannel(label) => {
            let Some(handle) = handle else { return };
            let Some(handler) = read_function(&handle, "ondatachannel", ctx) else {
                return;
            };
            let channel_obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("label"),
                    JsValue::from(js_string!(label)),
                    Attribute::READONLY,
                )
                .build();
            let event_obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("channel"),
                    JsValue::from(channel_obj),
                    Attribute::READONLY,
                )
                .build();
            let _ = handler.call(
                &JsValue::from(handle),
                &[JsValue::from(event_obj)],
                ctx,
            );
        }
        PcEvent::DataChannelOpen(_label) => {
            let Some(handle) = channel_handle else { return };
            if let Some(handler) = read_function(&handle, "onopen", ctx) {
                let _ = handler.call(&JsValue::from(handle), &[], ctx);
            }
        }
        PcEvent::DataMessage(_label, payload) => {
            let Some(handle) = channel_handle else { return };
            if let Some(handler) = read_function(&handle, "onmessage", ctx) {
                let event_obj = ObjectInitializer::new(ctx)
                    .property(
                        js_string!("data"),
                        JsValue::from(js_string!(payload)),
                        Attribute::READONLY,
                    )
                    .build();
                let _ = handler.call(
                    &JsValue::from(handle),
                    &[JsValue::from(event_obj)],
                    ctx,
                );
            }
        }
    }
}

fn read_function(obj: &boa_engine::JsObject, name: &str, ctx: &mut Context) -> Option<JsFunction> {
    let v = obj.get(js_string!(name), ctx).ok()?;
    let o = v.as_object()?;
    JsFunction::from_object(o.clone())
}
