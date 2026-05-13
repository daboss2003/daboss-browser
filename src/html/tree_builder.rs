//! Tree builder: turn a token stream into an arena DOM.
//!
//! Pragmatic implementation, not spec-faithful. We pre-create
//! `<html><head></head><body></body></html>` and route each token to the
//! right place. Head-only tags before any body content go in `<head>`;
//! everything else flips us into `<body>` (matching the implicit-insertion
//! behavior real HTML pages rely on when they omit those tags).

use super::tokenizer::Token;
use crate::dom::{Dom, NodeId, NodeKind};

pub fn build(tokens: impl IntoIterator<Item = Token>) -> Dom {
    let mut tb = TreeBuilder::new();
    for token in tokens {
        tb.process(token);
    }
    tb.dom
}

struct TreeBuilder {
    dom: Dom,
    stack: Vec<NodeId>,
    head: NodeId,
    body: NodeId,
    in_head: bool,
}

impl TreeBuilder {
    fn new() -> Self {
        let mut dom = Dom::new();
        let doc = dom.document();
        let html = dom.create_element("html", Vec::new());
        let head = dom.create_element("head", Vec::new());
        let body = dom.create_element("body", Vec::new());
        dom.append_child(doc, html);
        dom.append_child(html, head);
        dom.append_child(html, body);
        Self {
            dom,
            stack: vec![doc, html, head],
            head,
            body,
            in_head: true,
        }
    }

    fn current(&self) -> NodeId {
        *self.stack.last().expect("stack never empty")
    }

    /// True when the immediate insertion point is `<head>` itself —
    /// i.e. not inside a head element like `<title>` or `<style>`.
    fn at_head_level(&self) -> bool {
        self.in_head && self.current() == self.head
    }

    fn process(&mut self, token: Token) {
        match token {
            Token::Doctype { name } => {
                let doc = self.dom.document();
                let n = self.dom.create_doctype(name);
                // Doctype belongs before <html>. Insert as first child of document.
                match self.dom.node(doc).first_child {
                    Some(first) => self.dom.insert_before(doc, n, first),
                    None => self.dom.append_child(doc, n),
                }
            }
            Token::Comment(text) => {
                let parent = self.current();
                let n = self.dom.create_comment(text);
                self.dom.append_child(parent, n);
            }
            Token::Text(content) => {
                if self.at_head_level() {
                    if content.trim().is_empty() {
                        return; // drop whitespace between head-level tags
                    }
                    self.move_to_body();
                }
                let parent = self.current();
                let n = self.dom.create_text(content);
                self.dom.append_child(parent, n);
            }
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => self.handle_start(name, attrs, self_closing),
            Token::EndTag { name } => self.handle_end(&name),
        }
    }

    fn handle_start(&mut self, name: String, attrs: Vec<(String, String)>, self_closing: bool) {
        match name.as_str() {
            "html" => return, // already created
            "head" => {
                if !self.in_head {
                    self.in_head = true;
                    self.stack.truncate(2);
                    self.stack.push(self.head);
                }
                return;
            }
            "body" => {
                self.move_to_body();
                // Merge any incoming attributes onto the body. Toy: ignore.
                let _ = attrs;
                return;
            }
            "title" | "meta" | "link" | "base" | "style" | "script" if self.at_head_level() => {
                // Stay in head.
            }
            _ => {
                if self.at_head_level() {
                    self.move_to_body();
                }
            }
        }

        let parent = self.current();
        let elem = self.dom.create_element(name.clone(), attrs);
        self.dom.append_child(parent, elem);
        if !is_void(&name) && !self_closing {
            self.stack.push(elem);
        }
    }

    fn handle_end(&mut self, name: &str) {
        match name {
            "html" | "body" => return,
            "head" => {
                if self.in_head {
                    self.in_head = false;
                    self.stack.truncate(2); // [doc, html]
                    self.stack.push(self.body);
                }
                return;
            }
            _ => {}
        }
        if let Some(idx) = self.stack.iter().rposition(|&id| match &self.dom.node(id).kind {
            NodeKind::Element { tag, .. } => tag == name,
            _ => false,
        }) {
            // Pop everything from idx onwards (inclusive of the matched element).
            self.stack.truncate(idx);
        }
        // Unmatched end tags are ignored.
    }

    fn move_to_body(&mut self) {
        self.in_head = false;
        self.stack.truncate(2); // [doc, html]
        self.stack.push(self.body);
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::tokenizer::Tokenizer;

    fn parse(input: &str) -> Dom {
        build(Tokenizer::new(input).tokenize())
    }

    fn find_first(dom: &Dom, root: NodeId, tag: &str) -> Option<NodeId> {
        if let NodeKind::Element { tag: t, .. } = &dom.node(root).kind {
            if t == tag {
                return Some(root);
            }
        }
        for child in dom.children(root).collect::<Vec<_>>() {
            if let Some(found) = find_first(dom, child, tag) {
                return Some(found);
            }
        }
        None
    }

    fn tag_of(dom: &Dom, id: NodeId) -> &str {
        match &dom.node(id).kind {
            NodeKind::Element { tag, .. } => tag,
            _ => panic!("not an element"),
        }
    }

    #[test]
    fn implicit_html_head_body() {
        let dom = parse("<p>hi</p>");
        let html = find_first(&dom, dom.document(), "html").unwrap();
        let body = find_first(&dom, html, "body").unwrap();
        let p = find_first(&dom, body, "p").unwrap();
        assert_eq!(tag_of(&dom, p), "p");
    }

    #[test]
    fn meta_goes_in_head() {
        let dom = parse(r#"<meta charset="utf-8"><p>hi</p>"#);
        let head = find_first(&dom, dom.document(), "head").unwrap();
        let meta = find_first(&dom, head, "meta").unwrap();
        assert_eq!(tag_of(&dom, meta), "meta");
        // p is in body, not head
        assert!(find_first(&dom, head, "p").is_none());
    }

    #[test]
    fn void_elements_dont_nest_children() {
        let dom = parse("<br><p>hi</p>");
        let body = find_first(&dom, dom.document(), "body").unwrap();
        let body_kids: Vec<NodeId> = dom.children(body).collect();
        assert_eq!(body_kids.len(), 2);
        assert_eq!(tag_of(&dom, body_kids[0]), "br");
        assert_eq!(tag_of(&dom, body_kids[1]), "p");
    }

    #[test]
    fn mismatched_close_pops_to_match() {
        let dom = parse("<div><span><b>x</div>");
        let body = find_first(&dom, dom.document(), "body").unwrap();
        let body_kids: Vec<NodeId> = dom.children(body).collect();
        // After </div>, stack should be back at body.
        assert_eq!(body_kids.len(), 1);
        assert_eq!(tag_of(&dom, body_kids[0]), "div");
    }

    #[test]
    fn full_document() {
        let dom = parse(
            "<!DOCTYPE html>\
             <html><head><title>T</title></head>\
             <body><h1>Hello</h1><p>World</p></body></html>",
        );
        let body = find_first(&dom, dom.document(), "body").unwrap();
        let body_kids: Vec<NodeId> = dom.children(body).collect();
        assert_eq!(body_kids.len(), 2);
        assert_eq!(tag_of(&dom, body_kids[0]), "h1");
        assert_eq!(tag_of(&dom, body_kids[1]), "p");
    }
}
