//! `<dialog>` element + Popover API.
//!
//! Both surfaces track an `open` (dialog) / "shown" (popover) flag
//! against the DOM. Dialogs reflect via the `open` attribute (which
//! the existing UA stylesheet already keys on); popovers track in a
//! per-node thread-local set so `:popover-open` matching + the
//! `popovertarget` invoker can introspect state.
//!
//! Out of scope for the toy:
//!   * Top-layer rendering (modal dialogs / popover=auto don't
//!     visually float above other content; they're laid out where
//!     they live in the DOM).
//!   * Light-dismiss for popover=auto (clicking outside doesn't
//!     close).
//!   * Focus trapping inside `showModal()`.

use std::cell::RefCell;
use std::collections::HashSet;

use boa_engine::{
    js_string,
    object::ObjectInitializer,
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::{NodeId, NodeKind};

thread_local! {
    /// Element ids currently in the "shown" popover state.
    pub(crate) static OPEN_POPOVERS: RefCell<HashSet<NodeId>> =
        RefCell::new(HashSet::new());
    /// Per-dialog return-value string, set by `dialog.close(value)`.
    pub(crate) static DIALOG_RETURN_VALUES: RefCell<
        std::collections::HashMap<NodeId, String>,
    > = RefCell::new(std::collections::HashMap::new());
}

// ============ <dialog> ============

pub fn dialog_show(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    crate::js::with_dom_mut(|dom| dom.set_attribute(id, "open", String::new()));
    Ok(JsValue::undefined())
}

pub fn dialog_show_modal(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // `open` + the marker attribute the UA stylesheet can hook on to
    // render the backdrop.
    crate::js::with_dom_mut(|dom| {
        dom.set_attribute(id, "open", String::new());
        dom.set_attribute(id, "data-modal", "true".to_string());
    });
    Ok(JsValue::undefined())
}

pub fn dialog_close(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let return_value = args
        .first()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    DIALOG_RETURN_VALUES.with(|m| {
        m.borrow_mut().insert(id, return_value);
    });
    crate::js::with_dom_mut(|dom| {
        dom.remove_attribute(id, "open");
        dom.remove_attribute(id, "data-modal");
    });
    fire_event(ctx, id, "close");
    Ok(JsValue::undefined())
}

pub fn dialog_get_open(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let open = crate::js::with_dom(|dom| {
        matches!(&dom.node(id).kind, NodeKind::Element { attrs, .. }
            if attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("open")))
    })
    .unwrap_or(false);
    Ok(JsValue::from(open))
}

pub fn dialog_set_open(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let should_open = args.first().map(|v| v.to_boolean()).unwrap_or(false);
    crate::js::with_dom_mut(|dom| {
        if should_open {
            dom.set_attribute(id, "open", String::new());
        } else {
            dom.remove_attribute(id, "open");
            dom.remove_attribute(id, "data-modal");
        }
    });
    Ok(JsValue::undefined())
}

pub fn dialog_get_return_value(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let v = DIALOG_RETURN_VALUES.with(|m| m.borrow().get(&id).cloned().unwrap_or_default());
    Ok(JsValue::from(js_string!(v)))
}

pub fn dialog_set_return_value(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let v = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    DIALOG_RETURN_VALUES.with(|m| {
        m.borrow_mut().insert(id, v);
    });
    Ok(JsValue::undefined())
}

// ============ Popover API ============

pub fn element_show_popover(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let was_open = OPEN_POPOVERS.with(|s| s.borrow().contains(&id));
    if was_open {
        return Ok(JsValue::undefined());
    }
    fire_toggle_events(ctx, id, "closed", "open");
    OPEN_POPOVERS.with(|s| {
        s.borrow_mut().insert(id);
    });
    Ok(JsValue::undefined())
}

pub fn element_hide_popover(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let was_open = OPEN_POPOVERS.with(|s| s.borrow().contains(&id));
    if !was_open {
        return Ok(JsValue::undefined());
    }
    fire_toggle_events(ctx, id, "open", "closed");
    OPEN_POPOVERS.with(|s| {
        s.borrow_mut().remove(&id);
    });
    Ok(JsValue::undefined())
}

pub fn element_toggle_popover(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let force = args
        .first()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_boolean());
    let was_open = OPEN_POPOVERS.with(|s| s.borrow().contains(&id));
    let want_open = force.unwrap_or(!was_open);
    if want_open == was_open {
        return Ok(JsValue::from(was_open));
    }
    if want_open {
        let _ = element_show_popover(this, &[], ctx);
    } else {
        let _ = element_hide_popover(this, &[], ctx);
    }
    Ok(JsValue::from(want_open))
}

/// Public hook for `main.rs` click handling: when a `<button>` (or
/// `<input type=button>`) has `popovertarget="some-id"`, clicking it
/// toggles that target's popover state. Returns true if a popover was
/// invoked (caller can skip its default click action).
pub fn try_invoke_popovertarget(button: NodeId, ctx: &mut Context) -> bool {
    let (target_id_str, action) = crate::js::with_dom(|dom| {
        let NodeKind::Element { attrs, .. } = &dom.node(button).kind else {
            return (String::new(), "toggle".to_string());
        };
        let target = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("popovertarget"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let action = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("popovertargetaction"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "toggle".to_string());
        (target, action)
    })
    .unwrap_or((String::new(), "toggle".to_string()));
    if target_id_str.is_empty() {
        return false;
    }
    let target = crate::js::with_dom(|dom| find_by_id(dom, &target_id_str)).flatten();
    let Some(target) = target else {
        return false;
    };
    let was_open = OPEN_POPOVERS.with(|s| s.borrow().contains(&target));
    let want_open = match action.to_ascii_lowercase().as_str() {
        "show" => true,
        "hide" => false,
        _ => !was_open,
    };
    if want_open == was_open {
        return true;
    }
    if want_open {
        fire_toggle_events(ctx, target, "closed", "open");
        OPEN_POPOVERS.with(|s| {
            s.borrow_mut().insert(target);
        });
    } else {
        fire_toggle_events(ctx, target, "open", "closed");
        OPEN_POPOVERS.with(|s| {
            s.borrow_mut().remove(&target);
        });
    }
    true
}

fn find_by_id(dom: &crate::dom::Dom, wanted: &str) -> Option<NodeId> {
    use std::collections::VecDeque;
    let mut stack: VecDeque<NodeId> = VecDeque::new();
    stack.push_back(dom.document());
    while let Some(n) = stack.pop_front() {
        if let NodeKind::Element { attrs, .. } = &dom.node(n).kind {
            if attrs
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == wanted)
            {
                return Some(n);
            }
        }
        for c in dom.children(n) {
            stack.push_back(c);
        }
    }
    None
}

// ============ event helpers ============

fn fire_toggle_events(ctx: &mut Context, target: NodeId, old_state: &str, new_state: &str) {
    fire_toggle_event(ctx, target, "beforetoggle", old_state, new_state);
    fire_toggle_event(ctx, target, "toggle", old_state, new_state);
}

fn fire_toggle_event(
    ctx: &mut Context,
    target: NodeId,
    name: &str,
    old_state: &str,
    new_state: &str,
) {
    use boa_engine::object::builtins::JsFunction;
    use super::engine::JS_LISTENERS;
    let listeners: Vec<JsFunction> = JS_LISTENERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&(target, name.to_string())).cloned())
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
            js_string!("oldState"),
            JsValue::from(js_string!(old_state.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("newState"),
            JsValue::from(js_string!(new_state.to_string())),
            Attribute::READONLY,
        )
        .build();
    let event_val = JsValue::from(event);
    for f in listeners {
        let _ = f.call(&JsValue::undefined(), &[event_val.clone()], ctx);
    }
}

fn fire_event(ctx: &mut Context, target: NodeId, name: &str) {
    use boa_engine::object::builtins::JsFunction;
    use super::engine::JS_LISTENERS;
    let listeners: Vec<JsFunction> = JS_LISTENERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&(target, name.to_string())).cloned())
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
        .build();
    let event_val = JsValue::from(event);
    for f in listeners {
        let _ = f.call(&JsValue::undefined(), &[event_val.clone()], ctx);
    }
}

fn read_node_id(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(crate::js::dom::NODE_ID_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}
