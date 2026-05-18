//! `AbortController` + `AbortSignal` JS bindings.
//!
//! Each `new AbortController()` mints a paired Controller and Signal.
//! The signal carries:
//!   * `aborted` — bool flipped by `controller.abort(reason)`
//!   * `reason` — value passed to `abort` (undefined initially)
//!   * `onabort` event property
//!   * `addEventListener('abort', cb)` listener list
//!   * `throwIfAborted()` — throws when aborted
//!
//! Plus static helpers on `AbortSignal`:
//!   * `AbortSignal.abort(reason?)` — already-aborted
//!   * `AbortSignal.timeout(ms)` — aborts after `ms` (via setTimeout)
//!   * `AbortSignal.any([signals])` — aborts when any input does

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

const SIGNAL_ID_KEY: &str = "__signal_id";

pub struct SignalState {
    pub aborted: bool,
    pub reason: JsValue,
    /// `signal.addEventListener('abort', cb)` listeners.
    pub listeners: Vec<JsFunction>,
    /// JS handle so we can read `.onabort` at dispatch time.
    pub handle: Option<boa_engine::JsObject>,
    /// Signals to propagate aborts onto (for AbortSignal.any).
    pub follow: Vec<u32>,
}

pub type SignalRegistry = Rc<RefCell<HashMap<u32, SignalState>>>;

thread_local! {
    pub(crate) static JS_SIGNALS: RefCell<Option<SignalRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static SIGNAL_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_signal_id() -> u32 {
    SIGNAL_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    JS_SIGNALS.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    ctx.register_global_callable(
        js_string!("AbortController"),
        0,
        NativeFunction::from_fn_ptr(abort_controller_constructor),
    )
    .ok();
    install_signal_namespace(ctx);
    install_abort_by_id_global(ctx);
}

fn install_signal_namespace(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let abort_fn = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(signal_static_abort),
    )
    .build();
    let timeout_fn = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(signal_static_timeout),
    )
    .build();
    let any_fn =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(signal_static_any))
            .build();
    let ns = ObjectInitializer::new(ctx)
        .property(js_string!("abort"), JsValue::from(abort_fn), Attribute::READONLY)
        .property(js_string!("timeout"), JsValue::from(timeout_fn), Attribute::READONLY)
        .property(js_string!("any"), JsValue::from(any_fn), Attribute::READONLY)
        .build();
    let _ = ctx.register_global_property(
        js_string!("AbortSignal"),
        ns,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn abort_controller_constructor(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let signal_id = create_signal(false, JsValue::undefined());
    let signal = build_signal_object(ctx, signal_id);
    let signal_clone = signal.clone();
    let signal_for_handle = signal.as_object().cloned();
    if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
        if let Some(state) = reg.borrow_mut().get_mut(&signal_id) {
            state.handle = signal_for_handle;
        }
    }
    let controller = ObjectInitializer::new(ctx)
        .property(js_string!("signal"), signal_clone, Attribute::READONLY)
        .property(
            js_string!(SIGNAL_ID_KEY),
            JsValue::from(signal_id),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(controller_abort),
            js_string!("abort"),
            1,
        )
        .build();
    Ok(JsValue::from(controller))
}

fn create_signal(aborted: bool, reason: JsValue) -> u32 {
    let id = next_signal_id();
    if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(
            id,
            SignalState {
                aborted,
                reason,
                listeners: Vec::new(),
                handle: None,
                follow: Vec::new(),
            },
        );
    }
    id
}

fn build_signal_object(ctx: &mut Context, signal_id: u32) -> JsValue {
    let aborted_now = JS_SIGNALS
        .with(|r| {
            r.borrow()
                .as_ref()
                .map(|rc| rc.borrow().get(&signal_id).map(|s| s.aborted).unwrap_or(false))
        })
        .unwrap_or(false);
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(SIGNAL_ID_KEY),
        JsValue::from(signal_id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("aborted"),
        JsValue::from(aborted_now),
        Attribute::all(),
    );
    b.property(
        js_string!("reason"),
        JsValue::undefined(),
        Attribute::all(),
    );
    b.property(
        js_string!("onabort"),
        JsValue::null(),
        Attribute::all(),
    );
    b.function(
        NativeFunction::from_fn_ptr(signal_add_event_listener),
        js_string!("addEventListener"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(signal_remove_event_listener),
        js_string!("removeEventListener"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(signal_throw_if_aborted),
        js_string!("throwIfAborted"),
        0,
    );
    JsValue::from(b.build())
}

fn signal_id_of(val: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = val.as_object()?;
    let v = obj.get(js_string!(SIGNAL_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

/// Public helper for `fetch` etc. — returns true if the signal is
/// aborted right now.
pub fn signal_is_aborted(val: &JsValue, ctx: &mut Context) -> bool {
    let Some(id) = signal_id_of(val, ctx) else {
        return false;
    };
    JS_SIGNALS
        .with(|r| {
            r.borrow()
                .as_ref()
                .map(|rc| rc.borrow().get(&id).map(|s| s.aborted).unwrap_or(false))
        })
        .unwrap_or(false)
}

fn controller_abort(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let id = obj
        .get(js_string!(SIGNAL_ID_KEY), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok());
    let Some(id) = id else {
        return Ok(JsValue::undefined());
    };
    let reason = args.first().cloned().unwrap_or_else(default_abort_reason);
    fire_abort(ctx, id, reason);
    Ok(JsValue::undefined())
}

fn default_abort_reason() -> JsValue {
    // Spec: defaults to a DOMException("AbortError"). We use a plain
    // string for the toy.
    JsValue::from(js_string!("AbortError"))
}

fn fire_abort(ctx: &mut Context, signal_id: u32, reason: JsValue) {
    let (handle, listeners, follow_ids) = JS_SIGNALS
        .with(|r| -> Option<(Option<boa_engine::JsObject>, Vec<JsFunction>, Vec<u32>)> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let state = reg.get_mut(&signal_id)?;
            if state.aborted {
                return None;
            }
            state.aborted = true;
            state.reason = reason.clone();
            Some((
                state.handle.clone(),
                state.listeners.clone(),
                state.follow.clone(),
            ))
        })
        .unwrap_or((None, Vec::new(), Vec::new()));

    if let Some(handle) = handle.as_ref() {
        let _ = handle.set(
            js_string!("aborted"),
            JsValue::from(true),
            false,
            ctx,
        );
        let _ = handle.set(
            js_string!("reason"),
            reason.clone(),
            false,
            ctx,
        );
        // Read .onabort and invoke if a function.
        if let Ok(on_abort) = handle.get(js_string!("onabort"), ctx) {
            if let Some(o) = on_abort.as_object() {
                if let Some(f) = JsFunction::from_object(o.clone()) {
                    let event = build_abort_event(ctx, &reason);
                    let _ = f.call(&JsValue::from(handle.clone()), &[event], ctx);
                }
            }
        }
        // Fire addEventListener-registered handlers.
        for f in listeners {
            let event = build_abort_event(ctx, &reason);
            let _ = f.call(&JsValue::from(handle.clone()), &[event], ctx);
        }
    }
    // Cascade onto signals registered via AbortSignal.any.
    for sub_id in follow_ids {
        fire_abort(ctx, sub_id, reason.clone());
    }
}

fn build_abort_event(ctx: &mut Context, _reason: &JsValue) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!("abort")),
            Attribute::READONLY,
        )
        .build()
        .into()
}

fn signal_add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = signal_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    if name != "abort" {
        return Ok(JsValue::undefined());
    }
    let Some(handler_val) = args.get(1) else {
        return Ok(JsValue::undefined());
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.listeners.push(handler);
        }
    }
    Ok(JsValue::undefined())
}

fn signal_remove_event_listener(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // The toy uses function-identity comparison only via reference,
    // which boa's JsFunction doesn't expose. Clear the list when a
    // remove targets the abort event.
    if let Some(id) = signal_id_of(this, ctx) {
        if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
            if let Some(state) = reg.borrow_mut().get_mut(&id) {
                state.listeners.clear();
            }
        }
    }
    Ok(JsValue::undefined())
}

fn signal_throw_if_aborted(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = signal_id_of(this, ctx) {
        let aborted = JS_SIGNALS
            .with(|r| {
                r.borrow()
                    .as_ref()
                    .map(|rc| rc.borrow().get(&id).map(|s| s.aborted).unwrap_or(false))
            })
            .unwrap_or(false);
        if aborted {
            let reason = JS_SIGNALS
                .with(|r| {
                    r.borrow()
                        .as_ref()
                        .and_then(|rc| rc.borrow().get(&id).map(|s| s.reason.clone()))
                })
                .unwrap_or(JsValue::undefined());
            return Err(boa_engine::JsError::from_opaque(reason));
        }
    }
    Ok(JsValue::undefined())
}

fn signal_static_abort(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let reason = args.first().cloned().unwrap_or_else(default_abort_reason);
    let id = create_signal(true, reason.clone());
    let signal = build_signal_object(ctx, id);
    if let Some(obj) = signal.as_object() {
        let _ = obj.set(js_string!("aborted"), JsValue::from(true), false, ctx);
        let _ = obj.set(js_string!("reason"), reason, false, ctx);
        if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
            if let Some(state) = reg.borrow_mut().get_mut(&id) {
                state.handle = Some(obj.clone());
            }
        }
    }
    Ok(signal)
}

fn signal_static_timeout(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = args
        .first()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0)
        .max(0.0);
    let id = create_signal(false, JsValue::undefined());
    let signal = build_signal_object(ctx, id);
    let handle = signal.as_object().cloned();
    if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.handle = handle;
        }
    }
    // Schedule the abort via setTimeout. Build a tiny IIFE that calls
    // our internal abort-by-id global so the timer can fire it.
    install_abort_by_id_global(ctx);
    let code = format!(
        "setTimeout(function() {{ __daboss_abort_signal__({id}, 'TimeoutError'); }}, {ms});",
    );
    let _ = ctx.eval(boa_engine::Source::from_bytes(code.as_bytes()));
    Ok(signal)
}

fn install_abort_by_id_global(ctx: &mut Context) {
    // Idempotent — installing twice is fine; boa silently overwrites.
    ctx.register_global_callable(
        js_string!("__daboss_abort_signal__"),
        2,
        NativeFunction::from_fn_ptr(abort_signal_by_id),
    )
    .ok();
}

fn abort_signal_by_id(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args
        .first()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let reason = args.get(1).cloned().unwrap_or_else(default_abort_reason);
    fire_abort(ctx, id, reason);
    Ok(JsValue::undefined())
}

fn signal_static_any(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let combined_id = create_signal(false, JsValue::undefined());
    let signal = build_signal_object(ctx, combined_id);
    if let Some(obj) = signal.as_object() {
        if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
            if let Some(state) = reg.borrow_mut().get_mut(&combined_id) {
                state.handle = Some(obj.clone());
            }
        }
    }
    let Some(arr) = args.first().and_then(|v| v.as_object()) else {
        return Ok(signal);
    };
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    for i in 0..len {
        let Ok(item) = arr.get(i, ctx) else { continue };
        let Some(sid) = signal_id_of(&item, ctx) else { continue };
        let (already_aborted, reason) = JS_SIGNALS
            .with(|r| {
                r.borrow().as_ref().and_then(|rc| {
                    rc.borrow()
                        .get(&sid)
                        .map(|s| (s.aborted, s.reason.clone()))
                })
            })
            .unwrap_or((false, JsValue::undefined()));
        if already_aborted {
            fire_abort(ctx, combined_id, reason);
            return Ok(signal);
        }
        // Register so the parent aborts when this child does.
        if let Some(reg) = JS_SIGNALS.with(|r| r.borrow().clone()) {
            if let Some(state) = reg.borrow_mut().get_mut(&sid) {
                state.follow.push(combined_id);
            }
        }
    }
    Ok(signal)
}
