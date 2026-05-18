//! Trusted Types — DOM-XSS injection-sink gate.
//!
//! Surface (spec-faithful subset):
//!   * `window.trustedTypes.createPolicy(name, { createHTML,
//!     createScript, createScriptURL })` — returns a policy object
//!     whose `createHTML(...)` / `createScript(...)` /
//!     `createScriptURL(...)` produce tagged "Trusted" objects.
//!   * `trustedTypes.isHTML(v)` / `isScript(v)` / `isScriptURL(v)`
//!     — predicate over a tagged object.
//!   * `trustedTypes.getPropertyType(tagName, propertyName)` —
//!     returns the trusted-type name expected for a given DOM
//!     sink, or `null`.
//!   * `trustedTypes.defaultPolicy` — the `name === "default"`
//!     policy, applied as a fallback when an injection sink
//!     receives a plain string under enforcement.
//!
//! Enforcement is gated on the page's CSP: when the document's
//! policy carries `require-trusted-types-for 'script'`, sinks
//! like `Element.innerHTML` must consume a TrustedHTML (or the
//! default policy's HTML projection) — assigning a raw string
//! throws.
//!
//! Tagging strategy: a Trusted value is a regular JS object with a
//! single non-configurable read-only property
//! `__trusted_kind: "TrustedHTML" | "TrustedScript" | "TrustedScriptURL"`
//! and a `__trusted_value: string`. `toString()` returns the
//! payload, so legacy code that string-coerces a Trusted value
//! still works.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction,
};

pub const KIND_KEY: &str = "__trusted_kind";
pub const VALUE_KEY: &str = "__trusted_value";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedKind {
    Html,
    Script,
    ScriptUrl,
}

impl TrustedKind {
    pub fn name(self) -> &'static str {
        match self {
            TrustedKind::Html => "TrustedHTML",
            TrustedKind::Script => "TrustedScript",
            TrustedKind::ScriptUrl => "TrustedScriptURL",
        }
    }
}

struct DefaultPolicy {
    create_html: Option<JsFunction>,
    create_script: Option<JsFunction>,
    create_script_url: Option<JsFunction>,
}

thread_local! {
    /// `true` when CSP enforced require-trusted-types-for 'script'
    /// on the document. Flipped by [`set_required`].
    pub(crate) static TT_REQUIRED: AtomicBool = const { AtomicBool::new(false) };
    /// Registered policies. We don't bother per-policy state beyond
    /// the default; non-default policies are returned to JS but we
    /// don't track them on the Rust side.
    static DEFAULT_POLICY: RefCell<Option<DefaultPolicy>> = const { RefCell::new(None) };
    /// Indirection table for user-supplied `createHTML` / friends.
    /// `from_copy_closure` requires `Copy` captures, so the wrapper
    /// closure captures a `u32` index and looks the function up here.
    static USER_FNS: RefCell<HashMap<u32, Option<JsFunction>>> =
        RefCell::new(HashMap::new());
    static NEXT_FN_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn register_user_fn(f: Option<JsFunction>) -> u32 {
    let id = NEXT_FN_ID.with(|n| {
        let mut v = n.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    });
    USER_FNS.with(|m| m.borrow_mut().insert(id, f));
    id
}

fn lookup_user_fn(id: u32) -> Option<JsFunction> {
    USER_FNS.with(|m| m.borrow().get(&id).cloned().flatten())
}

/// Configure whether Trusted Types enforcement is required for this
/// document. Driven by the document's CSP.
pub fn set_required(required: bool) {
    TT_REQUIRED.with(|f| f.store(required, Ordering::Relaxed));
}

pub fn is_required() -> bool {
    TT_REQUIRED.with(|f| f.load(Ordering::Relaxed))
}

/// Install the `trustedTypes` global on the JS context.
pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let create_policy = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(create_policy_fn),
    )
    .build();
    let is_html = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(is_html_fn),
    )
    .build();
    let is_script = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(is_script_fn),
    )
    .build();
    let is_script_url = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(is_script_url_fn),
    )
    .build();
    let get_prop_type = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(get_property_type_fn),
    )
    .build();
    let empty_html = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(empty_html_fn),
    )
    .build();
    let empty_script = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(empty_script_fn),
    )
    .build();
    let trusted_types = ObjectInitializer::new(ctx)
        .property(
            js_string!("createPolicy"),
            JsValue::from(create_policy),
            Attribute::READONLY,
        )
        .property(
            js_string!("isHTML"),
            JsValue::from(is_html),
            Attribute::READONLY,
        )
        .property(
            js_string!("isScript"),
            JsValue::from(is_script),
            Attribute::READONLY,
        )
        .property(
            js_string!("isScriptURL"),
            JsValue::from(is_script_url),
            Attribute::READONLY,
        )
        .property(
            js_string!("getPropertyType"),
            JsValue::from(get_prop_type),
            Attribute::READONLY,
        )
        .property(
            js_string!("emptyHTML"),
            JsValue::from(empty_html),
            Attribute::READONLY,
        )
        .property(
            js_string!("emptyScript"),
            JsValue::from(empty_script),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("trustedTypes"),
        trusted_types,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

// ============ createPolicy ============

fn create_policy_fn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let options = args.get(1).and_then(|v| v.as_object().cloned());
    // Pull the three callbacks if present and store the default
    // policy if the name is "default".
    let mk_html = options
        .as_ref()
        .and_then(|o| o.get(js_string!("createHTML"), ctx).ok())
        .and_then(|v| v.as_object().and_then(|o| JsFunction::from_object(o.clone())));
    let mk_script = options
        .as_ref()
        .and_then(|o| o.get(js_string!("createScript"), ctx).ok())
        .and_then(|v| v.as_object().and_then(|o| JsFunction::from_object(o.clone())));
    let mk_script_url = options
        .as_ref()
        .and_then(|o| o.get(js_string!("createScriptURL"), ctx).ok())
        .and_then(|v| v.as_object().and_then(|o| JsFunction::from_object(o.clone())));
    if name == "default" {
        DEFAULT_POLICY.with(|slot| {
            *slot.borrow_mut() = Some(DefaultPolicy {
                create_html: mk_html.clone(),
                create_script: mk_script.clone(),
                create_script_url: mk_script_url.clone(),
            });
        });
    }
    // Build the policy object exposed to JS. We wire the
    // callbacks via a closure-bound NativeFunction so each policy
    // remembers its own create*() handlers.
    let policy = build_policy_object(ctx, &name, mk_html, mk_script, mk_script_url);
    Ok(JsValue::from(policy))
}

fn build_policy_object(
    ctx: &mut Context,
    name: &str,
    mk_html: Option<JsFunction>,
    mk_script: Option<JsFunction>,
    mk_script_url: Option<JsFunction>,
) -> JsObject {
    let realm = ctx.realm().clone();
    let create_html = build_wrapper(&realm, mk_html, TrustedKind::Html);
    let create_script = build_wrapper(&realm, mk_script, TrustedKind::Script);
    let create_script_url = build_wrapper(&realm, mk_script_url, TrustedKind::ScriptUrl);
    ObjectInitializer::new(ctx)
        .property(
            js_string!("name"),
            JsValue::from(js_string!(name.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("createHTML"),
            JsValue::from(create_html),
            Attribute::READONLY,
        )
        .property(
            js_string!("createScript"),
            JsValue::from(create_script),
            Attribute::READONLY,
        )
        .property(
            js_string!("createScriptURL"),
            JsValue::from(create_script_url),
            Attribute::READONLY,
        )
        .build()
}

fn build_wrapper(
    realm: &boa_engine::realm::Realm,
    user_fn: Option<JsFunction>,
    kind: TrustedKind,
) -> JsFunction {
    // Capture identity by `Copy` types only: an id into the user-fn
    // table and a u8 kind discriminant. `from_copy_closure` requires
    // this; storing the function or owning a String would fail to
    // compile.
    let fn_id = register_user_fn(user_fn);
    let kind_disc: u8 = match kind {
        TrustedKind::Html => 0,
        TrustedKind::Script => 1,
        TrustedKind::ScriptUrl => 2,
    };
    boa_engine::object::FunctionObjectBuilder::new(
        realm,
        NativeFunction::from_copy_closure(move |_this, args, ctx| {
            let input = args.first().cloned().unwrap_or(JsValue::undefined());
            let projected = if let Some(f) = lookup_user_fn(fn_id) {
                f.call(&JsValue::undefined(), std::slice::from_ref(&input), ctx)?
            } else {
                input
            };
            let s = projected.to_string(ctx)?.to_std_string_escaped();
            let kind = match kind_disc {
                0 => TrustedKind::Html,
                1 => TrustedKind::Script,
                _ => TrustedKind::ScriptUrl,
            };
            Ok(JsValue::from(make_trusted(ctx, kind.name(), &s)))
        }),
    )
    .build()
}

fn make_trusted(ctx: &mut Context, kind_name: &str, value: &str) -> JsObject {
    ObjectInitializer::new(ctx)
        .property(
            js_string!(KIND_KEY),
            JsValue::from(js_string!(kind_name.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!(VALUE_KEY),
            JsValue::from(js_string!(value.to_string())),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(to_string_fn),
            js_string!("toString"),
            0,
        )
        .build()
}

fn to_string_fn(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(obj) = this.as_object() {
        if let Ok(v) = obj.get(js_string!(VALUE_KEY), ctx) {
            return Ok(v);
        }
    }
    Ok(JsValue::from(js_string!("")))
}

// ============ predicates ============

fn predicate(val: &JsValue, want: &str, ctx: &mut Context) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    let Ok(kind) = obj.get(js_string!(KIND_KEY), ctx) else {
        return false;
    };
    let Ok(s) = kind.to_string(ctx) else { return false };
    s.to_std_string_escaped() == want
}

fn is_html_fn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.first().cloned().unwrap_or(JsValue::undefined());
    Ok(JsValue::from(predicate(&v, "TrustedHTML", ctx)))
}

fn is_script_fn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.first().cloned().unwrap_or(JsValue::undefined());
    Ok(JsValue::from(predicate(&v, "TrustedScript", ctx)))
}

fn is_script_url_fn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.first().cloned().unwrap_or(JsValue::undefined());
    Ok(JsValue::from(predicate(&v, "TrustedScriptURL", ctx)))
}

fn get_property_type_fn(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let tag = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default()
        .to_ascii_lowercase();
    let prop = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // Per the IDL [Default-Policy]: trustedTypes.getPropertyType
    // tells consumers which Trusted Type a given DOM sink demands.
    // The full table is large; cover the high-traffic sinks.
    let trusted = match (tag.as_str(), prop.as_str()) {
        (_, "innerHTML") | (_, "outerHTML") | ("iframe", "srcdoc") => Some("TrustedHTML"),
        ("script", "src") => Some("TrustedScriptURL"),
        ("script", "text") | ("script", "textContent") | ("script", "innerText") => {
            Some("TrustedScript")
        }
        _ => None,
    };
    Ok(match trusted {
        Some(s) => JsValue::from(js_string!(s)),
        None => JsValue::null(),
    })
}

fn empty_html_fn(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(make_trusted(ctx, "TrustedHTML", "")))
}

fn empty_script_fn(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(make_trusted(ctx, "TrustedScript", "")))
}

// ============ sink helpers (called from DOM bindings) ============

/// Resolve a value passed to an injection sink that expects
/// `TrustedHTML`. Returns the underlying string.
///
/// Behaviour:
/// * Always accept a tagged `TrustedHTML`.
/// * Under enforcement, fall back to the default policy's
///   `createHTML` if registered; otherwise throw.
/// * Without enforcement, accept any string.
pub fn resolve_html_sink(val: &JsValue, ctx: &mut Context) -> JsResult<String> {
    if predicate(val, "TrustedHTML", ctx) {
        return Ok(extract_value(val, ctx));
    }
    if !is_required() {
        return Ok(val.to_string(ctx)?.to_std_string_escaped());
    }
    // Enforcement on. Try the default policy.
    let fallback = DEFAULT_POLICY.with(|slot| slot.borrow().as_ref().and_then(|p| p.create_html.clone()));
    if let Some(f) = fallback {
        let projected = f.call(&JsValue::undefined(), std::slice::from_ref(val), ctx)?;
        return Ok(projected.to_string(ctx)?.to_std_string_escaped());
    }
    Err(boa_engine::JsNativeError::typ()
        .with_message(
            "This document requires 'TrustedHTML' assignment (CSP require-trusted-types-for 'script')",
        )
        .into())
}

fn extract_value(val: &JsValue, ctx: &mut Context) -> String {
    val.as_object()
        .and_then(|o| o.get(js_string!(VALUE_KEY), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_spec() {
        assert_eq!(TrustedKind::Html.name(), "TrustedHTML");
        assert_eq!(TrustedKind::Script.name(), "TrustedScript");
        assert_eq!(TrustedKind::ScriptUrl.name(), "TrustedScriptURL");
    }

    #[test]
    fn required_flag_round_trips() {
        let prev = is_required();
        set_required(true);
        assert!(is_required());
        set_required(false);
        assert!(!is_required());
        set_required(prev);
    }
}
