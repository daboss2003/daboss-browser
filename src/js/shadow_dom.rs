//! Shadow DOM + Custom Elements (toy implementations).
//!
//! **Shadow DOM** — `element.attachShadow({mode})` creates a real
//! child element under the host with a synthetic tag (`__shadow_root__`)
//! and returns a handle to it. Children added via the returned
//! object render normally because the wrapper is a regular DOM
//! node. This skips real style/event encapsulation — author CSS
//! can leak in and selectors cross the boundary — but it makes the
//! API usable and the contents actually appear on screen, which is
//! the practical bar for most pages that touch the feature.
//!
//! **Custom Elements** — `customElements.define / get / whenDefined`
//! are wired with an in-memory registry. We don't run lifecycle
//! callbacks (`connectedCallback` etc.) because pages depend on
//! when those fire and our parsing/upgrade story would have to be
//! airtight. Registering and querying the registry is enough for
//! feature detection and for libraries that gate behind
//! `customElements.get(name) !== undefined`.
//!
//! Out of scope: scoped styles, slot composition / `<slot>`, real
//! event retargeting across shadow boundaries, attributeChangedCallback,
//! observedAttributes.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::{Dom, NodeId, NodeKind};

const SHADOW_TAG: &str = "__shadow_root__";

thread_local! {
    /// `customElements` registry. Maps a tag name to the user-provided
    /// constructor `JsValue`. We store `JsValue` (not `JsFunction`)
    /// so non-function values stored via `define` round-trip cleanly
    /// for `get()`.
    pub(crate) static CE_REGISTRY: RefCell<HashMap<String, JsValue>> =
        RefCell::new(HashMap::new());
}

pub fn install(ctx: &mut Context) {
    install_custom_elements(ctx);
}

fn install_custom_elements(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let define_fn = mk(ce_define);
    let get_fn = mk(ce_get);
    let when_defined_fn = mk(ce_when_defined);
    let upgrade_fn = mk(ce_upgrade);
    let registry = ObjectInitializer::new(ctx)
        .property(js_string!("define"), JsValue::from(define_fn), Attribute::READONLY)
        .property(js_string!("get"), JsValue::from(get_fn), Attribute::READONLY)
        .property(
            js_string!("whenDefined"),
            JsValue::from(when_defined_fn),
            Attribute::READONLY,
        )
        .property(
            js_string!("upgrade"),
            JsValue::from(upgrade_fn),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("customElements"),
        registry,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn ce_define(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_v) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_v.to_string(ctx)?.to_std_string_escaped();
    let ctor = args.get(1).cloned().unwrap_or(JsValue::undefined());
    CE_REGISTRY.with(|m| {
        m.borrow_mut().insert(name.clone(), ctor);
    });
    // Walk the existing DOM and upgrade any element matching this
    // tag — pages typically call `customElements.define(...)`
    // AFTER the parser has placed the elements, so without this
    // pass the constructor would never run.
    let matches = super::with_dom(|dom| collect_matching_elements(dom, dom.document(), &name))
        .unwrap_or_default();
    for node_id in matches {
        let handle = super::dom::make_element_handle(ctx, node_id);
        try_upgrade_element(ctx, &name, &handle);
    }
    Ok(JsValue::undefined())
}

fn collect_matching_elements(
    dom: &crate::dom::Dom,
    node: NodeId,
    tag: &str,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    walk_for_tag(dom, node, tag, &mut out);
    out
}

fn walk_for_tag(dom: &crate::dom::Dom, node: NodeId, tag: &str, out: &mut Vec<NodeId>) {
    if let NodeKind::Element { tag: t, .. } = &dom.node(node).kind {
        if t.eq_ignore_ascii_case(tag) {
            out.push(node);
        }
    }
    for child in dom.children(node).collect::<Vec<_>>() {
        walk_for_tag(dom, child, tag, out);
    }
}

/// Run a registered custom element's constructor + lifecycle on
/// `handle`. No-op when the tag isn't registered or the stored
/// value isn't a callable. Skips elements that have already been
/// upgraded (we tag them on the handle).
pub fn try_upgrade_element(ctx: &mut Context, tag: &str, handle: &boa_engine::JsObject) {
    let ctor_val = CE_REGISTRY.with(|m| m.borrow().get(tag).cloned());
    let Some(ctor_val) = ctor_val else { return };
    let Some(ctor_obj) = ctor_val.as_object() else { return };
    let Some(ctor) = boa_engine::object::builtins::JsFunction::from_object(ctor_obj.clone())
    else {
        return;
    };
    let already_upgraded = handle
        .get(js_string!("__ce_upgraded"), ctx)
        .ok()
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    if already_upgraded {
        return;
    }
    let _ = handle.set(
        js_string!("__ce_upgraded"),
        JsValue::from(true),
        false,
        ctx,
    );
    // Set the prototype to the constructor's `prototype` so that
    // method calls (`connectedCallback`, `attributeChangedCallback`,
    // any user methods) on the element handle resolve. Without
    // this, `el.someMethod()` would fail.
    if let Ok(proto_val) = ctor_obj.get(js_string!("prototype"), ctx) {
        if let Some(proto_obj) = proto_val.as_object() {
            let _ = handle.set_prototype(Some(proto_obj.clone()));
        }
    }
    // Invoke the constructor with `this = element handle`.
    let handle_val = JsValue::from(handle.clone());
    let _ = ctor.call(&handle_val, &[], ctx);
    // Fire connectedCallback if present. Spec semantics require
    // the element to be in a "connected" tree; for the toy we
    // fire optimistically — pages that gate on isConnected can
    // still no-op.
    if let Ok(cb_val) = handle.get(js_string!("connectedCallback"), ctx) {
        if let Some(cb_obj) = cb_val.as_object() {
            if let Some(cb) =
                boa_engine::object::builtins::JsFunction::from_object(cb_obj.clone())
            {
                let _ = cb.call(&handle_val, &[], ctx);
            }
        }
    }
}

/// Fire `attributeChangedCallback(name, oldValue, newValue, namespace?)`
/// for a custom element when an observed attribute changes. The
/// constructor's `observedAttributes` static array drives the list.
pub fn fire_attribute_changed(
    ctx: &mut Context,
    tag: &str,
    handle: &boa_engine::JsObject,
    attr_name: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
) {
    let ctor_val = CE_REGISTRY.with(|m| m.borrow().get(tag).cloned());
    let Some(ctor_val) = ctor_val else { return };
    let Some(ctor_obj) = ctor_val.as_object() else { return };
    // Read the static observedAttributes list.
    let Ok(observed_val) = ctor_obj.get(js_string!("observedAttributes"), ctx) else {
        return;
    };
    let Some(observed_obj) = observed_val.as_object() else {
        return;
    };
    let len = observed_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut watched = false;
    for i in 0..len {
        if let Ok(v) = observed_obj.get(i as u64, ctx) {
            if let Ok(s) = v.to_string(ctx) {
                if s.to_std_string_escaped().eq_ignore_ascii_case(attr_name) {
                    watched = true;
                    break;
                }
            }
        }
    }
    if !watched {
        return;
    }
    let Ok(cb_val) = handle.get(js_string!("attributeChangedCallback"), ctx) else {
        return;
    };
    let Some(cb_obj) = cb_val.as_object() else { return };
    let Some(cb) = boa_engine::object::builtins::JsFunction::from_object(cb_obj.clone()) else {
        return;
    };
    let args = [
        JsValue::from(js_string!(attr_name.to_string())),
        match old_value {
            Some(s) => JsValue::from(js_string!(s.to_string())),
            None => JsValue::null(),
        },
        match new_value {
            Some(s) => JsValue::from(js_string!(s.to_string())),
            None => JsValue::null(),
        },
        JsValue::null(),
    ];
    let _ = cb.call(&JsValue::from(handle.clone()), &args, ctx);
}

fn ce_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_v) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_v.to_string(ctx)?.to_std_string_escaped();
    let found = CE_REGISTRY.with(|m| m.borrow().get(&name).cloned());
    Ok(found.unwrap_or(JsValue::undefined()))
}

fn ce_when_defined(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Per spec returns a promise that resolves with the constructor
    // when (and only when) the registry is populated. Our toy never
    // upgrades asynchronously, so we resolve immediately for
    // already-defined names and also for unknowns (matches what
    // many feature-detection paths actually do).
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let ctor = CE_REGISTRY.with(|m| m.borrow().get(&name).cloned());
    Ok(JsPromise::resolve(ctor.unwrap_or(JsValue::undefined()), ctx).into())
}

fn ce_upgrade(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // We don't track pending upgrades; a no-op is spec-permissible.
    Ok(JsValue::undefined())
}

// =================== Shadow DOM ===================

/// Implements `element.attachShadow({ mode })`. Creates a real child
/// DOM element with a synthetic tag under the host, returns a JS
/// handle to it. Storing `__shadow_open` on the host signals
/// `shadowRoot` to expose / hide the inner node.
pub fn element_attach_shadow(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(host_id) = super::dom::read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let mode = args
        .first()
        .and_then(|v| v.as_object().cloned())
        .and_then(|o| o.get(js_string!("mode"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "open".to_string());
    let open = !mode.eq_ignore_ascii_case("closed");

    // If a shadow root already exists, return the existing one.
    let existing = super::with_dom(|dom| find_shadow_child(dom, host_id)).flatten();
    let shadow_id = match existing {
        Some(id) => id,
        None => match super::with_dom_mut(|dom| {
            let id = dom.create_element(SHADOW_TAG.to_string(), Vec::new());
            dom.append_child(host_id, id);
            id
        }) {
            Some(id) => id,
            None => return Ok(JsValue::null()),
        },
    };

    // Track whether the host's shadow root is open or closed.
    if let Some(host_obj) = this.as_object() {
        let _ = host_obj.set(
            js_string!("__shadow_open"),
            JsValue::from(open),
            false,
            ctx,
        );
    }
    Ok(JsValue::from(super::dom::make_element_handle(ctx, shadow_id)))
}

/// Implements the `shadowRoot` getter. Returns a handle to the
/// synthetic shadow child if one exists and `mode === "open"`.
pub fn element_get_shadow_root(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(host_id) = super::dom::read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let open = this
        .as_object()
        .and_then(|o| o.get(js_string!("__shadow_open"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(true);
    if !open {
        return Ok(JsValue::null());
    }
    let child = super::with_dom(|dom| find_shadow_child(dom, host_id)).flatten();
    match child {
        Some(id) => Ok(JsValue::from(super::dom::make_element_handle(ctx, id))),
        None => Ok(JsValue::null()),
    }
}

fn find_shadow_child(dom: &Dom, host_id: NodeId) -> Option<NodeId> {
    for child in dom.children(host_id) {
        if let NodeKind::Element { tag, .. } = &dom.node(child).kind {
            if tag == SHADOW_TAG {
                return Some(child);
            }
        }
    }
    None
}
