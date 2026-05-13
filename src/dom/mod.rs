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

    pub fn print(&self) {
        self.print_node(self.document, 0);
    }

    fn print_node(&self, id: NodeId, depth: usize) {
        let node = self.node(id);
        let indent = "  ".repeat(depth);
        match &node.kind {
            NodeKind::Document => println!("{indent}#document"),
            NodeKind::Element { tag, attrs } => {
                let mut attr_str = String::new();
                for (k, v) in attrs {
                    attr_str.push(' ');
                    attr_str.push_str(k);
                    attr_str.push('=');
                    attr_str.push_str(&format!("{v:?}"));
                }
                println!("{indent}<{tag}{attr_str}>");
            }
            NodeKind::Text(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    println!("{indent}\"{}\"", truncate(trimmed, 80));
                }
            }
            NodeKind::Comment(s) => {
                println!("{indent}<!--{}-->", truncate(s, 80));
            }
            NodeKind::Doctype(s) => {
                println!("{indent}<!DOCTYPE {s}>");
            }
        }
        let kids: Vec<NodeId> = self.children(id).collect();
        for child in kids {
            self.print_node(child, depth + 1);
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

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
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
