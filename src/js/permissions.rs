//! JS-facing Permissions Policy plumbing.
//!
//! Exposes `document.featurePolicy` (the older, browser-supported
//! name) and `document.permissionsPolicy` (the newer official name)
//! with the same shape:
//!
//! ```text
//! interface FeaturePolicy {
//!   boolean allowsFeature(DOMString feature);
//!   sequence<DOMString> features();
//!   sequence<DOMString> allowedFeatures();
//! };
//! ```
//!
//! The page loader calls [`set_policy`] with the parsed
//! `Permissions-Policy` header so JS observers see the actual rules
//! the response advertised.

use std::cell::RefCell;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction,
};

use crate::net::PermissionsPolicy;

thread_local! {
    pub(crate) static CURRENT_POLICY: RefCell<PermissionsPolicy> =
        RefCell::new(PermissionsPolicy::default());
}

/// Replace the current document's permissions policy. Called from
/// the page-load path after parsing the response header.
pub fn set_policy(policy: PermissionsPolicy) {
    CURRENT_POLICY.with(|p| *p.borrow_mut() = policy);
}

/// Build the JS-side `FeaturePolicy` object. The same object is
/// installed under both `document.featurePolicy` and
/// `document.permissionsPolicy`.
pub fn build_policy_object(ctx: &mut Context) -> JsObject {
    ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(allows_feature),
            js_string!("allowsFeature"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(features),
            js_string!("features"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(allowed_features),
            js_string!("allowedFeatures"),
            0,
        )
        .build()
}

fn allows_feature(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let name = name.to_string(ctx)?.to_std_string_escaped();
    let origin = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let allowed = CURRENT_POLICY.with(|p| p.borrow().allows(&name, origin.as_ref()));
    Ok(JsValue::from(allowed))
}

fn features(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // The spec-defined set of known feature names is open-ended; we
    // return the ones we actually implement so feature detection
    // round-trips. Anything not in this list still gets answered by
    // `allowsFeature` (it defaults to allowed).
    const KNOWN: &[&str] = &[
        "accelerometer",
        "ambient-light-sensor",
        "autoplay",
        "battery",
        "camera",
        "clipboard-read",
        "clipboard-write",
        "display-capture",
        "encrypted-media",
        "fullscreen",
        "gamepad",
        "geolocation",
        "gyroscope",
        "magnetometer",
        "microphone",
        "midi",
        "payment",
        "picture-in-picture",
        "publickey-credentials-get",
        "screen-wake-lock",
        "serial",
        "usb",
        "web-share",
        "xr-spatial-tracking",
    ];
    let arr = JsArray::new(ctx);
    for f in KNOWN {
        let _ = arr.push(JsValue::from(js_string!(*f)), ctx);
    }
    Ok(arr.into())
}

fn allowed_features(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let origin = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let arr = JsArray::new(ctx);
    CURRENT_POLICY.with(|p| {
        let p = p.borrow();
        const KNOWN: &[&str] = &[
            "accelerometer",
            "ambient-light-sensor",
            "autoplay",
            "battery",
            "camera",
            "clipboard-read",
            "clipboard-write",
            "display-capture",
            "encrypted-media",
            "fullscreen",
            "gamepad",
            "geolocation",
            "gyroscope",
            "magnetometer",
            "microphone",
            "midi",
            "payment",
            "picture-in-picture",
            "publickey-credentials-get",
            "screen-wake-lock",
            "serial",
            "usb",
            "web-share",
            "xr-spatial-tracking",
        ];
        for f in KNOWN {
            if p.allows(f, origin.as_ref()) {
                let _ = arr.push(JsValue::from(js_string!(*f)), ctx);
            }
        }
    });
    Ok(arr.into())
}

/// Convenience for Rust-side gating of feature-locked APIs (e.g.
/// `navigator.clipboard.readText`).
pub fn allows(feature: &str) -> bool {
    let origin = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    CURRENT_POLICY.with(|p| p.borrow().allows(feature, origin.as_ref()))
}
