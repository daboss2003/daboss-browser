//! DOM bindings exposed to inline scripts. Element handles are plain JS
//! objects carrying a private `__nodeId` (the arena index). All properties
//! and methods read the live `Dom` via [`super::with_dom`] /
//! [`super::with_dom_mut`] so mutations made by one script are visible to
//! the next.
//!
//! Supported surface (Phase 7b):
//!
//! * `document`
//!     - `documentElement`, `body`, `title` (read-only properties)
//!     - `getElementById(id)`
//!     - `querySelector(sel)`, `querySelectorAll(sel)`
//! * `Element`
//!     - `tagName`, `nodeName` (getters)
//!     - `id`, `className` (getter + setter — proxied to the
//!       underlying `id` / `class` attributes)
//!     - `textContent` (getter concatenates descendants; setter replaces
//!       all children with a single text node)
//!     - `getAttribute(name)`, `hasAttribute(name)`,
//!       `setAttribute(name, value)`, `removeAttribute(name)`
//!     - `parentElement` (getter)
//!     - `children` (returns an array of element-only child handles)
//!
//! Mutations happen *before* style/layout, so the cascade and box tree see
//! the post-script DOM. Mutation from event handlers / timers (Phase 7c+)
//! will need to trigger re-cascade and re-layout — out of scope here.
//!
//! Selector parsing reuses the CSS selector parser from
//! [`crate::css::parse_selector_list_str`], so it handles compounds and
//! combinators (`>`, descendant, `+`, `~`).

use boa_engine::{
    js_string,
    object::{builtins::JsArray, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction,
};

use super::{with_dom, with_dom_mut};
use crate::css::{parse_selector_list_str, selector_matches, Selector};
use crate::dom::{Dom, NodeId, NodeKind};

pub(crate) const NODE_ID_KEY: &str = "__nodeId";

pub fn install(ctx: &mut Context) {
    let document = build_document(ctx);
    ctx.register_global_property(js_string!("document"), document, Attribute::all())
        .ok();
}

fn build_document(ctx: &mut Context) -> JsObject {
    let (root_id, body_id, title) = with_dom(|dom| {
        let root = find_root_element(dom);
        let body = root.and_then(|r| find_descendant_by_tag(dom, r, "body"));
        let title = read_title(dom);
        (root, body, title)
    })
    .unwrap_or((None, None, String::new()));

    let root_value = match root_id {
        Some(id) => JsValue::from(make_element_handle(ctx, id)),
        None => JsValue::null(),
    };
    let body_value = match body_id {
        Some(id) => JsValue::from(make_element_handle(ctx, id)),
        None => JsValue::null(),
    };

    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };
    let cookie_get = getter(document_get_cookie);
    let cookie_set = getter(document_set_cookie);

    let mut b = ObjectInitializer::new(ctx);
    b.function(
        NativeFunction::from_fn_ptr(get_element_by_id),
        js_string!("getElementById"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(query_selector),
        js_string!("querySelector"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(query_selector_all),
        js_string!("querySelectorAll"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(document_create_element),
        js_string!("createElement"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(document_create_text_node),
        js_string!("createTextNode"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(document_get_elements_by_tag_name),
        js_string!("getElementsByTagName"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(document_get_elements_by_class_name),
        js_string!("getElementsByClassName"),
        1,
    );
    b.property(
        js_string!("documentElement"),
        root_value,
        Attribute::READONLY,
    );
    b.property(js_string!("body"), body_value, Attribute::READONLY);
    b.property(js_string!("title"), js_string!(title), Attribute::READONLY);
    b.accessor(
        js_string!("cookie"),
        Some(cookie_get),
        Some(cookie_set),
        Attribute::ENUMERABLE,
    );
    b.build()
}

pub(crate) fn make_element_handle(ctx: &mut Context, id: NodeId) -> JsObject {
    // Clone the realm once so we can build many `JsFunction`s (one per
    // getter / setter) without re-borrowing the context each time.
    let realm = ctx.realm().clone();

    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };

    let tag_name_get = getter(element_get_tag_name);
    let node_name_get = getter(element_get_tag_name);
    let id_get = getter(element_get_id);
    let id_set = getter(element_set_id);
    let class_get = getter(element_get_class_name);
    let class_set = getter(element_set_class_name);
    let text_get = getter(element_get_text_content);
    let text_set = getter(element_set_text_content);
    let parent_get = getter(element_get_parent_element);
    let children_get = getter(element_get_children);

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(NODE_ID_KEY),
        JsValue::from(id.index() as u32),
        Attribute::READONLY,
    );
    init.accessor(
        js_string!("tagName"),
        Some(tag_name_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("nodeName"),
        Some(node_name_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("id"),
        Some(id_get),
        Some(id_set),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("className"),
        Some(class_get),
        Some(class_set),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("textContent"),
        Some(text_get),
        Some(text_set),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("parentElement"),
        Some(parent_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("children"),
        Some(children_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_get_attribute),
        js_string!("getAttribute"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_has_attribute),
        js_string!("hasAttribute"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_set_attribute),
        js_string!("setAttribute"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_remove_attribute),
        js_string!("removeAttribute"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_add_event_listener),
        js_string!("addEventListener"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_get_bounding_client_rect),
        js_string!("getBoundingClientRect"),
        0,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_get_context),
        js_string!("getContext"),
        1,
    );
    // <audio> playback methods. Safe to install on every element —
    // the implementations no-op when the target isn't a registered
    // audio element.
    init.function(
        NativeFunction::from_fn_ptr(audio_play),
        js_string!("play"),
        0,
    );
    init.function(
        NativeFunction::from_fn_ptr(audio_pause),
        js_string!("pause"),
        0,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_append_child),
        js_string!("appendChild"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_remove_child),
        js_string!("removeChild"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_insert_before),
        js_string!("insertBefore"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_replace_child),
        js_string!("replaceChild"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_clone_node),
        js_string!("cloneNode"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_matches),
        js_string!("matches"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_closest),
        js_string!("closest"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_query_selector),
        js_string!("querySelector"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_query_selector_all),
        js_string!("querySelectorAll"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_animate),
        js_string!("animate"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(element_get_animations),
        js_string!("getAnimations"),
        0,
    );

    // Live-ish helpers that build a fresh object on every access. Cheap
    // enough for a toy; real browsers cache.
    let inner_html_get = getter(element_get_inner_html);
    let inner_html_set = getter(element_set_inner_html);
    let outer_html_get = getter(element_get_outer_html);
    let class_list_get = getter(element_get_class_list);
    let style_get = getter(element_get_style);
    let dataset_get = getter(element_get_dataset);
    let src_object_get = getter(element_get_src_object);
    let src_object_set = getter(element_set_src_object);

    init.accessor(
        js_string!("innerHTML"),
        Some(inner_html_get),
        Some(inner_html_set),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("outerHTML"),
        Some(outer_html_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("classList"),
        Some(class_list_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("style"),
        Some(style_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("dataset"),
        Some(dataset_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("srcObject"),
        Some(src_object_get),
        Some(src_object_set),
        Attribute::ENUMERABLE,
    );

    // contenteditable: a real getter/setter that maps to the
    // `contenteditable` attribute, plus the read-only
    // `isContentEditable` boolean which walks ancestors to determine
    // effective editability. Reuses the `realm` cloned at the top of
    // this function so we don't double-borrow ctx.
    let ce_get = getter(element_get_content_editable);
    let ce_set = getter(element_set_content_editable);
    let is_ce_get = getter(element_get_is_content_editable);
    init.accessor(
        js_string!("contentEditable"),
        Some(ce_get),
        Some(ce_set),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("isContentEditable"),
        Some(is_ce_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.build()
}

fn element_get_content_editable(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let val = read_attr(this, ctx, "contenteditable").unwrap_or_else(|| "inherit".to_string());
    let normalised = match val.to_ascii_lowercase().as_str() {
        "true" | "" => "true",
        "false" => "false",
        "plaintext-only" => "plaintext-only",
        _ => "inherit",
    };
    Ok(JsValue::from(js_string!(normalised.to_string())))
}

fn element_set_content_editable(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let val = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let normalised = match val.to_ascii_lowercase().as_str() {
        "true" | "" => "true",
        "false" => "false",
        "plaintext-only" => "plaintext-only",
        _ => "inherit",
    };
    super::with_dom_mut(|dom| dom.set_attribute(id, "contenteditable", normalised.to_string()));
    Ok(JsValue::undefined())
}

fn element_get_is_content_editable(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let editable = super::with_dom(|dom| is_effectively_editable(dom, id)).unwrap_or(false);
    Ok(JsValue::from(editable))
}

fn is_effectively_editable(dom: &Dom, mut id: NodeId) -> bool {
    loop {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            if let Some((_, v)) = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("contenteditable"))
            {
                match v.to_ascii_lowercase().as_str() {
                    "true" | "" | "plaintext-only" => return true,
                    "false" => return false,
                    _ => {}
                }
            }
        }
        let Some(parent) = dom.node(id).parent else {
            return false;
        };
        id = parent;
    }
}

/// `element.animate(keyframes, options)` — delegate to the Web
/// Animations registry in `animations.rs`. Returns an `Animation`.
fn element_animate(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let keyframes = args.first().cloned().unwrap_or(JsValue::undefined());
    let options = args.get(1).cloned().unwrap_or(JsValue::undefined());
    Ok(super::animations::element_animate(id, &keyframes, &options, ctx))
}

/// `element.getAnimations()` — returns Animations whose target is
/// this element. We surface the global registry for the toy.
fn element_get_animations(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(super::animations::document_get_animations(ctx))
}

/// `<video>.srcObject = stream` — read the MediaStream's
/// `__capture_idx` and remember which capture this element renders.
fn element_set_src_object(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(stream_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    // `null` clears any binding.
    if stream_val.is_null() || stream_val.is_undefined() {
        super::media::JS_CAPTURE_BINDINGS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                rc.borrow_mut().remove(&id);
            }
        });
        return Ok(JsValue::undefined());
    }
    let Some(obj) = stream_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let idx_val = obj.get(js_string!("__capture_idx"), ctx).ok();
    let Some(idx_value) = idx_val else {
        return Ok(JsValue::undefined());
    };
    let idx = match idx_value.to_u32(ctx) {
        Ok(n) => n as usize,
        Err(_) => return Ok(JsValue::undefined()),
    };
    super::media::JS_CAPTURE_BINDINGS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().insert(id, idx);
        }
    });
    Ok(JsValue::undefined())
}

/// `<video>.srcObject` getter — for now reflects whether a binding
/// exists. Real spec returns the stream object; we synthesise a
/// minimal handle so JS can compare against `null`.
fn element_get_src_object(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let bound = super::media::JS_CAPTURE_BINDINGS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&id).copied())
    });
    match bound {
        Some(idx) => {
            let obj = ObjectInitializer::new(ctx)
                .property(
                    js_string!("__capture_idx"),
                    JsValue::from(idx as u32),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("active"),
                    JsValue::from(true),
                    Attribute::READONLY,
                )
                .build();
            Ok(JsValue::from(obj))
        }
        None => Ok(JsValue::null()),
    }
}

// ---------- DOM tree helpers (Rust-side) ----------

fn find_root_element(dom: &Dom) -> Option<NodeId> {
    for c in dom.children(dom.document()) {
        if let NodeKind::Element { .. } = &dom.node(c).kind {
            return Some(c);
        }
    }
    None
}

fn find_descendant_by_tag(dom: &Dom, root: NodeId, tag_name: &str) -> Option<NodeId> {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if let NodeKind::Element { tag, .. } = &dom.node(n).kind {
            if tag == tag_name {
                return Some(n);
            }
        }
        let mut kids: Vec<NodeId> = dom.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    None
}

fn read_title(dom: &Dom) -> String {
    if let Some(root) = find_root_element(dom) {
        if let Some(title_el) = find_descendant_by_tag(dom, root, "title") {
            return text_content_of(dom, title_el);
        }
    }
    String::new()
}

fn text_content_of(dom: &Dom, node: NodeId) -> String {
    let mut buf = String::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match &dom.node(n).kind {
            NodeKind::Text(t) => buf.push_str(t),
            NodeKind::Element { .. } | NodeKind::Document => {
                let mut kids: Vec<NodeId> = dom.children(n).collect();
                kids.reverse();
                stack.extend(kids);
            }
            _ => {}
        }
    }
    buf
}

/// Visible to other JS-subsystem modules under test (e.g.
/// `engine.rs` integration tests) so they can locate a node by id
/// without duplicating the walk. Not exposed beyond the crate.
#[cfg(test)]
pub(crate) fn find_for_test_by_id(dom: &Dom, wanted: &str) -> Option<NodeId> {
    find_by_id(dom, wanted)
}

fn find_by_id(dom: &Dom, wanted: &str) -> Option<NodeId> {
    let root = find_root_element(dom)?;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if let NodeKind::Element { attrs, .. } = &dom.node(n).kind {
            if attrs.iter().any(|(k, v)| k == "id" && v == wanted) {
                return Some(n);
            }
        }
        let mut kids: Vec<NodeId> = dom.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    None
}

/// Tree-walk in document order returning every element matching any of
/// `selectors`. If `first_only` is true, stops after the first hit.
fn collect_matching(dom: &Dom, selectors: &[Selector], first_only: bool) -> Vec<NodeId> {
    let mut out = Vec::new();
    let Some(root) = find_root_element(dom) else {
        return out;
    };
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if matches!(dom.node(n).kind, NodeKind::Element { .. })
            && selectors.iter().any(|s| selector_matches(s, dom, n))
        {
            out.push(n);
            if first_only {
                return out;
            }
        }
        let mut kids: Vec<NodeId> = dom.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

// ---------- JS-callable shims: document.* ----------

fn get_element_by_id(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = args.first() else {
        return Ok(JsValue::null());
    };
    let id_str = name.to_string(ctx)?.to_std_string_escaped();
    let found = with_dom(|dom| find_by_id(dom, &id_str)).flatten();
    match found {
        Some(node_id) => Ok(JsValue::from(make_element_handle(ctx, node_id))),
        None => Ok(JsValue::null()),
    }
}

fn query_selector(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsValue::null());
    };
    let sel_str = arg.to_string(ctx)?.to_std_string_escaped();
    let Some(selectors) = parse_selector_list_str(&sel_str) else {
        return Ok(JsValue::null());
    };
    let hit =
        with_dom(|dom| collect_matching(dom, &selectors, true).into_iter().next()).flatten();
    match hit {
        Some(id) => Ok(JsValue::from(make_element_handle(ctx, id))),
        None => Ok(JsValue::null()),
    }
}

fn query_selector_all(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsArray::new(ctx).into());
    };
    let sel_str = arg.to_string(ctx)?.to_std_string_escaped();
    let Some(selectors) = parse_selector_list_str(&sel_str) else {
        return Ok(JsArray::new(ctx).into());
    };
    let hits = with_dom(|dom| collect_matching(dom, &selectors, false)).unwrap_or_default();

    let arr = JsArray::new(ctx);
    for id in hits {
        let handle = make_element_handle(ctx, id);
        arr.push(JsValue::from(handle), ctx)?;
    }
    Ok(arr.into())
}

// ---------- JS-callable shims: Element.* ----------

fn read_self_node_id(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let val = obj.get(js_string!(NODE_ID_KEY), ctx).ok()?;
    let n = val.to_u32(ctx).ok()?;
    Some(NodeId::from_raw(n))
}

fn element_get_tag_name(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let tag = with_dom(|dom| match &dom.node(id).kind {
        NodeKind::Element { tag, .. } => tag.to_ascii_uppercase(),
        _ => String::new(),
    })
    .unwrap_or_default();
    Ok(JsValue::from(js_string!(tag)))
}

fn element_get_id(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = read_attr(this, ctx, "id").unwrap_or_default();
    Ok(JsValue::from(js_string!(v)))
}

fn element_set_id(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let s = val.to_string(ctx)?.to_std_string_escaped();
    with_dom_mut(|dom| dom.set_attribute(id, "id", s));
    Ok(JsValue::undefined())
}

fn element_get_class_name(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = read_attr(this, ctx, "class").unwrap_or_default();
    Ok(JsValue::from(js_string!(v)))
}

fn element_set_class_name(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let s = val.to_string(ctx)?.to_std_string_escaped();
    with_dom_mut(|dom| dom.set_attribute(id, "class", s));
    Ok(JsValue::undefined())
}

fn element_get_text_content(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let s = with_dom(|dom| text_content_of(dom, id)).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn element_set_text_content(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let s = val.to_string(ctx)?.to_std_string_escaped();
    with_dom_mut(|dom| dom.set_text_content(id, s));
    super::observers::push_mutation_record(super::observers::MutationRecord {
        kind: super::observers::MutationKind::CharacterData,
        target: id,
        attribute_name: None,
    });
    Ok(JsValue::undefined())
}

fn element_get_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    Ok(match read_attr(this, ctx, &name) {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn element_has_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let has = with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name))
        } else {
            false
        }
    })
    .unwrap_or(false);
    Ok(JsValue::from(has))
}

fn element_set_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(name_val), Some(val_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let value = val_val.to_string(ctx)?.to_std_string_escaped();
    // If a `<video>` (or `<audio>`) element is being pointed at a
    // MediaSource via `blob:mediasource/...`, attach the page node to
    // the MediaSource instead of going through the network prefetch.
    if name.eq_ignore_ascii_case("src") && value.starts_with("blob:mediasource/") {
        if super::mse::try_attach(&value, id, ctx) {
            super::with_dom_mut(|dom| dom.set_attribute(id, &name, value.clone()));
            super::observers::push_mutation_record(super::observers::MutationRecord {
                kind: super::observers::MutationKind::Attributes,
                target: id,
                attribute_name: Some(name),
            });
            return Ok(JsValue::undefined());
        }
    }
    with_dom_mut(|dom| dom.set_attribute(id, &name, value));
    super::observers::push_mutation_record(super::observers::MutationRecord {
        kind: super::observers::MutationKind::Attributes,
        target: id,
        attribute_name: Some(name),
    });
    Ok(JsValue::undefined())
}

fn element_remove_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    with_dom_mut(|dom| dom.remove_attribute(id, &name));
    super::observers::push_mutation_record(super::observers::MutationRecord {
        kind: super::observers::MutationKind::Attributes,
        target: id,
        attribute_name: Some(name),
    });
    Ok(JsValue::undefined())
}

fn element_get_parent_element(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let parent = with_dom(|dom| {
        let p = dom.node(id).parent?;
        match dom.node(p).kind {
            NodeKind::Element { .. } => Some(p),
            _ => None,
        }
    })
    .flatten();
    match parent {
        Some(p) => Ok(JsValue::from(make_element_handle(ctx, p))),
        None => Ok(JsValue::null()),
    }
}

fn element_add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsFunction;

    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(name_val), Some(handler_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let event_type = name_val.to_string(ctx)?.to_std_string_escaped();
    let Some(handler_obj) = handler_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        // Second arg wasn't callable — match the web platform's silent
        // tolerance here.
        return Ok(JsValue::undefined());
    };
    super::engine::JS_LISTENERS.with(|slot| {
        if let Some(map_rc) = slot.borrow().as_ref() {
            map_rc
                .borrow_mut()
                .entry((id, event_type))
                .or_default()
                .push(handler);
        }
    });
    Ok(JsValue::undefined())
}

fn element_get_children(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsArray::new(ctx).into());
    };
    let ids: Vec<NodeId> = with_dom(|dom| {
        dom.children(id)
            .filter(|c| matches!(dom.node(*c).kind, NodeKind::Element { .. }))
            .collect()
    })
    .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in ids {
        arr.push(JsValue::from(make_element_handle(ctx, id)), ctx)?;
    }
    Ok(arr.into())
}

// ---------- mutation: createElement / append / remove / insert / replace ----------

fn document_create_element(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(tag_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let tag = tag_val.to_string(ctx)?.to_std_string_escaped().to_ascii_lowercase();
    let id = super::with_dom_mut(|dom| dom.create_element(tag, Vec::new()));
    match id {
        Some(id) => Ok(JsValue::from(make_element_handle(ctx, id))),
        None => Ok(JsValue::null()),
    }
}

fn document_create_text_node(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(text_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let text = text_val.to_string(ctx)?.to_std_string_escaped();
    let id = super::with_dom_mut(|dom| dom.create_text(text));
    match id {
        Some(id) => Ok(JsValue::from(make_element_handle(ctx, id))),
        None => Ok(JsValue::null()),
    }
}

fn document_get_elements_by_tag_name(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(tag_val) = args.first() else {
        return Ok(JsArray::new(ctx).into());
    };
    let tag = tag_val.to_string(ctx)?.to_std_string_escaped().to_ascii_lowercase();
    let hits: Vec<NodeId> = with_dom(|dom| {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = vec![dom.document()];
        while let Some(n) = stack.pop() {
            if let NodeKind::Element { tag: t, .. } = &dom.node(n).kind {
                if tag == "*" || t == &tag {
                    out.push(n);
                }
            }
            let mut kids: Vec<NodeId> = dom.children(n).collect();
            kids.reverse();
            stack.extend(kids);
        }
        out
    })
    .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in hits {
        arr.push(JsValue::from(make_element_handle(ctx, id)), ctx)?;
    }
    Ok(arr.into())
}

fn document_get_elements_by_class_name(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(cls_val) = args.first() else {
        return Ok(JsArray::new(ctx).into());
    };
    let cls = cls_val.to_string(ctx)?.to_std_string_escaped();
    let hits: Vec<NodeId> = with_dom(|dom| {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = vec![dom.document()];
        while let Some(n) = stack.pop() {
            if let NodeKind::Element { attrs, .. } = &dom.node(n).kind {
                if attrs
                    .iter()
                    .find(|(k, _)| k == "class")
                    .map(|(_, v)| v.split_ascii_whitespace().any(|c| c == cls))
                    .unwrap_or(false)
                {
                    out.push(n);
                }
            }
            let mut kids: Vec<NodeId> = dom.children(n).collect();
            kids.reverse();
            stack.extend(kids);
        }
        out
    })
    .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in hits {
        arr.push(JsValue::from(make_element_handle(ctx, id)), ctx)?;
    }
    Ok(arr.into())
}

fn element_append_child(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(child_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let Some(child) = read_node_id_from(child_val, ctx) else {
        return Ok(JsValue::null());
    };
    let ok = super::with_dom_mut(|dom| {
        if dom.contains(child, parent) {
            return false;
        }
        if dom.node(child).parent.is_some() {
            dom.detach(child);
        }
        dom.append_child(parent, child);
        true
    })
    .unwrap_or(false);
    if !ok {
        return Ok(JsValue::null());
    }
    super::observers::push_mutation_record(super::observers::MutationRecord {
        kind: super::observers::MutationKind::ChildList,
        target: parent,
        attribute_name: None,
    });
    Ok(child_val.clone())
}

fn element_remove_child(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(child_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let Some(child) = read_node_id_from(child_val, ctx) else {
        return Ok(JsValue::null());
    };
    super::with_dom_mut(|dom| {
        if dom.node(child).parent == Some(parent) {
            dom.detach(child);
        }
    });
    super::observers::push_mutation_record(super::observers::MutationRecord {
        kind: super::observers::MutationKind::ChildList,
        target: parent,
        attribute_name: None,
    });
    Ok(child_val.clone())
}

fn element_insert_before(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let (Some(new_val), Some(ref_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::null());
    };
    let Some(new_id) = read_node_id_from(new_val, ctx) else {
        return Ok(JsValue::null());
    };
    // `ref` may be null → behaves like appendChild.
    if ref_val.is_null() || ref_val.is_undefined() {
        return element_append_child(this, &[new_val.clone()], ctx);
    }
    let Some(ref_id) = read_node_id_from(ref_val, ctx) else {
        return Ok(JsValue::null());
    };
    super::with_dom_mut(|dom| {
        if dom.node(ref_id).parent != Some(parent) {
            return;
        }
        if dom.node(new_id).parent.is_some() {
            dom.detach(new_id);
        }
        dom.insert_before(parent, new_id, ref_id);
    });
    Ok(new_val.clone())
}

fn element_replace_child(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let (Some(new_val), Some(old_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::null());
    };
    let Some(new_id) = read_node_id_from(new_val, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(old_id) = read_node_id_from(old_val, ctx) else {
        return Ok(JsValue::null());
    };
    super::with_dom_mut(|dom| {
        if dom.node(old_id).parent != Some(parent) {
            return;
        }
        if dom.node(new_id).parent.is_some() {
            dom.detach(new_id);
        }
        dom.insert_before(parent, new_id, old_id);
        dom.detach(old_id);
    });
    Ok(old_val.clone())
}

fn element_clone_node(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let deep = args
        .first()
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let new_id = super::with_dom_mut(|dom| {
        if deep {
            dom.clone_subtree(id)
        } else {
            let kind = dom.node(id).kind.clone();
            // Strip children by creating the same kind fresh.
            match kind {
                crate::dom::NodeKind::Element { tag, attrs } => {
                    dom.create_element(tag, attrs)
                }
                crate::dom::NodeKind::Text(t) => dom.create_text(t),
                _ => dom.create_text(String::new()),
            }
        }
    });
    match new_id {
        Some(id) => Ok(JsValue::from(make_element_handle(ctx, id))),
        None => Ok(JsValue::null()),
    }
}

fn element_matches(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(sel_val) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let sel_str = sel_val.to_string(ctx)?.to_std_string_escaped();
    let Some(sels) = parse_selector_list_str(&sel_str) else {
        return Ok(JsValue::from(false));
    };
    let hit = with_dom(|dom| sels.iter().any(|s| selector_matches(s, dom, id)))
        .unwrap_or(false);
    Ok(JsValue::from(hit))
}

fn element_closest(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(start) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(sel_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let sel_str = sel_val.to_string(ctx)?.to_std_string_escaped();
    let Some(sels) = parse_selector_list_str(&sel_str) else {
        return Ok(JsValue::null());
    };
    let found = with_dom(|dom| {
        let mut cur = Some(start);
        while let Some(n) = cur {
            if matches!(dom.node(n).kind, NodeKind::Element { .. })
                && sels.iter().any(|s| selector_matches(s, dom, n))
            {
                return Some(n);
            }
            cur = dom.node(n).parent;
        }
        None
    })
    .flatten();
    match found {
        Some(n) => Ok(JsValue::from(make_element_handle(ctx, n))),
        None => Ok(JsValue::null()),
    }
}

fn element_query_selector(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(start) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(sel_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let sel_str = sel_val.to_string(ctx)?.to_std_string_escaped();
    let Some(sels) = parse_selector_list_str(&sel_str) else {
        return Ok(JsValue::null());
    };
    let hit = with_dom(|dom| collect_matching_descendants(dom, start, &sels, true).into_iter().next())
        .flatten();
    match hit {
        Some(n) => Ok(JsValue::from(make_element_handle(ctx, n))),
        None => Ok(JsValue::null()),
    }
}

fn element_query_selector_all(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(start) = read_self_node_id(this, ctx) else {
        return Ok(JsArray::new(ctx).into());
    };
    let Some(sel_val) = args.first() else {
        return Ok(JsArray::new(ctx).into());
    };
    let sel_str = sel_val.to_string(ctx)?.to_std_string_escaped();
    let Some(sels) = parse_selector_list_str(&sel_str) else {
        return Ok(JsArray::new(ctx).into());
    };
    let hits = with_dom(|dom| collect_matching_descendants(dom, start, &sels, false))
        .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in hits {
        arr.push(JsValue::from(make_element_handle(ctx, id)), ctx)?;
    }
    Ok(arr.into())
}

fn collect_matching_descendants(
    dom: &Dom,
    root: NodeId,
    selectors: &[Selector],
    first_only: bool,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = dom.children(root).collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if matches!(dom.node(n).kind, NodeKind::Element { .. })
            && selectors.iter().any(|s| selector_matches(s, dom, n))
        {
            out.push(n);
            if first_only {
                return out;
            }
        }
        let mut kids: Vec<NodeId> = dom.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

// ---------- innerHTML / outerHTML ----------

fn element_get_inner_html(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let html = with_dom(|dom| serialize_children(dom, id)).unwrap_or_default();
    Ok(JsValue::from(js_string!(html)))
}

fn element_get_outer_html(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let html = with_dom(|dom| serialize_node(dom, id)).unwrap_or_default();
    Ok(JsValue::from(js_string!(html)))
}

fn element_set_inner_html(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(target) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let src = val.to_string(ctx)?.to_std_string_escaped();
    // Parse the new content into a temporary Dom. The parser wraps any
    // bare fragment in a synthetic Document; the actual content lives
    // under the document's root element / body equivalent.
    let parsed = crate::html::parse(&src);
    super::with_dom_mut(|dom| {
        // Detach every existing child of `target`.
        let kids: Vec<NodeId> = dom.children(target).collect();
        for k in kids {
            dom.detach(k);
        }
        // Walk the parsed tree and adopt every top-level element /
        // text child of `parsed.document()` into our Dom.
        let mut to_adopt: Vec<NodeId> = parsed.children(parsed.document()).collect();
        // If the only top child is <html>, unwrap one level so we
        // adopt <html>'s children directly — closer to what authors
        // expect when they write `el.innerHTML = "<p>x</p>"`.
        if to_adopt.len() == 1 {
            if let NodeKind::Element { tag, .. } = &parsed.node(to_adopt[0]).kind {
                if tag == "html" {
                    to_adopt = parsed.children(to_adopt[0]).collect();
                }
            }
        }
        // Some HTMLs further wrap children in <head>/<body>; flatten one
        // more level if all adopted nodes are head/body.
        let all_head_body = !to_adopt.is_empty()
            && to_adopt.iter().all(|n| {
                matches!(&parsed.node(*n).kind, NodeKind::Element { tag, .. }
                    if tag == "head" || tag == "body")
            });
        if all_head_body {
            let mut flat: Vec<NodeId> = Vec::new();
            for n in &to_adopt {
                flat.extend(parsed.children(*n));
            }
            to_adopt = flat;
        }
        for n in to_adopt {
            let adopted = dom.adopt_subtree(&parsed, n);
            dom.append_child(target, adopted);
        }
    });
    Ok(JsValue::undefined())
}

fn serialize_node(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    write_node(dom, node, &mut out);
    out
}

fn serialize_children(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    for c in dom.children(node).collect::<Vec<_>>() {
        write_node(dom, c, &mut out);
    }
    out
}

fn write_node(dom: &Dom, node: NodeId, out: &mut String) {
    match &dom.node(node).kind {
        NodeKind::Text(t) => out.push_str(&escape_html_text(t)),
        NodeKind::Comment(c) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
        NodeKind::Doctype(d) => {
            out.push_str("<!DOCTYPE ");
            out.push_str(d);
            out.push('>');
        }
        NodeKind::Element { tag, attrs } => {
            out.push('<');
            out.push_str(tag);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&escape_attr(v));
                out.push('"');
            }
            // Void elements per the HTML spec.
            let is_void = matches!(
                tag.as_str(),
                "area" | "base" | "br" | "col" | "embed" | "hr" | "img"
                    | "input" | "link" | "meta" | "param" | "source"
                    | "track" | "wbr"
            );
            if is_void && dom.children(node).next().is_none() {
                out.push_str(" />");
                return;
            }
            out.push('>');
            for c in dom.children(node).collect::<Vec<_>>() {
                write_node(dom, c, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        NodeKind::Document => {
            for c in dom.children(node).collect::<Vec<_>>() {
                write_node(dom, c, out);
            }
        }
    }
}

fn escape_html_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

// ---------- classList / style / dataset ----------

const HANDLE_NODE_ID_KEY: &str = NODE_ID_KEY;

fn element_get_class_list(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    Ok(JsValue::from(build_class_list(ctx, id)))
}

fn build_class_list(ctx: &mut Context, id: NodeId) -> JsObject {
    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };
    let length_get = getter(class_list_length);
    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(HANDLE_NODE_ID_KEY),
        JsValue::from(id.index() as u32),
        Attribute::READONLY,
    );
    init.accessor(
        js_string!("length"),
        Some(length_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.function(
        NativeFunction::from_fn_ptr(class_list_add),
        js_string!("add"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(class_list_remove),
        js_string!("remove"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(class_list_toggle),
        js_string!("toggle"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(class_list_contains),
        js_string!("contains"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(class_list_replace),
        js_string!("replace"),
        2,
    );
    init.build()
}

fn class_list_classes(id: NodeId) -> Vec<String> {
    with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs
                .iter()
                .find(|(k, _)| k == "class")
                .map(|(_, v)| {
                    v.split_ascii_whitespace().map(|s| s.to_string()).collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    })
    .unwrap_or_default()
}

fn class_list_write(id: NodeId, classes: &[String]) {
    let joined = classes.join(" ");
    super::with_dom_mut(|dom| {
        if joined.is_empty() {
            dom.remove_attribute(id, "class");
        } else {
            dom.set_attribute(id, "class", joined);
        }
    });
}

fn class_list_length(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(0_u32));
    };
    Ok(JsValue::from(class_list_classes(id).len() as u32))
}

fn class_list_add(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let mut classes = class_list_classes(id);
    for a in args {
        let token = a.to_string(ctx)?.to_std_string_escaped();
        if !classes.iter().any(|c| *c == token) && !token.is_empty() {
            classes.push(token);
        }
    }
    class_list_write(id, &classes);
    Ok(JsValue::undefined())
}

fn class_list_remove(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let mut classes = class_list_classes(id);
    for a in args {
        let token = a.to_string(ctx)?.to_std_string_escaped();
        classes.retain(|c| c != &token);
    }
    class_list_write(id, &classes);
    Ok(JsValue::undefined())
}

fn class_list_toggle(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(token_val) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let token = token_val.to_string(ctx)?.to_std_string_escaped();
    let mut classes = class_list_classes(id);
    let has = classes.iter().any(|c| *c == token);
    let force = args.get(1).map(|v| v.to_boolean());
    let want = match force {
        Some(true) => true,
        Some(false) => false,
        None => !has,
    };
    if want {
        if !has && !token.is_empty() {
            classes.push(token);
        }
    } else {
        classes.retain(|c| c != &token);
    }
    class_list_write(id, &classes);
    Ok(JsValue::from(want))
}

fn class_list_contains(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(token_val) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let token = token_val.to_string(ctx)?.to_std_string_escaped();
    let has = class_list_classes(id).iter().any(|c| *c == token);
    Ok(JsValue::from(has))
}

fn class_list_replace(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let (Some(old_val), Some(new_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::from(false));
    };
    let old = old_val.to_string(ctx)?.to_std_string_escaped();
    let new = new_val.to_string(ctx)?.to_std_string_escaped();
    let mut classes = class_list_classes(id);
    let mut found = false;
    for c in classes.iter_mut() {
        if *c == old {
            *c = new.clone();
            found = true;
        }
    }
    if found {
        class_list_write(id, &classes);
    }
    Ok(JsValue::from(found))
}

// ---------- style — a tiny proxy over the inline style attribute ----------

fn element_get_style(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let style_text = with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs
                .iter()
                .find(|(k, _)| k == "style")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        } else {
            String::new()
        }
    })
    .unwrap_or_default();

    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };
    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(HANDLE_NODE_ID_KEY),
        JsValue::from(id.index() as u32),
        Attribute::READONLY,
    );
    // Pre-populate explicit accessors for the handful of properties
    // pages reach for most often. Each accessor reads/writes the
    // inline `style` attribute. For everything else, users can call
    // `setProperty(name, value)` / `getPropertyValue(name)`.
    let well_known = [
        "color",
        "background",
        "background-color",
        "display",
        "visibility",
        "opacity",
        "width",
        "height",
        "margin",
        "padding",
        "border",
        "font-size",
        "font-weight",
        "text-align",
        "transform",
        "z-index",
        "position",
        "top",
        "left",
        "right",
        "bottom",
    ];
    let _ = style_text;
    for prop in well_known {
        let camel = kebab_to_camel(prop);
        // Boa's `accessor` registers a getter+setter under one name;
        // we register both the kebab and camelCase aliases via two
        // separate properties pointing to the same handlers.
        let get1 = getter(style_get_property_wrap);
        let set1 = getter(style_set_property_wrap);
        let get2 = getter(style_get_property_wrap);
        let set2 = getter(style_set_property_wrap);
        // The wrapper reads the property name from `this.__current_prop`,
        // which we don't have — instead, we encode the name in a per-key
        // bound function via a synthetic property. For the toy that means
        // we just provide a single dynamic `setProperty` / `getPropertyValue`
        // path and skip the explicit accessor.
        let _ = (get1, set1, get2, set2);
        let _ = camel;
        let _ = prop;
        break; // bail out — keep the listing simple in this commit.
    }
    init.function(
        NativeFunction::from_fn_ptr(style_set_property),
        js_string!("setProperty"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(style_get_property_value),
        js_string!("getPropertyValue"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(style_remove_property),
        js_string!("removeProperty"),
        1,
    );
    // `cssText` is the raw inline style text.
    let css_text_get = getter(style_get_css_text);
    let css_text_set = getter(style_set_css_text);
    init.accessor(
        js_string!("cssText"),
        Some(css_text_get),
        Some(css_text_set),
        Attribute::ENUMERABLE,
    );
    Ok(JsValue::from(init.build()))
}

fn kebab_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = false;
    for c in s.chars() {
        if c == '-' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn style_get_css_text(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let text = with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs.iter().find(|(k, _)| k == "style").map(|(_, v)| v.clone())
        } else {
            None
        }
    })
    .flatten()
    .unwrap_or_default();
    Ok(JsValue::from(js_string!(text)))
}

fn style_set_css_text(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let text = val.to_string(ctx)?.to_std_string_escaped();
    super::with_dom_mut(|dom| dom.set_attribute(id, "style", text));
    Ok(JsValue::undefined())
}

fn style_get_property_value(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::from(js_string!("")));
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped().to_ascii_lowercase();
    let value = parse_inline_style_for(id)
        .into_iter()
        .find(|(k, _)| k == &name)
        .map(|(_, v)| v)
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(value)))
}

fn style_set_property(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(name_val), Some(val_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped().to_ascii_lowercase();
    let value = val_val.to_string(ctx)?.to_std_string_escaped();
    let mut decls = parse_inline_style_for(id);
    decls.retain(|(k, _)| k != &name);
    decls.push((name, value));
    write_inline_style(id, &decls);
    Ok(JsValue::undefined())
}

fn style_remove_property(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::from(js_string!("")));
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped().to_ascii_lowercase();
    let mut decls = parse_inline_style_for(id);
    let removed = decls
        .iter()
        .find(|(k, _)| k == &name)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    decls.retain(|(k, _)| k != &name);
    write_inline_style(id, &decls);
    Ok(JsValue::from(js_string!(removed)))
}

// Placeholder wrappers for the well-known property table; current
// path uses setProperty/getPropertyValue so these are unused.
fn style_get_property_wrap(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!("")))
}
fn style_set_property_wrap(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn parse_inline_style_for(id: NodeId) -> Vec<(String, String)> {
    let text = with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs.iter().find(|(k, _)| k == "style").map(|(_, v)| v.clone())
        } else {
            None
        }
    })
    .flatten()
    .unwrap_or_default();
    parse_style_decl_pairs(&text)
}

fn parse_style_decl_pairs(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in text.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some((k, v)) = chunk.split_once(':') {
            out.push((
                k.trim().to_ascii_lowercase(),
                v.trim().to_string(),
            ));
        }
    }
    out
}

fn write_inline_style(id: NodeId, decls: &[(String, String)]) {
    let joined = decls
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ");
    super::with_dom_mut(|dom| {
        if joined.is_empty() {
            dom.remove_attribute(id, "style");
        } else {
            dom.set_attribute(id, "style", joined);
        }
    });
}

// ---------- dataset — data-* attribute proxy ----------

fn element_get_dataset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    // Snapshot the current `data-*` attrs as plain JS properties. Writes
    // go through `setDataAttribute(name, value)` on the returned object;
    // we don't implement Proxy here so direct `dataset.foo = "x"`
    // assignments only update the snapshot, not the underlying attr.
    let pairs: Vec<(String, String)> = with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs
                .iter()
                .filter(|(k, _)| k.starts_with("data-"))
                .map(|(k, v)| (data_attr_to_camel(k), v.clone()))
                .collect()
        } else {
            Vec::new()
        }
    })
    .unwrap_or_default();

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(HANDLE_NODE_ID_KEY),
        JsValue::from(id.index() as u32),
        Attribute::READONLY,
    );
    for (k, v) in pairs {
        init.property(js_string!(k), JsValue::from(js_string!(v)), Attribute::all());
    }
    Ok(JsValue::from(init.build()))
}

fn data_attr_to_camel(s: &str) -> String {
    let trimmed = s.strip_prefix("data-").unwrap_or(s);
    kebab_to_camel(trimmed)
}

// ---------- helpers ----------

fn element_get_bounding_client_rect(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(empty_rect(ctx));
    };
    let rect = super::engine::JS_BOUNDING_RECTS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&id).copied())
    });
    let [x, y, w, h] = rect.unwrap_or([0.0, 0.0, 0.0, 0.0]);
    let f = |v: f32| JsValue::from(v as f64);
    let obj = ObjectInitializer::new(ctx)
        .property(js_string!("x"), f(x), Attribute::READONLY)
        .property(js_string!("y"), f(y), Attribute::READONLY)
        .property(js_string!("width"), f(w), Attribute::READONLY)
        .property(js_string!("height"), f(h), Attribute::READONLY)
        .property(js_string!("left"), f(x), Attribute::READONLY)
        .property(js_string!("top"), f(y), Attribute::READONLY)
        .property(js_string!("right"), f(x + w), Attribute::READONLY)
        .property(js_string!("bottom"), f(y + h), Attribute::READONLY)
        .build();
    Ok(JsValue::from(obj))
}

fn audio_play(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = read_self_node_id(this, ctx) {
        super::engine::JS_AUDIO_ELEMENTS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(el) = rc.borrow().get(&id) {
                    el.play();
                }
            }
        });
        super::engine::JS_VIDEO_ELEMENTS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(el) = rc.borrow().get(&id) {
                    el.play();
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn audio_pause(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = read_self_node_id(this, ctx) {
        super::engine::JS_AUDIO_ELEMENTS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(el) = rc.borrow().get(&id) {
                    el.pause();
                }
            }
        });
        super::engine::JS_VIDEO_ELEMENTS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(el) = rc.borrow().get(&id) {
                    el.pause();
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn element_get_context(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_self_node_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let ty = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // Route to the right context backend by `type`.
    if ty == "webgl" || ty == "webgl2" || ty == "experimental-webgl" {
        let is_canvas = with_dom(|dom| {
            matches!(
                &dom.node(id).kind,
                NodeKind::Element { tag, .. } if tag == "canvas"
            )
        })
        .unwrap_or(false);
        if !is_canvas {
            return Ok(JsValue::null());
        }
        return Ok(super::webgl::get_or_create_context(ctx, id));
    }
    if ty == "webgpu" {
        let is_canvas = with_dom(|dom| {
            matches!(
                &dom.node(id).kind,
                NodeKind::Element { tag, .. } if tag == "canvas"
            )
        })
        .unwrap_or(false);
        if !is_canvas {
            return Ok(JsValue::null());
        }
        return Ok(super::webgpu::get_canvas_context(ctx, id));
    }
    if ty != "2d" {
        return Ok(JsValue::null());
    }

    let (is_canvas, width, height) = with_dom(|dom| {
        if let NodeKind::Element { tag, attrs } = &dom.node(id).kind {
            if tag != "canvas" {
                return (false, 0_u32, 0_u32);
            }
            let w = attrs
                .iter()
                .find(|(k, _)| k == "width")
                .and_then(|(_, v)| v.parse::<u32>().ok())
                .unwrap_or(300);
            let h = attrs
                .iter()
                .find(|(k, _)| k == "height")
                .and_then(|(_, v)| v.parse::<u32>().ok())
                .unwrap_or(150);
            (true, w, h)
        } else {
            (false, 0, 0)
        }
    })
    .unwrap_or((false, 0, 0));
    if !is_canvas {
        return Ok(JsValue::null());
    }
    Ok(super::canvas::get_or_create_context(ctx, id, width, height))
}

fn empty_rect(ctx: &mut Context) -> JsValue {
    let zero = JsValue::from(0.0_f64);
    JsValue::from(
        ObjectInitializer::new(ctx)
            .property(js_string!("x"), zero.clone(), Attribute::READONLY)
            .property(js_string!("y"), zero.clone(), Attribute::READONLY)
            .property(js_string!("width"), zero.clone(), Attribute::READONLY)
            .property(js_string!("height"), zero.clone(), Attribute::READONLY)
            .property(js_string!("left"), zero.clone(), Attribute::READONLY)
            .property(js_string!("top"), zero.clone(), Attribute::READONLY)
            .property(js_string!("right"), zero.clone(), Attribute::READONLY)
            .property(js_string!("bottom"), zero, Attribute::READONLY)
            .build(),
    )
}

// ---------- document.cookie ----------

fn document_get_cookie(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let value = super::engine::JS_FETCH_CLIENT.with(|client_slot| {
        let client = client_slot.borrow().as_ref().cloned()?;
        let url = super::engine::JS_BASE_URL.with(|u| u.borrow().clone())?;
        Some(client.cookies_for(&url))
    });
    Ok(JsValue::from(js_string!(value.unwrap_or_default())))
}

fn document_set_cookie(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let text = val.to_string(ctx)?.to_std_string_escaped();
    super::engine::JS_FETCH_CLIENT.with(|client_slot| {
        if let Some(client) = client_slot.borrow().as_ref() {
            if let Some(url) = super::engine::JS_BASE_URL.with(|u| u.borrow().clone()) {
                client.set_cookie_for(&url, &text);
            }
        }
    });
    Ok(JsValue::undefined())
}

fn read_node_id_from(val: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = val.as_object()?;
    let v = obj.get(js_string!(NODE_ID_KEY), ctx).ok()?;
    let n = v.to_u32(ctx).ok()?;
    Some(NodeId::from_raw(n))
}

fn read_attr(this: &JsValue, ctx: &mut Context, name: &str) -> Option<String> {
    let id = read_self_node_id(this, ctx)?;
    with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        } else {
            None
        }
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::js::run_inline_scripts;

    #[test]
    fn read_title_finds_title_text() {
        let dom = html::parse("<html><head><title>Hello</title></head><body></body></html>");
        assert_eq!(read_title(&dom), "Hello");
    }

    #[test]
    fn find_by_id_walks_the_tree() {
        let dom = html::parse("<html><body><div><p id=target>X</p></div></body></html>");
        let id = find_by_id(&dom, "target").expect("id present");
        if let NodeKind::Element { tag, .. } = &dom.node(id).kind {
            assert_eq!(tag, "p");
        } else {
            panic!("not an element");
        }
    }

    #[test]
    fn text_content_concatenates_descendants() {
        let dom = html::parse("<html><body><div>a<span>b</span>c</div></body></html>");
        let root = find_root_element(&dom).unwrap();
        let div = find_descendant_by_tag(&dom, root, "div").unwrap();
        assert_eq!(text_content_of(&dom, div), "abc");
    }

    #[test]
    fn run_does_not_panic_with_dom_lookups() {
        let src = r#"
            var el = document.getElementById('hi');
            if (el) {
                console.log(el.tagName, el.id, el.textContent);
                console.log(el.getAttribute('data-x'));
                console.log(el.hasAttribute('id'));
            }
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi' data-x='42'>hello</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
    }

    #[test]
    fn collect_matching_supports_class_and_tag() {
        let dom = html::parse(
            r#"<html><body>
                <div class="foo">A</div>
                <p class="foo">B</p>
                <p class="bar">C</p>
            </body></html>"#,
        );
        let sels = parse_selector_list_str("p.foo").unwrap();
        let hits = collect_matching(&dom, &sels, false);
        assert_eq!(hits.len(), 1);
        if let NodeKind::Element { tag, .. } = &dom.node(hits[0]).kind {
            assert_eq!(tag, "p");
        }
    }

    #[test]
    fn collect_matching_first_only_short_circuits() {
        let dom = html::parse(
            r#"<html><body><span>1</span><span>2</span><span>3</span></body></html>"#,
        );
        let sels = parse_selector_list_str("span").unwrap();
        let first = collect_matching(&dom, &sels, true);
        assert_eq!(first.len(), 1);
        let all = collect_matching(&dom, &sels, false);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn query_selector_descendant_combinator_works() {
        let dom = html::parse(
            r#"<html><body>
                <div><p class="inner">A</p></div>
                <p class="inner">B</p>
            </body></html>"#,
        );
        let sels = parse_selector_list_str("div p.inner").unwrap();
        let hits = collect_matching(&dom, &sels, false);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn set_attribute_via_js_mutates_dom() {
        let src = r#"
            var el = document.getElementById('hi');
            el.setAttribute('data-x', 'new');
            el.id = 'renamed';
            el.className = 'big';
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi' data-x='old'>x</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);

        // Find the original div — its id is now "renamed" — and verify
        // the attribute changes landed on the Dom.
        let renamed = find_by_id(&dom, "renamed").expect("renamed lookup");
        if let NodeKind::Element { attrs, .. } = &dom.node(renamed).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-x").unwrap().1,
                "new"
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "class").unwrap().1,
                "big"
            );
        } else {
            panic!("not an element");
        }
    }

    #[test]
    fn text_content_setter_replaces_children() {
        let src = r#"
            document.getElementById('hi').textContent = 'replaced';
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>old<span>nested</span>tail</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);

        let div = find_by_id(&dom, "hi").unwrap();
        assert_eq!(text_content_of(&dom, div), "replaced");
        // And the original <span> is detached.
        let kids: Vec<NodeId> = dom.children(div).collect();
        assert_eq!(kids.len(), 1);
        assert!(matches!(dom.node(kids[0]).kind, NodeKind::Text(_)));
    }

    #[test]
    fn remove_attribute_via_js_works() {
        let src = r#"
            document.getElementById('hi').removeAttribute('data-x');
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi' data-x='42'>x</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);

        let div = find_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert!(attrs.iter().all(|(k, _)| k != "data-x"));
        }
    }

    #[test]
    fn class_list_add_remove_toggle_round_trip() {
        let src = r#"
            var el = document.getElementById('hi');
            el.classList.add('a', 'b');
            el.classList.add('a');           // duplicate noop
            el.classList.remove('a');
            el.classList.toggle('c');        // adds c
            el.classList.toggle('b');        // removes b
            el.setAttribute('data-has', String(el.classList.contains('c')));
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let id = find_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "class").map(|(_, v)| v.as_str()),
                Some("c")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-has").map(|(_, v)| v.as_str()),
                Some("true")
            );
        }
    }

    #[test]
    fn append_child_and_remove_child_actually_mutate_dom() {
        let src = r#"
            var hi = document.getElementById('hi');
            var p = document.createElement('p');
            p.id = 'new';
            p.textContent = 'created';
            hi.appendChild(p);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'></div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let new_id = find_by_id(&dom, "new").expect("new node");
        assert_eq!(text_content_of(&dom, new_id), "created");
        if let NodeKind::Element { tag, .. } = &dom.node(new_id).kind {
            assert_eq!(tag, "p");
        }
    }

    #[test]
    fn inner_html_setter_replaces_children() {
        let src = r#"
            document.getElementById('hi').innerHTML = '<span>hello</span>';
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>old</div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let div = find_by_id(&dom, "hi").unwrap();
        let kids: Vec<NodeId> = dom.children(div).collect();
        assert_eq!(kids.len(), 1);
        if let NodeKind::Element { tag, .. } = &dom.node(kids[0]).kind {
            assert_eq!(tag, "span");
        } else {
            panic!("not an element");
        }
        assert_eq!(text_content_of(&dom, div), "hello");
    }

    #[test]
    fn inner_html_getter_serialises_children() {
        let src = r#"
            var el = document.getElementById('hi');
            el.setAttribute('data-html', el.innerHTML);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'><span class=\"x\">y</span></div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let id = find_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            let html = attrs.iter().find(|(k, _)| k == "data-html").map(|(_, v)| v.as_str());
            assert!(html.is_some());
            // Tolerant assertion — the parser may lowercase / re-quote.
            let h = html.unwrap();
            assert!(h.contains("<span"));
            assert!(h.contains("y"));
        }
    }

    #[test]
    fn matches_and_closest_work() {
        let src = r#"
            var inner = document.getElementById('inner');
            var matched = inner.matches('p.inner');
            var ancestor = inner.closest('div.outer');
            document.getElementById('inner').setAttribute(
                'data-result',
                String(matched) + ',' + (ancestor ? ancestor.id : 'null')
            );
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='outer' class='outer'><p id='inner' class='inner'>x</p></div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let id = find_by_id(&dom, "inner").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-result").map(|(_, v)| v.as_str()),
                Some("true,outer")
            );
        }
    }

    #[test]
    fn style_set_property_writes_inline_style() {
        let src = r#"
            var el = document.getElementById('hi');
            el.style.setProperty('color', 'red');
            el.style.setProperty('font-size', '20px');
            el.setAttribute('data-color', el.style.getPropertyValue('color'));
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'></div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let id = find_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            let style = attrs.iter().find(|(k, _)| k == "style").map(|(_, v)| v.as_str()).unwrap_or("");
            assert!(style.contains("color: red"));
            assert!(style.contains("font-size: 20px"));
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-color").map(|(_, v)| v.as_str()),
                Some("red")
            );
        }
    }

    #[test]
    fn dataset_reflects_data_attributes() {
        let src = r#"
            var el = document.getElementById('hi');
            el.setAttribute('data-result', el.dataset.userName);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi' data-user-name='alice'></div><script>{src}</script></body></html>"
        ));
        run_inline_scripts(&mut dom);
        let id = find_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-result").map(|(_, v)| v.as_str()),
                Some("alice")
            );
        }
    }
}
