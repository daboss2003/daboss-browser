//! JavaScript subsystem (Phase 7a + 7b).
//!
//! Embeds the `boa` engine and runs every inline `<script>` body from
//! the parsed DOM in a single JS context per page. Exposes:
//!
//!  * `console.log/warn/error/debug/info` — prefix-and-print to stderr.
//!  * `document.title`, `document.body`, `document.documentElement` — root
//!    handles into the DOM.
//!  * `document.getElementById(id)` — first matching element or `null`.
//!  * `Element.tagName`, `.id`, `.className`, `.textContent` — snapshotted
//!    at handle-creation time.
//!  * `Element.getAttribute(name)` / `.hasAttribute(name)` — read attrs.
//!  * `Element.parentElement` — walk up the tree.
//!
//! Not yet implemented (later sub-phases):
//!  * `document.querySelector` / `querySelectorAll`
//!  * DOM mutation (`textContent =`, `setAttribute`, `appendChild`)
//!  * Event listeners (`element.addEventListener`)
//!  * `setTimeout` / `setInterval` and the microtask queue
//!  * `fetch` / XMLHttpRequest
//!  * External `<script src="...">` (we skip those for now)
//!
//! Security stance: `boa` is pure Rust with no FFI to native code or the
//! filesystem. Scripts can only do CPU-bounded work and use the host
//! functions we explicitly install. With no DOM mutation yet, the worst a
//! malicious script can do is loop forever (no script timeout yet — that's
//! a known TODO).
//!
//! Dom sharing: the parsed `Dom` is moved into a per-thread `JS_DOM` slot
//! (`Rc<RefCell<Dom>>`) for the duration of `run_inline_scripts`. JS
//! callbacks pick it up via [`with_dom`]. When scripts finish, the handle
//! is dropped and the `Dom` is moved back out into the caller's `&mut Dom`.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsResult, JsValue,
    NativeFunction,
};

use crate::dom::{Dom, NodeId, NodeKind};

pub(crate) mod canvas;
pub(crate) mod dom;
pub mod engine;
pub(crate) mod observers;
pub(crate) mod rtc;
pub(crate) mod storage;
pub(crate) mod web_classes;
pub(crate) mod xhr;

pub use canvas::CanvasSurfaces;

/// Shared registry of `<audio>` elements per page. The browser
/// pre-decodes each audio source during navigate and stashes the
/// resulting `AudioElement` here; the JS shims for `play()` /
/// `pause()` / `currentTime` etc. look up by `NodeId`.
pub type AudioElements =
    std::rc::Rc<std::cell::RefCell<std::collections::HashMap<crate::dom::NodeId, crate::audio::AudioElement>>>;

/// Same shape as [`AudioElements`] but for `<video>`. The decoder
/// thread is owned by each `VideoElement`; paint pulls the latest
/// frame for composite.
pub type VideoElements =
    std::rc::Rc<std::cell::RefCell<std::collections::HashMap<crate::dom::NodeId, crate::video::VideoElement>>>;

pub use engine::JsEngine;
pub use storage::StorageArea;

thread_local! {
    /// Active DOM during script execution. Set by [`run_inline_scripts`]
    /// or `JsEngine` for the lifetime of the inner block, then cleared.
    pub(crate) static JS_DOM: RefCell<Option<Rc<RefCell<Dom>>>> = const { RefCell::new(None) };
}

/// Borrow the active DOM inside a JS callback. Returns `None` if no
/// scripts are currently running (which would be a logic error inside the
/// JS module, not a runtime condition).
pub(crate) fn with_dom<R>(f: impl FnOnce(&Dom) -> R) -> Option<R> {
    JS_DOM.with(|slot| {
        let guard = slot.borrow();
        let rc = guard.as_ref()?.clone();
        drop(guard);
        let dom = rc.borrow();
        Some(f(&dom))
    })
}

/// Mutable variant of [`with_dom`]. Used by setters and methods that
/// modify the Dom (setAttribute, textContent assignment, ...).
pub(crate) fn with_dom_mut<R>(f: impl FnOnce(&mut Dom) -> R) -> Option<R> {
    JS_DOM.with(|slot| {
        let guard = slot.borrow();
        let rc = guard.as_ref()?.clone();
        drop(guard);
        let mut dom = rc.borrow_mut();
        Some(f(&mut dom))
    })
}

/// Build a fresh [`JsEngine`] for a page and run its inline `<script>`s.
/// The engine survives so that later event dispatches (clicks, etc.) can
/// invoke any handlers the scripts registered via `addEventListener`.
pub fn run_inline_scripts(dom: &mut Dom) -> JsEngine {
    JsEngine::new(dom)
}

pub(crate) fn install_console(ctx: &mut Context) {
    fn print_args(level: &str, args: &[JsValue]) {
        let mut buf = String::new();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                buf.push(' ');
            }
            buf.push_str(&a.display().to_string());
        }
        eprintln!("[js {level}] {buf}");
    }
    fn log(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        print_args("log", args);
        Ok(JsValue::undefined())
    }
    fn warn(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        print_args("warn", args);
        Ok(JsValue::undefined())
    }
    fn error(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        print_args("error", args);
        Ok(JsValue::undefined())
    }
    fn debug(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        print_args("debug", args);
        Ok(JsValue::undefined())
    }
    fn info(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        print_args("info", args);
        Ok(JsValue::undefined())
    }
    let console = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(log), js_string!("log"), 1)
        .function(NativeFunction::from_fn_ptr(warn), js_string!("warn"), 1)
        .function(NativeFunction::from_fn_ptr(error), js_string!("error"), 1)
        .function(NativeFunction::from_fn_ptr(debug), js_string!("debug"), 1)
        .function(NativeFunction::from_fn_ptr(info), js_string!("info"), 1)
        .build();
    ctx.register_global_property(js_string!("console"), console, Attribute::all())
        .ok();
}

pub(crate) fn collect_inline_scripts(dom: &Dom) -> Vec<String> {
    let mut out = Vec::new();
    walk(dom, dom.document(), &mut out);
    out
}

fn walk(dom: &Dom, node: NodeId, out: &mut Vec<String>) {
    if let NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "script" {
            // Skip external scripts — we don't fetch them yet.
            let has_src = attrs.iter().any(|(k, _)| k == "src");
            // Skip non-JS scripts (`type="application/ld+json"` etc.).
            let type_ok = attrs
                .iter()
                .find(|(k, _)| k == "type")
                .map(|(_, v)| {
                    let t = v.to_ascii_lowercase();
                    t.is_empty()
                        || t == "text/javascript"
                        || t == "application/javascript"
                        || t == "module"
                })
                .unwrap_or(true);
            if !has_src && type_ok {
                let mut body = String::new();
                for c in dom.children(node).collect::<Vec<_>>() {
                    if let NodeKind::Text(t) = &dom.node(c).kind {
                        body.push_str(t);
                    }
                }
                if !body.trim().is_empty() {
                    out.push(body);
                }
            }
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for c in kids {
        walk(dom, c, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn finds_inline_script() {
        let mut dom = html::parse("<html><body><script>1+1</script></body></html>");
        let scripts = collect_inline_scripts(&dom);
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("1+1"));
        // Doesn't panic when run.
        let _engine = run_inline_scripts(&mut dom);
    }

    #[test]
    fn skips_external_script() {
        let dom = html::parse(r#"<html><body><script src="x.js"></script></body></html>"#);
        let scripts = collect_inline_scripts(&dom);
        assert!(scripts.is_empty());
    }

    #[test]
    fn skips_non_js_type() {
        let dom = html::parse(
            r#"<html><body><script type="application/ld+json">{}</script></body></html>"#,
        );
        let scripts = collect_inline_scripts(&dom);
        assert!(scripts.is_empty());
    }

    #[test]
    fn dom_is_returned_unchanged() {
        let mut dom = html::parse("<html><body><div id=x>hi</div><script>1</script></body></html>");
        let before_len = collect_inline_scripts(&dom).len();
        let _engine = run_inline_scripts(&mut dom);
        // After run, dom should still have the same scripts (we don't strip them).
        let after_len = collect_inline_scripts(&dom).len();
        assert_eq!(before_len, after_len);
        // And the rest of the tree survived.
        let mut found_div = false;
        for c in dom.children(dom.document()).collect::<Vec<_>>() {
            walk_check(&dom, c, &mut found_div);
        }
        assert!(found_div);
    }

    fn walk_check(dom: &Dom, node: NodeId, hit: &mut bool) {
        if let NodeKind::Element { tag, attrs } = &dom.node(node).kind {
            if tag == "div" && attrs.iter().any(|(k, v)| k == "id" && v == "x") {
                *hit = true;
            }
        }
        for c in dom.children(node).collect::<Vec<_>>() {
            walk_check(dom, c, hit);
        }
    }
}
