//! `Selection` + `Range` JS APIs, plus the `contentEditable` /
//! `isContentEditable` element accessors and a small `execCommand`
//! surface.
//!
//! Storage shape:
//!   * One `Selection` singleton per engine, held in
//!     `SELECTION_STATE`. JS code that calls `window.getSelection()`
//!     or `document.getSelection()` gets a handle bound to the same
//!     anchor/focus pair.
//!   * `Range` objects each get an integer id and an entry in
//!     `RANGE_REGISTRY`. The Selection's "current range" is just the
//!     id; `selection.getRangeAt(0)` rebuilds the JS facade.
//!
//! Out of scope for the toy:
//!   * Range.extractContents() / surroundContents() do real DOM
//!     surgery for common cases (text-only ranges).
//!   * Selection.modify("extend", ...) — the keyboard-driven path.
//!   * Highlights API — render-side selection visualisation lives in
//!     the paint layer and is a follow-up.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;

const RANGE_ID_KEY: &str = "__range_id";
const SELECTION_TAG_KEY: &str = "__is_selection";

#[derive(Clone)]
pub struct RangeState {
    pub start_node: NodeId,
    pub start_offset: u32,
    pub end_node: NodeId,
    pub end_offset: u32,
}

impl RangeState {
    pub fn collapsed(node: NodeId) -> Self {
        Self {
            start_node: node,
            start_offset: 0,
            end_node: node,
            end_offset: 0,
        }
    }
}

pub type RangeRegistry = Rc<RefCell<HashMap<u32, RangeState>>>;

#[derive(Default, Clone)]
pub struct SelectionState {
    /// Current range id, or `None` for an empty selection. We model
    /// the (deprecated-but-universal) single-range Selection rather
    /// than the Firefox-only multi-range surface.
    pub current: Option<u32>,
}

pub type SelectionShared = Rc<RefCell<SelectionState>>;

thread_local! {
    pub(crate) static RANGE_REGISTRY: RefCell<Option<RangeRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static SELECTION_STATE: RefCell<Option<SelectionShared>> =
        const { RefCell::new(None) };
    pub(crate) static RANGE_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_range_id() -> u32 {
    RANGE_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    RANGE_REGISTRY.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    SELECTION_STATE.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(SelectionState::default())));
        }
    });
    // `window.getSelection()` / `document.getSelection()` are installed
    // by their host modules; we expose a `Range` constructor so JS
    // code that calls `new Range()` (rare but valid) gets a real one.
    ctx.register_global_callable(
        js_string!("Range"),
        0,
        NativeFunction::from_fn_ptr(range_ctor),
    )
    .ok();
}

// ============ Range ============

fn range_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let document_node = crate::js::with_dom(|dom| dom.document()).unwrap_or(NodeId::from_raw(0));
    let id = store_range(RangeState::collapsed(document_node));
    Ok(build_range_object(ctx, id))
}

/// Public wrapper around [`build_range_object`]. Used by `engine.rs`
/// when `document.createRange()` mints a JS handle.
pub fn build_range_object_public(ctx: &mut Context, range_id: u32) -> JsValue {
    build_range_object(ctx, range_id)
}

/// `execCommand("insertText", false, value)` — replace the active
/// selection's contents with `value` and collapse the selection
/// after the inserted text. Only the same-text-node case mutates the
/// DOM; cross-node selections collapse onto the start.
pub fn exec_insert_text(value: &str) {
    use crate::dom::NodeKind;
    let Some(state) = current_range_state() else {
        return;
    };
    crate::js::with_dom_mut(|dom| {
        if state.start_node == state.end_node {
            if let NodeKind::Text(t) = &dom.node(state.start_node).kind {
                let chars: Vec<char> = t.chars().collect();
                let lo = (state.start_offset as usize).min(chars.len());
                let hi = (state.end_offset as usize).min(chars.len());
                let mut new_text = String::new();
                new_text.extend(chars[..lo].iter());
                new_text.push_str(value);
                new_text.extend(chars[hi..].iter());
                dom.set_text_content(state.start_node, new_text);
            }
        }
    });
    let new_offset = state.start_offset + value.chars().count() as u32;
    let id = store_range(RangeState {
        start_node: state.start_node,
        start_offset: new_offset,
        end_node: state.start_node,
        end_offset: new_offset,
    });
    with_selection_state(|s| s.current = Some(id));
}

/// `execCommand("delete" | "forwardDelete")` — delete selection
/// contents (toy: same-text-node case).
pub fn exec_delete() {
    let Some(state) = current_range_state() else {
        return;
    };
    crate::js::with_dom_mut(|dom| {
        delete_range_contents(dom, &state);
    });
    let id = store_range(RangeState {
        start_node: state.start_node,
        start_offset: state.start_offset,
        end_node: state.start_node,
        end_offset: state.start_offset,
    });
    with_selection_state(|s| s.current = Some(id));
}

/// `execCommand("selectAll")` — select the entire document.
pub fn exec_select_all() {
    let Some(doc) = crate::js::with_dom(|dom| dom.document()) else {
        return;
    };
    let len = crate::js::with_dom(|dom| node_length(dom, doc)).unwrap_or(0);
    let id = store_range(RangeState {
        start_node: doc,
        start_offset: 0,
        end_node: doc,
        end_offset: len,
    });
    with_selection_state(|s| s.current = Some(id));
}

pub fn store_range(state: RangeState) -> u32 {
    let id = next_range_id();
    if let Some(reg) = RANGE_REGISTRY.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(id, state);
    }
    id
}

fn build_range_object(ctx: &mut Context, range_id: u32) -> JsValue {
    let realm = ctx.realm().clone();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(RANGE_ID_KEY),
        JsValue::from(range_id),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("setStart", NativeFunction::from_fn_ptr(range_set_start), 2),
        ("setEnd", NativeFunction::from_fn_ptr(range_set_end), 2),
        ("setStartBefore", NativeFunction::from_fn_ptr(range_set_start_before), 1),
        ("setStartAfter", NativeFunction::from_fn_ptr(range_set_start_after), 1),
        ("setEndBefore", NativeFunction::from_fn_ptr(range_set_end_before), 1),
        ("setEndAfter", NativeFunction::from_fn_ptr(range_set_end_after), 1),
        ("collapse", NativeFunction::from_fn_ptr(range_collapse), 1),
        ("selectNode", NativeFunction::from_fn_ptr(range_select_node), 1),
        ("selectNodeContents", NativeFunction::from_fn_ptr(range_select_node_contents), 1),
        ("cloneRange", NativeFunction::from_fn_ptr(range_clone), 0),
        ("toString", NativeFunction::from_fn_ptr(range_to_string), 0),
        ("deleteContents", NativeFunction::from_fn_ptr(range_delete_contents), 0),
        ("insertNode", NativeFunction::from_fn_ptr(range_insert_node), 1),
        ("getBoundingClientRect", NativeFunction::from_fn_ptr(range_bounding_rect), 0),
        ("getClientRects", NativeFunction::from_fn_ptr(range_client_rects), 0),
        ("detach", NativeFunction::from_fn_ptr(noop), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    // Live accessors so reads reflect the registry state.
    let getters: &[(&str, fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>)] = &[
        ("startContainer", range_get_start_container),
        ("endContainer", range_get_end_container),
        ("startOffset", range_get_start_offset),
        ("endOffset", range_get_end_offset),
        ("collapsed", range_get_collapsed),
        ("commonAncestorContainer", range_get_common_ancestor),
    ];
    let handle = b.build();
    for (name, f) in getters {
        let getter = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(*f),
        )
        .build();
        let _ = handle.define_property_or_throw(
            js_string!(name.to_string()),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .enumerable(true)
                .configurable(true),
            ctx,
        );
    }
    JsValue::from(handle)
}

fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn range_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(RANGE_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn with_range_mut<R>(
    this: &JsValue,
    ctx: &mut Context,
    f: impl FnOnce(&mut RangeState) -> R,
) -> Option<R> {
    let id = range_id_of(this, ctx)?;
    let reg = RANGE_REGISTRY.with(|r| r.borrow().clone())?;
    let mut borrow = reg.borrow_mut();
    let state = borrow.get_mut(&id)?;
    Some(f(state))
}

fn with_range<R>(
    this: &JsValue,
    ctx: &mut Context,
    f: impl FnOnce(&RangeState) -> R,
) -> Option<R> {
    let id = range_id_of(this, ctx)?;
    let reg = RANGE_REGISTRY.with(|r| r.borrow().clone())?;
    let borrow = reg.borrow();
    borrow.get(&id).map(f)
}

fn node_id_of(val: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = val.as_object()?;
    let v = obj.get(js_string!(crate::js::dom::NODE_ID_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

fn range_set_start(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    with_range_mut(this, ctx, |state| {
        state.start_node = node;
        state.start_offset = offset;
        // Range invariant: start ≤ end. If we crossed, collapse end onto start.
        if state.end_node == node && state.end_offset < offset {
            state.end_offset = offset;
        }
    });
    Ok(JsValue::undefined())
}

fn range_set_end(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    with_range_mut(this, ctx, |state| {
        state.end_node = node;
        state.end_offset = offset;
        if state.start_node == node && state.start_offset > offset {
            state.start_offset = offset;
        }
    });
    Ok(JsValue::undefined())
}

fn range_set_start_before(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let parent_offset = parent_index_of(node).unwrap_or((node, 0));
    with_range_mut(this, ctx, |state| {
        state.start_node = parent_offset.0;
        state.start_offset = parent_offset.1;
    });
    Ok(JsValue::undefined())
}

fn range_set_start_after(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let parent_offset = parent_index_of(node).unwrap_or((node, 0));
    with_range_mut(this, ctx, |state| {
        state.start_node = parent_offset.0;
        state.start_offset = parent_offset.1 + 1;
    });
    Ok(JsValue::undefined())
}

fn range_set_end_before(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let parent_offset = parent_index_of(node).unwrap_or((node, 0));
    with_range_mut(this, ctx, |state| {
        state.end_node = parent_offset.0;
        state.end_offset = parent_offset.1;
    });
    Ok(JsValue::undefined())
}

fn range_set_end_after(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let parent_offset = parent_index_of(node).unwrap_or((node, 0));
    with_range_mut(this, ctx, |state| {
        state.end_node = parent_offset.0;
        state.end_offset = parent_offset.1 + 1;
    });
    Ok(JsValue::undefined())
}

fn parent_index_of(node: NodeId) -> Option<(NodeId, u32)> {
    crate::js::with_dom(|dom| {
        let parent = dom.node(node).parent?;
        let idx = dom.children(parent).position(|c| c == node)? as u32;
        Some((parent, idx))
    })
    .flatten()
}

fn range_collapse(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let to_start = args
        .first()
        .map(|v| v.to_boolean())
        .unwrap_or(true);
    with_range_mut(this, ctx, |state| {
        if to_start {
            state.end_node = state.start_node;
            state.end_offset = state.start_offset;
        } else {
            state.start_node = state.end_node;
            state.start_offset = state.end_offset;
        }
    });
    Ok(JsValue::undefined())
}

fn range_select_node(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let Some((parent, idx)) = parent_index_of(node) else {
        return Ok(JsValue::undefined());
    };
    with_range_mut(this, ctx, |state| {
        state.start_node = parent;
        state.start_offset = idx;
        state.end_node = parent;
        state.end_offset = idx + 1;
    });
    Ok(JsValue::undefined())
}

fn range_select_node_contents(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    // For Text nodes the end offset is the character count; for
    // Element/Document it's the number of children.
    let end_offset = crate::js::with_dom(|dom| node_length(dom, node)).unwrap_or(0);
    with_range_mut(this, ctx, |state| {
        state.start_node = node;
        state.start_offset = 0;
        state.end_node = node;
        state.end_offset = end_offset;
    });
    Ok(JsValue::undefined())
}

fn node_length(dom: &crate::dom::Dom, node: NodeId) -> u32 {
    use crate::dom::NodeKind;
    match &dom.node(node).kind {
        NodeKind::Text(t) => t.chars().count() as u32,
        _ => dom.children(node).count() as u32,
    }
}

fn range_clone(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = with_range(this, ctx, |s| s.clone()) else {
        return Ok(JsValue::null());
    };
    let id = store_range(state);
    Ok(build_range_object(ctx, id))
}

fn range_to_string(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let state = match with_range(this, ctx, |s| s.clone()) {
        Some(s) => s,
        None => return Ok(JsValue::from(js_string!(""))),
    };
    let text = crate::js::with_dom(|dom| extract_range_text(dom, &state)).unwrap_or_default();
    Ok(JsValue::from(js_string!(text)))
}

fn extract_range_text(dom: &crate::dom::Dom, state: &RangeState) -> String {
    use crate::dom::NodeKind;
    // Toy: when start and end are the same Text node, slice it.
    // Otherwise concatenate the text content of every text node
    // between them in document order. This won't be spec-accurate
    // for partial-element selections, but covers the dominant case
    // (selecting within a contenteditable's body).
    if state.start_node == state.end_node {
        if let NodeKind::Text(t) = &dom.node(state.start_node).kind {
            let chars: Vec<char> = t.chars().collect();
            let lo = (state.start_offset as usize).min(chars.len());
            let hi = (state.end_offset as usize).min(chars.len());
            return chars[lo..hi].iter().collect();
        }
    }
    let mut out = String::new();
    let mut started = false;
    let mut stopped = false;
    walk_document(dom, &mut |id, depth| {
        let _ = depth;
        if stopped {
            return;
        }
        if id == state.start_node {
            started = true;
        }
        if started {
            if let NodeKind::Text(t) = &dom.node(id).kind {
                out.push_str(t);
            }
        }
        if id == state.end_node {
            stopped = true;
        }
    });
    out
}

fn walk_document(dom: &crate::dom::Dom, f: &mut impl FnMut(NodeId, usize)) {
    walk_inner(dom, dom.document(), 0, f);
}

fn walk_inner(
    dom: &crate::dom::Dom,
    node: NodeId,
    depth: usize,
    f: &mut impl FnMut(NodeId, usize),
) {
    f(node, depth);
    for c in dom.children(node).collect::<Vec<_>>() {
        walk_inner(dom, c, depth + 1, f);
    }
}

fn range_delete_contents(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let state = match with_range(this, ctx, |s| s.clone()) {
        Some(s) => s,
        None => return Ok(JsValue::undefined()),
    };
    crate::js::with_dom_mut(|dom| {
        delete_range_contents(dom, &state);
    });
    with_range_mut(this, ctx, |s| {
        s.end_node = s.start_node;
        s.end_offset = s.start_offset;
    });
    Ok(JsValue::undefined())
}

fn delete_range_contents(dom: &mut crate::dom::Dom, state: &RangeState) {
    use crate::dom::NodeKind;
    // Toy: only the same-text-node case mutates the DOM. Cross-node
    // selections are clamped — full implementation would split text
    // nodes and recursively detach intermediate subtrees.
    if state.start_node == state.end_node {
        if let NodeKind::Text(t) = &dom.node(state.start_node).kind {
            let chars: Vec<char> = t.chars().collect();
            let lo = (state.start_offset as usize).min(chars.len());
            let hi = (state.end_offset as usize).min(chars.len());
            if lo < hi {
                let mut new_text = String::new();
                new_text.extend(chars[..lo].iter());
                new_text.extend(chars[hi..].iter());
                dom.set_text_content(state.start_node, new_text);
            }
        }
    }
}

fn range_insert_node(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let state = match with_range(this, ctx, |s| s.clone()) {
        Some(s) => s,
        None => return Ok(JsValue::undefined()),
    };
    crate::js::with_dom_mut(|dom| {
        // Insert before the start position. If start is inside a text
        // node, splitting the text would be ideal — toy collapses by
        // inserting after the text node instead.
        let target_parent = match parent_index_of(state.start_node) {
            Some((p, _)) => p,
            None => state.start_node,
        };
        let _ = dom.append_child(target_parent, node);
    });
    Ok(JsValue::undefined())
}

fn range_bounding_rect(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Toy: report the bounding rect of the range's startContainer.
    let state = match with_range(this, ctx, |s| s.clone()) {
        Some(s) => s,
        None => return Ok(empty_rect_obj(ctx)),
    };
    let rect = crate::js::engine::JS_BOUNDING_RECTS
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|rc| rc.borrow().get(&state.start_node).copied())
        })
        .unwrap_or([0.0, 0.0, 0.0, 0.0]);
    Ok(rect_to_js(ctx, rect))
}

fn range_client_rects(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let rect = range_bounding_rect(this, &[], ctx)?;
    let arr = JsArray::new(ctx);
    let _ = arr.push(rect, ctx);
    Ok(arr.into())
}

fn rect_to_js(ctx: &mut Context, r: [f32; 4]) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(js_string!("x"), JsValue::from(r[0] as f64), Attribute::READONLY)
        .property(js_string!("y"), JsValue::from(r[1] as f64), Attribute::READONLY)
        .property(js_string!("width"), JsValue::from(r[2] as f64), Attribute::READONLY)
        .property(js_string!("height"), JsValue::from(r[3] as f64), Attribute::READONLY)
        .property(
            js_string!("left"),
            JsValue::from(r[0] as f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("top"),
            JsValue::from(r[1] as f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("right"),
            JsValue::from((r[0] + r[2]) as f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("bottom"),
            JsValue::from((r[1] + r[3]) as f64),
            Attribute::READONLY,
        )
        .build()
        .into()
}

fn empty_rect_obj(ctx: &mut Context) -> JsValue {
    rect_to_js(ctx, [0.0, 0.0, 0.0, 0.0])
}

// Range accessors

fn range_get_start_container(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = with_range(this, ctx, |s| s.start_node) else {
        return Ok(JsValue::null());
    };
    Ok(JsValue::from(crate::js::dom::make_element_handle(ctx, node)))
}

fn range_get_end_container(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = with_range(this, ctx, |s| s.end_node) else {
        return Ok(JsValue::null());
    };
    Ok(JsValue::from(crate::js::dom::make_element_handle(ctx, node)))
}

fn range_get_start_offset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(with_range(this, ctx, |s| s.start_offset).unwrap_or(0)))
}

fn range_get_end_offset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(with_range(this, ctx, |s| s.end_offset).unwrap_or(0)))
}

fn range_get_collapsed(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let collapsed = with_range(this, ctx, |s| {
        s.start_node == s.end_node && s.start_offset == s.end_offset
    })
    .unwrap_or(true);
    Ok(JsValue::from(collapsed))
}

fn range_get_common_ancestor(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let state = match with_range(this, ctx, |s| s.clone()) {
        Some(s) => s,
        None => return Ok(JsValue::null()),
    };
    let ancestor = crate::js::with_dom(|dom| {
        lowest_common_ancestor(dom, state.start_node, state.end_node)
    })
    .flatten();
    match ancestor {
        Some(n) => Ok(JsValue::from(crate::js::dom::make_element_handle(ctx, n))),
        None => Ok(JsValue::null()),
    }
}

fn lowest_common_ancestor(
    dom: &crate::dom::Dom,
    a: NodeId,
    b: NodeId,
) -> Option<NodeId> {
    let a_path = ancestor_path(dom, a);
    let b_path = ancestor_path(dom, b);
    let mut common = None;
    for (x, y) in a_path.iter().zip(b_path.iter()) {
        if x == y {
            common = Some(*x);
        } else {
            break;
        }
    }
    common
}

fn ancestor_path(dom: &crate::dom::Dom, mut node: NodeId) -> Vec<NodeId> {
    let mut out = vec![node];
    while let Some(p) = dom.node(node).parent {
        out.push(p);
        node = p;
    }
    out.reverse();
    out
}

// ============ Selection ============

/// `window.getSelection()` / `document.getSelection()` build target.
/// All callers get a fresh JS handle that talks to the same shared
/// `SelectionState`.
pub fn get_selection_object(ctx: &mut Context) -> JsValue {
    let realm = ctx.realm().clone();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(SELECTION_TAG_KEY), JsValue::from(true), Attribute::READONLY);
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("getRangeAt", NativeFunction::from_fn_ptr(selection_get_range_at), 1),
        ("addRange", NativeFunction::from_fn_ptr(selection_add_range), 1),
        ("removeAllRanges", NativeFunction::from_fn_ptr(selection_remove_all_ranges), 0),
        ("removeRange", NativeFunction::from_fn_ptr(selection_remove_range), 1),
        ("collapse", NativeFunction::from_fn_ptr(selection_collapse), 2),
        ("collapseToStart", NativeFunction::from_fn_ptr(selection_collapse_to_start), 0),
        ("collapseToEnd", NativeFunction::from_fn_ptr(selection_collapse_to_end), 0),
        ("extend", NativeFunction::from_fn_ptr(selection_extend), 2),
        ("setBaseAndExtent", NativeFunction::from_fn_ptr(selection_set_base_and_extent), 4),
        ("selectAllChildren", NativeFunction::from_fn_ptr(selection_select_all_children), 1),
        ("toString", NativeFunction::from_fn_ptr(selection_to_string), 0),
        ("empty", NativeFunction::from_fn_ptr(selection_remove_all_ranges), 0),
        ("deleteFromDocument", NativeFunction::from_fn_ptr(selection_delete_from_document), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    let handle = b.build();
    // Live accessors.
    let getters: &[(&str, fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>)] = &[
        ("anchorNode", selection_anchor_node),
        ("anchorOffset", selection_anchor_offset),
        ("focusNode", selection_focus_node),
        ("focusOffset", selection_focus_offset),
        ("rangeCount", selection_range_count),
        ("isCollapsed", selection_is_collapsed),
        ("type", selection_type),
    ];
    for (name, f) in getters {
        let getter = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(*f),
        )
        .build();
        let _ = handle.define_property_or_throw(
            js_string!(name.to_string()),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .enumerable(true)
                .configurable(true),
            ctx,
        );
    }
    JsValue::from(handle)
}

fn selection_state() -> Option<SelectionShared> {
    SELECTION_STATE.with(|r| r.borrow().clone())
}

fn with_selection_state<R>(f: impl FnOnce(&mut SelectionState) -> R) -> Option<R> {
    let shared = selection_state()?;
    let mut s = shared.borrow_mut();
    Some(f(&mut s))
}

fn current_range_state() -> Option<RangeState> {
    let id = selection_state()?.borrow().current?;
    let reg = RANGE_REGISTRY.with(|r| r.borrow().clone())?;
    let result = reg.borrow().get(&id).cloned();
    result
}

fn selection_get_range_at(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let idx = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if idx != 0 {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getRangeAt: index out of bounds")
            .into());
    }
    let Some(id) = selection_state().and_then(|s| s.borrow().current) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getRangeAt: no range")
            .into());
    };
    Ok(build_range_object(ctx, id))
}

fn selection_add_range(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(id) = arg
        .as_object()
        .and_then(|o| o.get(js_string!(RANGE_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    else {
        return Ok(JsValue::undefined());
    };
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_remove_all_ranges(
    _: &JsValue,
    _: &[JsValue],
    _: &mut Context,
) -> JsResult<JsValue> {
    with_selection_state(|s| s.current = None);
    Ok(JsValue::undefined())
}

fn selection_remove_range(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    with_selection_state(|s| s.current = None);
    Ok(JsValue::undefined())
}

fn selection_collapse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let id = store_range(RangeState {
        start_node: node,
        start_offset: offset,
        end_node: node,
        end_offset: offset,
    });
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_collapse_to_start(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let Some(state) = current_range_state() else {
        return Ok(JsValue::undefined());
    };
    let id = store_range(RangeState {
        start_node: state.start_node,
        start_offset: state.start_offset,
        end_node: state.start_node,
        end_offset: state.start_offset,
    });
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_collapse_to_end(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let Some(state) = current_range_state() else {
        return Ok(JsValue::undefined());
    };
    let id = store_range(RangeState {
        start_node: state.end_node,
        start_offset: state.end_offset,
        end_node: state.end_node,
        end_offset: state.end_offset,
    });
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_extend(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if let Some(state) = current_range_state() {
        let id = store_range(RangeState {
            start_node: state.start_node,
            start_offset: state.start_offset,
            end_node: node,
            end_offset: offset,
        });
        with_selection_state(|s| s.current = Some(id));
    }
    Ok(JsValue::undefined())
}

fn selection_set_base_and_extent(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(anchor) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let anchor_offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let Some(focus) = args.get(2).and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let focus_offset = args.get(3).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let id = store_range(RangeState {
        start_node: anchor,
        start_offset: anchor_offset,
        end_node: focus,
        end_offset: focus_offset,
    });
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_select_all_children(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = args.first().and_then(|v| node_id_of(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let len = crate::js::with_dom(|dom| node_length(dom, node)).unwrap_or(0);
    let id = store_range(RangeState {
        start_node: node,
        start_offset: 0,
        end_node: node,
        end_offset: len,
    });
    with_selection_state(|s| s.current = Some(id));
    Ok(JsValue::undefined())
}

fn selection_to_string(_: &JsValue, _: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = current_range_state() else {
        return Ok(JsValue::from(js_string!("")));
    };
    let text =
        crate::js::with_dom(|dom| extract_range_text(dom, &state)).unwrap_or_default();
    Ok(JsValue::from(js_string!(text)))
}

fn selection_delete_from_document(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = current_range_state() {
        crate::js::with_dom_mut(|dom| delete_range_contents(dom, &state));
        with_selection_state(|s| s.current = None);
    }
    Ok(JsValue::undefined())
}

// Selection accessors

fn selection_anchor_node(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = current_range_state() else {
        return Ok(JsValue::null());
    };
    Ok(JsValue::from(crate::js::dom::make_element_handle(
        ctx,
        state.start_node,
    )))
}

fn selection_anchor_offset(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        current_range_state().map(|s| s.start_offset).unwrap_or(0),
    ))
}

fn selection_focus_node(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = current_range_state() else {
        return Ok(JsValue::null());
    };
    Ok(JsValue::from(crate::js::dom::make_element_handle(
        ctx,
        state.end_node,
    )))
}

fn selection_focus_offset(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        current_range_state().map(|s| s.end_offset).unwrap_or(0),
    ))
}

fn selection_range_count(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let count = selection_state()
        .map(|s| if s.borrow().current.is_some() { 1 } else { 0 })
        .unwrap_or(0);
    Ok(JsValue::from(count as u32))
}

fn selection_is_collapsed(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let collapsed = current_range_state()
        .map(|s| s.start_node == s.end_node && s.start_offset == s.end_offset)
        .unwrap_or(true);
    Ok(JsValue::from(collapsed))
}

fn selection_type(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let kind = match current_range_state() {
        None => "None",
        Some(s) if s.start_node == s.end_node && s.start_offset == s.end_offset => "Caret",
        Some(_) => "Range",
    };
    Ok(JsValue::from(js_string!(kind.to_string())))
}
