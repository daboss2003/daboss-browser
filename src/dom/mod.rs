//! Arena DOM. Every node lives in one `Vec`, every reference is a `NodeId`
//! index. The borrow checker is happy because there's only ever one owner
//! of the tree (the `Dom` itself), and parent / child / sibling pointers
//! are integers, not references.

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct NodeId(u32);

impl NodeId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct a `NodeId` from its raw index. Used by the JS subsystem
    /// to round-trip ids that have crossed into JS land as integers.
    pub const fn from_raw(i: u32) -> Self {
        NodeId(i)
    }
}

#[derive(Debug)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub kind: NodeKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Comment payload preserved for phase-7 JS DOM access
pub enum NodeKind {
    Document,
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
    Comment(String),
    Doctype(String),
}

pub struct Dom {
    nodes: Vec<Node>,
    document: NodeId,
}

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    pub fn new() -> Self {
        let mut dom = Self {
            nodes: Vec::new(),
            document: NodeId(0),
        };
        dom.document = dom.alloc(NodeKind::Document);
        dom
    }

    pub fn document(&self) -> NodeId {
        self.document
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    pub fn create_element(
        &mut self,
        tag: impl Into<String>,
        attrs: Vec<(String, String)>,
    ) -> NodeId {
        self.alloc(NodeKind::Element {
            tag: tag.into(),
            attrs,
        })
    }

    pub fn create_text(&mut self, text: impl Into<String>) -> NodeId {
        self.alloc(NodeKind::Text(text.into()))
    }

    pub fn create_comment(&mut self, text: impl Into<String>) -> NodeId {
        self.alloc(NodeKind::Comment(text.into()))
    }

    pub fn create_doctype(&mut self, name: impl Into<String>) -> NodeId {
        self.alloc(NodeKind::Doctype(name.into()))
    }

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            next_sibling: None,
            prev_sibling: None,
            kind,
        });
        id
    }

    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.node_mut(child).parent = Some(parent);
        self.node_mut(child).prev_sibling = self.node(parent).last_child;
        self.node_mut(child).next_sibling = None;

        if let Some(last) = self.node(parent).last_child {
            self.node_mut(last).next_sibling = Some(child);
        } else {
            self.node_mut(parent).first_child = Some(child);
        }
        self.node_mut(parent).last_child = Some(child);
    }

    pub fn insert_before(&mut self, parent: NodeId, new: NodeId, reference: NodeId) {
        let prev = self.node(reference).prev_sibling;
        self.node_mut(new).parent = Some(parent);
        self.node_mut(new).prev_sibling = prev;
        self.node_mut(new).next_sibling = Some(reference);
        self.node_mut(reference).prev_sibling = Some(new);
        match prev {
            Some(p) => self.node_mut(p).next_sibling = Some(new),
            None => self.node_mut(parent).first_child = Some(new),
        }
    }

    pub fn children(&self, parent: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.node(parent).first_child,
        }
    }

    /// Total number of arena slots (including detached ones). Used by
    /// the JS engine as a cheap "anything got allocated" signal.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Set an attribute on an element, overwriting any existing value with
    /// the same name. No-op for non-element nodes. Used by the JS DOM
    /// bindings (`element.setAttribute`, `element.className = ...`).
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: String) {
        if let NodeKind::Element { attrs, .. } = &mut self.node_mut(node).kind {
            if let Some(slot) = attrs.iter_mut().find(|(k, _)| k == name) {
                slot.1 = value;
            } else {
                attrs.push((name.to_string(), value));
            }
        }
    }

    /// Remove the named attribute from an element. No-op for non-elements
    /// or missing attributes.
    pub fn remove_attribute(&mut self, node: NodeId, name: &str) {
        if let NodeKind::Element { attrs, .. } = &mut self.node_mut(node).kind {
            attrs.retain(|(k, _)| k != name);
        }
    }

    /// Detach `child` from its current parent. No-op if `child` has no
    /// parent. Used by JS-side `removeChild` / `replaceChild`.
    pub fn detach(&mut self, child: NodeId) {
        let parent = match self.node(child).parent {
            Some(p) => p,
            None => return,
        };
        let prev = self.node(child).prev_sibling;
        let next = self.node(child).next_sibling;
        match prev {
            Some(p) => self.node_mut(p).next_sibling = next,
            None => self.node_mut(parent).first_child = next,
        }
        match next {
            Some(n) => self.node_mut(n).prev_sibling = prev,
            None => self.node_mut(parent).last_child = prev,
        }
        self.node_mut(child).parent = None;
        self.node_mut(child).prev_sibling = None;
        self.node_mut(child).next_sibling = None;
    }

    /// Returns `true` if `ancestor` is an ancestor of `descendant`
    /// (inclusive). Used to validate `appendChild` / `insertBefore` so
    /// we don't make a tree cyclic.
    pub fn contains(&self, ancestor: NodeId, descendant: NodeId) -> bool {
        let mut cur = Some(descendant);
        while let Some(n) = cur {
            if n == ancestor {
                return true;
            }
            cur = self.node(n).parent;
        }
        false
    }

    /// Recursively clone the subtree rooted at `node` into the same
    /// arena. Returns the id of the new root. The clone is detached
    /// (has no parent).
    pub fn clone_subtree(&mut self, node: NodeId) -> NodeId {
        let kind = self.node(node).kind.clone();
        let new_root = self.alloc(kind);
        let kids: Vec<NodeId> = self.children(node).collect();
        for k in kids {
            let cloned = self.clone_subtree(k);
            self.append_child(new_root, cloned);
        }
        new_root
    }

    /// Copy `other_root`'s subtree from `other` into this Dom. Returns
    /// the new root's id in this Dom. The returned node is detached.
    /// Useful for `innerHTML =` where the right-hand side is a freshly
    /// parsed Dom that needs to be spliced in.
    pub fn adopt_subtree(&mut self, other: &Dom, other_root: NodeId) -> NodeId {
        let kind = other.node(other_root).kind.clone();
        let new_root = self.alloc(kind);
        let kids: Vec<NodeId> = other.children(other_root).collect();
        for k in kids {
            let copied = self.adopt_subtree(other, k);
            self.append_child(new_root, copied);
        }
        new_root
    }

    /// Detach every child of `parent` and replace them with a single text
    /// node containing `text`. Equivalent to setting `textContent` on an
    /// element in the web platform.
    pub fn set_text_content(&mut self, parent: NodeId, text: String) {
        // Walk all current children and unlink them (we leave their slots
        // in `nodes` — the arena is append-only by design — but break the
        // parent/sibling pointers so they're unreachable from the tree).
        let kids: Vec<NodeId> = self.children(parent).collect();
        for k in kids {
            self.node_mut(k).parent = None;
            self.node_mut(k).prev_sibling = None;
            self.node_mut(k).next_sibling = None;
        }
        self.node_mut(parent).first_child = None;
        self.node_mut(parent).last_child = None;

        if !text.is_empty() {
            let text_id = self.alloc(NodeKind::Text(text));
            self.append_child(parent, text_id);
        }
    }
}

pub struct Children<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let current = self.next?;
        self.next = self.dom.node(current).next_sibling;
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dom_has_only_document() {
        let dom = Dom::new();
        assert!(matches!(dom.node(dom.document()).kind, NodeKind::Document));
        assert!(dom.children(dom.document()).next().is_none());
    }

    #[test]
    fn children_iterate_in_insertion_order() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let a = dom.create_element("a", vec![]);
        let b = dom.create_element("b", vec![]);
        let c = dom.create_element("c", vec![]);
        dom.append_child(doc, a);
        dom.append_child(doc, b);
        dom.append_child(doc, c);
        let kids: Vec<NodeId> = dom.children(doc).collect();
        assert_eq!(kids, vec![a, b, c]);
    }

    #[test]
    fn set_attribute_overwrites_existing_value() {
        let mut dom = Dom::new();
        let e = dom.create_element("div", vec![("id".into(), "old".into())]);
        dom.set_attribute(e, "id", "new".into());
        if let NodeKind::Element { attrs, .. } = &dom.node(e).kind {
            assert_eq!(attrs.iter().find(|(k, _)| k == "id").unwrap().1, "new");
            assert_eq!(attrs.len(), 1);
        } else {
            panic!("not an element");
        }
    }

    #[test]
    fn set_attribute_appends_when_missing() {
        let mut dom = Dom::new();
        let e = dom.create_element("div", vec![]);
        dom.set_attribute(e, "data-x", "1".into());
        if let NodeKind::Element { attrs, .. } = &dom.node(e).kind {
            assert_eq!(attrs[0].0, "data-x");
            assert_eq!(attrs[0].1, "1");
        } else {
            panic!("not an element");
        }
    }

    #[test]
    fn remove_attribute_works() {
        let mut dom = Dom::new();
        let e = dom.create_element("div", vec![("id".into(), "x".into())]);
        dom.remove_attribute(e, "id");
        if let NodeKind::Element { attrs, .. } = &dom.node(e).kind {
            assert!(attrs.is_empty());
        }
    }

    #[test]
    fn set_text_content_replaces_children() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let div = dom.create_element("div", vec![]);
        dom.append_child(doc, div);
        let old = dom.create_text("old");
        dom.append_child(div, old);

        dom.set_text_content(div, "new".into());

        let kids: Vec<NodeId> = dom.children(div).collect();
        assert_eq!(kids.len(), 1);
        if let NodeKind::Text(t) = &dom.node(kids[0]).kind {
            assert_eq!(t, "new");
        } else {
            panic!("expected text node");
        }
        // The old text node is detached, not in the tree any more.
        assert_eq!(dom.node(old).parent, None);
    }

    #[test]
    fn parent_and_sibling_links_are_set() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let a = dom.create_element("a", vec![]);
        let b = dom.create_element("b", vec![]);
        dom.append_child(doc, a);
        dom.append_child(doc, b);
        assert_eq!(dom.node(a).parent, Some(doc));
        assert_eq!(dom.node(b).parent, Some(doc));
        assert_eq!(dom.node(a).next_sibling, Some(b));
        assert_eq!(dom.node(b).prev_sibling, Some(a));
        assert_eq!(dom.node(a).prev_sibling, None);
        assert_eq!(dom.node(b).next_sibling, None);
    }
}
