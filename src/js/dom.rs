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

const NODE_ID_KEY: &str = "__nodeId";

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

    ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(get_element_by_id),
            js_string!("getElementById"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .property(
            js_string!("documentElement"),
            root_value,
            Attribute::READONLY,
        )
        .property(js_string!("body"), body_value, Attribute::READONLY)
        .property(js_string!("title"), js_string!(title), Attribute::READONLY)
        .build()
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
    init.build()
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
    with_dom_mut(|dom| dom.set_attribute(id, &name, value));
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
}
