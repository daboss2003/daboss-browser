//! Page Visibility + Fullscreen + Pointer Lock APIs.
//!
//! All three share a similar shape: a small set of properties on
//! `document`, request/exit methods on element handles, and an
//! event the browser shell fires when the underlying state changes.
//! For the toy, the state lives in thread-local cells the browser
//! main loop updates from winit events (focus / unfocus,
//! fullscreen transitions, pointer grab). Pages that listen via
//! `addEventListener('visibilitychange', cb)` / `'fullscreenchange'`
//! / `'pointerlockchange'` see real signal.

use std::cell::RefCell;

use boa_engine::{
    js_string,
    object::{builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;

thread_local! {
    pub(crate) static VISIBILITY_STATE: RefCell<&'static str> =
        const { RefCell::new("visible") };
    pub(crate) static FULLSCREEN_ELEMENT: RefCell<Option<NodeId>> =
        const { RefCell::new(None) };
    pub(crate) static POINTER_LOCK_ELEMENT: RefCell<Option<NodeId>> =
        const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    install_document_accessors(ctx);
}

fn install_document_accessors(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let global = ctx.global_object();
    let Ok(doc_val) = global.get(js_string!("document"), ctx) else {
        return;
    };
    let Some(doc) = doc_val.as_object() else {
        return;
    };

    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let accessors: &[(&str, fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>)] = &[
        ("visibilityState", doc_visibility_state),
        ("hidden", doc_hidden),
        ("fullscreenElement", doc_fullscreen_element),
        ("fullscreenEnabled", doc_fullscreen_enabled),
        ("pointerLockElement", doc_pointer_lock_element),
    ];
    for (name, fp) in accessors {
        let g = getter(*fp);
        let _ = doc.define_property_or_throw(
            js_string!(name.to_string()),
            boa_engine::property::PropertyDescriptor::builder()
                .get(g)
                .enumerable(true)
                .configurable(true),
            ctx,
        );
    }
    // exitFullscreen / exitPointerLock live on the document.
    let exit_fs = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(doc_exit_fullscreen),
    )
    .build();
    let exit_pl = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(doc_exit_pointer_lock),
    )
    .build();
    let _ = doc.set(
        js_string!("exitFullscreen"),
        JsValue::from(exit_fs),
        false,
        ctx,
    );
    let _ = doc.set(
        js_string!("exitPointerLock"),
        JsValue::from(exit_pl),
        false,
        ctx,
    );
}

// ============ document accessors ============

fn doc_visibility_state(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = VISIBILITY_STATE.with(|c| *c.borrow());
    Ok(JsValue::from(js_string!(s.to_string())))
}

fn doc_hidden(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let hidden = VISIBILITY_STATE.with(|c| *c.borrow()) != "visible";
    Ok(JsValue::from(hidden))
}

fn doc_fullscreen_element(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = FULLSCREEN_ELEMENT.with(|c| *c.borrow());
    match id {
        Some(n) => Ok(JsValue::from(super::dom::make_element_handle(ctx, n))),
        None => Ok(JsValue::null()),
    }
}

fn doc_fullscreen_enabled(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

fn doc_pointer_lock_element(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = POINTER_LOCK_ELEMENT.with(|c| *c.borrow());
    match id {
        Some(n) => Ok(JsValue::from(super::dom::make_element_handle(ctx, n))),
        None => Ok(JsValue::null()),
    }
}

fn doc_exit_fullscreen(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    set_fullscreen(None, ctx);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn doc_exit_pointer_lock(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    set_pointer_lock(None, ctx);
    Ok(JsValue::undefined())
}

// ============ element handle additions ============

/// `element.requestFullscreen()` / `requestPointerLock()` are installed
/// on every element handle by `dom.rs` calling these helpers.
pub fn element_request_fullscreen(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    set_fullscreen(Some(id), ctx);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

pub fn element_request_pointer_lock(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    set_pointer_lock(Some(id), ctx);
    Ok(JsValue::undefined())
}

fn read_node_id(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(crate::js::dom::NODE_ID_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

// ============ state mutation + event dispatch ============

fn set_fullscreen(id: Option<NodeId>, ctx: &mut Context) {
    let prev = FULLSCREEN_ELEMENT.with(|c| c.replace(id));
    if prev != id {
        let target = id.or(prev);
        if let Some(target) = target {
            dispatch_doc_event(ctx, "fullscreenchange", target);
        }
    }
}

fn set_pointer_lock(id: Option<NodeId>, ctx: &mut Context) {
    let prev = POINTER_LOCK_ELEMENT.with(|c| c.replace(id));
    if prev != id {
        let target = id.or(prev);
        if let Some(target) = target {
            dispatch_doc_event(ctx, "pointerlockchange", target);
        }
    }
}

/// Browser shell entrypoint — called from `main.rs` when the window
/// loses or regains focus, fullscreen toggles, or pointer-lock
/// transitions. The corresponding `*change` event fires on the
/// document.
pub fn note_visibility(state: &'static str, ctx: &mut Context) {
    let changed = VISIBILITY_STATE.with(|c| {
        let cur = *c.borrow();
        if cur == state {
            false
        } else {
            *c.borrow_mut() = state;
            true
        }
    });
    if changed {
        // Document is the dispatch target; use NodeId(0) since
        // bubble_chain handles the document node specially.
        if let Some(doc) = crate::js::with_dom(|dom| dom.document()) {
            dispatch_doc_event(ctx, "visibilitychange", doc);
        }
    }
}

fn dispatch_doc_event(ctx: &mut Context, name: &str, target: NodeId) {
    // Build a minimal Event and invoke listeners registered on the
    // document. Bypasses the engine's dispatch_event because we don't
    // own a `Dom` ref here; instead we walk `JS_LISTENERS` directly.
    use super::engine::JS_LISTENERS;
    let listeners: Vec<boa_engine::object::builtins::JsFunction> = JS_LISTENERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| {
                rc.borrow()
                    .get(&(target, name.to_string()))
                    .cloned()
            })
            .unwrap_or_default()
    });
    if listeners.is_empty() {
        return;
    }
    let event = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!(name.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("bubbles"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .build();
    let event_val = JsValue::from(event);
    for f in listeners {
        let _ = f.call(&JsValue::undefined(), &[event_val.clone()], ctx);
    }
}
