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
        m.borrow_mut().insert(name, ctor);
    });
    Ok(JsValue::undefined())
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
