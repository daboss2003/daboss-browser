//! View Transitions API — `document.startViewTransition(callback)`.
//!
//! Spec flow:
//!   1. Snapshot the current rendered page (the "old state").
//!   2. Run the user callback — typically the page mutates the DOM.
//!   3. Capture the new rendered state.
//!   4. Cross-fade between old and new over the transition duration
//!      (default 250ms, configurable via the `::view-transition-*`
//!      CSS pseudos).
//!
//! Our toy keeps the same shape but cheats on the rendering: we
//! record the "old" pixmap, run the callback (which mutates the DOM
//! synchronously), then schedule a fade-in animation that the existing
//! tick loop drives. The JS surface returns a `ViewTransition` with
//! `.updateCallbackDone` / `.ready` / `.finished` Promises and a
//! `.skipTransition()` method, all of which resolve in real time
//! against the same scheduling.

use std::cell::RefCell;

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

pub struct PendingTransition {
    pub finished_resolve: Option<JsFunction>,
    pub ready_resolve: Option<JsFunction>,
    pub update_resolve: Option<JsFunction>,
    pub remaining_ms: f32,
    pub total_ms: f32,
}

thread_local! {
    pub(crate) static ACTIVE: RefCell<Option<PendingTransition>> =
        const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let start_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(start_view_transition),
    )
    .build();
    let global = ctx.global_object();
    if let Ok(doc_val) = global.get(js_string!("document"), ctx) {
        if let Some(doc) = doc_val.as_object() {
            let _ = doc.set(
                js_string!("startViewTransition"),
                JsValue::from(start_fn),
                false,
                ctx,
            );
        }
    }
}

fn start_view_transition(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // The argument is either a callback (start the transition's
    // update step immediately) or an object `{ update: cb, types:
    // [...] }`. We honour both forms.
    let cb_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let update_cb: Option<JsFunction> = if let Some(obj) = cb_val.as_object() {
        // Either the argument is itself a function, or an options
        // object with an `update` field that's a function.
        if let Some(f) = JsFunction::from_object(obj.clone()) {
            Some(f)
        } else if let Ok(uv) = obj.get(js_string!("update"), ctx) {
            uv.as_object()
                .cloned()
                .and_then(JsFunction::from_object)
        } else {
            None
        }
    } else {
        None
    };

    // Build the three Promises we need to expose.
    let (update_p, update_rs) = JsPromise::new_pending(ctx);
    let (ready_p, ready_rs) = JsPromise::new_pending(ctx);
    let (finished_p, finished_rs) = JsPromise::new_pending(ctx);

    // Run the page's DOM-update callback immediately (synchronous
    // semantics — real browsers run it inside a rAF tick).
    if let Some(cb) = update_cb {
        match cb.call(&JsValue::undefined(), &[], ctx) {
            Ok(_) => {
                let _ = update_rs
                    .resolve
                    .call(&JsValue::undefined(), &[JsValue::undefined()], ctx);
            }
            Err(e) => {
                let _ = update_rs.reject.call(
                    &JsValue::undefined(),
                    &[JsValue::from(js_string!(e.to_string()))],
                    ctx,
                );
                let _ = ready_rs.reject.call(
                    &JsValue::undefined(),
                    &[JsValue::from(js_string!(e.to_string()))],
                    ctx,
                );
                // `finished` still resolves so handlers run.
                let _ = finished_rs
                    .resolve
                    .call(&JsValue::undefined(), &[JsValue::undefined()], ctx);
            }
        }
    } else {
        let _ = update_rs
            .resolve
            .call(&JsValue::undefined(), &[JsValue::undefined()], ctx);
    }

    // Schedule the cross-fade. We start it now and let the engine
    // tick advance it.
    ACTIVE.with(|s| {
        *s.borrow_mut() = Some(PendingTransition {
            finished_resolve: Some(finished_rs.resolve.clone()),
            ready_resolve: Some(ready_rs.resolve.clone()),
            update_resolve: Some(update_rs.resolve),
            remaining_ms: 250.0,
            total_ms: 250.0,
        });
    });

    // ready resolves at the start of the animation (next tick).
    let _ = ready_rs
        .resolve
        .call(&JsValue::undefined(), &[JsValue::undefined()], ctx);

    let realm = ctx.realm().clone();
    let skip_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(skip_transition),
    )
    .build();
    let transition = ObjectInitializer::new(ctx)
        .property(
            js_string!("updateCallbackDone"),
            JsValue::from(update_p),
            Attribute::READONLY,
        )
        .property(js_string!("ready"), JsValue::from(ready_p), Attribute::READONLY)
        .property(
            js_string!("finished"),
            JsValue::from(finished_p),
            Attribute::READONLY,
        )
        .property(
            js_string!("skipTransition"),
            JsValue::from(skip_fn),
            Attribute::READONLY,
        )
        .build();
    Ok(JsValue::from(transition))
}

fn skip_transition(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let resolve = ACTIVE.with(|s| {
        let mut slot = s.borrow_mut();
        slot.as_mut().and_then(|t| t.finished_resolve.take())
    });
    ACTIVE.with(|s| {
        *s.borrow_mut() = None;
    });
    if let Some(resolve) = resolve {
        let _ = resolve.call(&JsValue::undefined(), &[JsValue::undefined()], ctx);
    }
    Ok(JsValue::undefined())
}

/// Browser-shell hook: advance the active transition by `dt_ms` and,
/// when it completes, park its `.finished` resolver for the next
/// engine tick to fire (we don't hold a live `Context` here). Called
/// once per engine tick alongside the other animation drivers.
pub fn advance(dt_ms: f32) {
    let resolver = ACTIVE.with(|s| {
        let mut slot = s.borrow_mut();
        let take = if let Some(t) = slot.as_mut() {
            t.remaining_ms -= dt_ms;
            t.remaining_ms <= 0.0
        } else {
            false
        };
        if take {
            slot.take().and_then(|mut t| t.finished_resolve.take())
        } else {
            None
        }
    });
    if let Some(resolver) = resolver {
        PARKED_RESOLVERS.with(|s| s.borrow_mut().push(resolver));
    }
}

thread_local! {
    /// Holds the finished-resolve callbacks that `advance` peeled off
    /// while not holding a JS Context. The engine drains these once
    /// per tick.
    static PARKED_RESOLVERS: RefCell<Vec<JsFunction>> = const { RefCell::new(Vec::new()) };
}

/// Engine-tick drain. Resolves any parked `.finished` Promise.
pub fn drain_finished(ctx: &mut Context) {
    let parked: Vec<JsFunction> =
        PARKED_RESOLVERS.with(|s| std::mem::take(&mut *s.borrow_mut()));
    for f in parked {
        let _ = f.call(&JsValue::undefined(), &[JsValue::undefined()], ctx);
    }
}
