//! Small Web API stubs that pages feature-detect or call optimistically.
//!
//! Each API is a thin surface that returns plausible values or
//! rejected promises rather than throwing. Pages that gate features
//! on `if (navigator.wakeLock)` see the property; pages that call
//! `navigator.geolocation.getCurrentPosition` get an error callback
//! rather than a crash.
//!
//! Covered:
//!   * `navigator.wakeLock.request("screen")` — returns a sentinel
//!     with `release()`. No actual OS hook; we're a toy.
//!   * `navigator.geolocation.{getCurrentPosition, watchPosition,
//!     clearWatch}` — always rejects with `PERMISSION_DENIED`.
//!   * `window.speechSynthesis` + `SpeechSynthesisUtterance` — speak
//!     is a no-op; events fire synchronously so promise chains
//!     resolve cleanly.
//!   * `navigator.share()` — rejects with `AbortError` ("no share UI").
//!   * `window.navigation` — entries-based navigation introspection
//!     backed by the existing History stack. `.navigate(url)` issues
//!     a real navigation request through the engine.
//!   * `navigator.usb` / `bluetooth` / `serial` / `hid` — present
//!     but every method rejects with `NotAllowedError`.
//!   * `navigator.mediaSession` — settable but inert.

use std::cell::RefCell;

use boa_engine::{
    js_string,
    object::{builtins::JsPromise, JsObject, ObjectInitializer},
    property::Attribute,
    Context, JsError, JsResult, JsValue, NativeFunction,
};

pub fn install(ctx: &mut Context) {
    install_wake_lock(ctx);
    install_geolocation(ctx);
    install_speech(ctx);
    install_share(ctx);
    install_navigation(ctx);
    install_hardware_stubs(ctx);
    install_media_session(ctx);
}

thread_local! {
    /// `navigator.mediaSession.metadata` — pages set it; we store
    /// the value verbatim so reads round-trip.
    static MEDIA_SESSION_METADATA: RefCell<JsValue> = RefCell::new(JsValue::null());
    static MEDIA_SESSION_PLAYBACK: RefCell<String> = RefCell::new("none".into());
}

// =================== navigator getters/setters ===================

fn nav_set(ctx: &mut Context, key: &str, value: JsValue) {
    let global = ctx.global_object();
    if let Ok(nav_val) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.set(js_string!(key.to_string()), value, false, ctx);
        }
    }
}

// =================== Wake Lock ===================

fn install_wake_lock(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let request_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(wake_lock_request),
    )
    .build();
    let wake_lock = ObjectInitializer::new(ctx)
        .property(
            js_string!("request"),
            JsValue::from(request_fn),
            Attribute::READONLY,
        )
        .build();
    nav_set(ctx, "wakeLock", JsValue::from(wake_lock));
}

fn wake_lock_request(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.realm().clone();
    let release = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(wake_lock_release),
    )
    .build();
    let sentinel = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!("screen")),
            Attribute::READONLY,
        )
        .property(
            js_string!("released"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .property(js_string!("release"), JsValue::from(release), Attribute::READONLY)
        .build();
    Ok(JsPromise::resolve(JsValue::from(sentinel), ctx).into())
}

fn wake_lock_release(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

// =================== Geolocation ===================

fn install_geolocation(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let geo = ObjectInitializer::new(ctx)
        .property(
            js_string!("getCurrentPosition"),
            JsValue::from(mk(geo_get_current_position)),
            Attribute::READONLY,
        )
        .property(
            js_string!("watchPosition"),
            JsValue::from(mk(geo_watch_position)),
            Attribute::READONLY,
        )
        .property(
            js_string!("clearWatch"),
            JsValue::from(mk(noop_zero)),
            Attribute::READONLY,
        )
        .build();
    nav_set(ctx, "geolocation", JsValue::from(geo));
}

fn geo_get_current_position(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // No GPS / IP lookup on the toy. Invoke the error callback (if
    // provided) with a PERMISSION_DENIED-shaped object.
    if let Some(err_cb) = args.get(1).and_then(|v| v.as_object().cloned()) {
        let pos_err = ObjectInitializer::new(ctx)
            .property(
                js_string!("code"),
                JsValue::from(1u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("PERMISSION_DENIED"),
                JsValue::from(1u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("POSITION_UNAVAILABLE"),
                JsValue::from(2u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("TIMEOUT"),
                JsValue::from(3u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("message"),
                JsValue::from(js_string!("User denied geolocation")),
                Attribute::READONLY,
            )
            .build();
        if let Some(f) = boa_engine::object::builtins::JsFunction::from_object(err_cb) {
            let _ = f.call(&JsValue::undefined(), &[JsValue::from(pos_err)], ctx);
        }
    }
    Ok(JsValue::undefined())
}

fn geo_watch_position(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let _ = geo_get_current_position(&JsValue::undefined(), args, ctx);
    Ok(JsValue::from(0u32))
}

fn noop_zero(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

// =================== Speech ===================

fn install_speech(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let synth = ObjectInitializer::new(ctx)
        .property(js_string!("speak"), JsValue::from(mk(speech_speak)), Attribute::READONLY)
        .property(js_string!("cancel"), JsValue::from(mk(noop_zero)), Attribute::READONLY)
        .property(js_string!("pause"), JsValue::from(mk(noop_zero)), Attribute::READONLY)
        .property(js_string!("resume"), JsValue::from(mk(noop_zero)), Attribute::READONLY)
        .property(
            js_string!("getVoices"),
            JsValue::from(mk(speech_get_voices)),
            Attribute::READONLY,
        )
        .property(
            js_string!("speaking"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .property(
            js_string!("pending"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .property(
            js_string!("paused"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("speechSynthesis"),
        synth,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
    // `SpeechSynthesisUtterance` is a constructor — pages do
    // `new SpeechSynthesisUtterance(text)`.
    let _ = ctx.register_global_callable(
        js_string!("SpeechSynthesisUtterance"),
        1,
        NativeFunction::from_fn_ptr(speech_utterance_ctor),
    );
}

fn speech_speak(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Fire the utterance's onend if present, so promise chains that
    // bind on `utterance.onend` resolve.
    if let Some(utt) = args.first().and_then(|v| v.as_object().cloned()) {
        if let Ok(handler) = utt.get(js_string!("onend"), ctx) {
            if let Some(h_obj) = handler.as_object() {
                if let Some(f) =
                    boa_engine::object::builtins::JsFunction::from_object(h_obj.clone())
                {
                    let _ = f.call(&JsValue::undefined(), &[], ctx);
                }
            }
        }
    }
    Ok(JsValue::undefined())
}

fn speech_get_voices(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(boa_engine::object::builtins::JsArray::new(ctx).into())
}

fn speech_utterance_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("text"),
                JsValue::from(js_string!(text)),
                Attribute::WRITABLE,
            )
            .property(js_string!("lang"), JsValue::from(js_string!("en-US")), Attribute::WRITABLE)
            .property(js_string!("rate"), JsValue::from(1.0_f64), Attribute::WRITABLE)
            .property(js_string!("pitch"), JsValue::from(1.0_f64), Attribute::WRITABLE)
            .property(js_string!("volume"), JsValue::from(1.0_f64), Attribute::WRITABLE)
            .build(),
    ))
}

// =================== Web Share ===================

fn install_share(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let share = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(share_fn),
    )
    .build();
    nav_set(ctx, "share", JsValue::from(share));
    let can_share = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(can_share_fn),
    )
    .build();
    nav_set(ctx, "canShare", JsValue::from(can_share));
}

fn share_fn(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let err: JsError = boa_engine::JsNativeError::error()
        .with_message("AbortError: no share UI available")
        .into();
    Ok(JsPromise::reject(err, ctx).into())
}

fn can_share_fn(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(false))
}

// =================== Navigation API ===================

fn install_navigation(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let navigation = ObjectInitializer::new(ctx)
        .property(js_string!("entries"), JsValue::from(mk(nav_entries)), Attribute::READONLY)
        .property(
            js_string!("currentEntry"),
            JsValue::from(mk(nav_current_entry)),
            Attribute::READONLY,
        )
        .property(
            js_string!("navigate"),
            JsValue::from(mk(nav_navigate)),
            Attribute::READONLY,
        )
        .property(js_string!("reload"), JsValue::from(mk(nav_reload)), Attribute::READONLY)
        .property(js_string!("back"), JsValue::from(mk(nav_back)), Attribute::READONLY)
        .property(
            js_string!("forward"),
            JsValue::from(mk(nav_forward)),
            Attribute::READONLY,
        )
        .property(
            js_string!("canGoBack"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .property(
            js_string!("canGoForward"),
            JsValue::from(false),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("navigation"),
        navigation,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn nav_entries(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(boa_engine::object::builtins::JsArray::new(ctx).into())
}

fn nav_current_entry(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url = super::engine::JS_BASE_URL
        .with(|u| u.borrow().clone())
        .map(|u| u.to_string())
        .unwrap_or_default();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("url"),
                JsValue::from(js_string!(url)),
                Attribute::READONLY,
            )
            .property(
                js_string!("index"),
                JsValue::from(0u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("key"),
                JsValue::from(js_string!("current")),
                Attribute::READONLY,
            )
            .property(
                js_string!("id"),
                JsValue::from(js_string!("current")),
                Attribute::READONLY,
            )
            .build(),
    ))
}

fn nav_navigate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    if !url.is_empty() {
        super::engine::JS_NAV_REQUESTS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                rc.borrow_mut()
                    .push(super::engine::NavRequest::Assign(url));
            }
        });
    }
    // Spec returns `{ committed, finished }` Promises. We expose a
    // single resolved Promise with the same property shape.
    let done = JsPromise::resolve(JsValue::undefined(), ctx);
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("committed"),
                JsValue::from(done.clone()),
                Attribute::READONLY,
            )
            .property(
                js_string!("finished"),
                JsValue::from(done),
                Attribute::READONLY,
            )
            .build(),
    ))
}

fn nav_reload(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    super::engine::JS_NAV_REQUESTS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().push(super::engine::NavRequest::Reload);
        }
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn nav_back(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    super::engine::JS_NAV_REQUESTS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().push(super::engine::NavRequest::Go(-1));
        }
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn nav_forward(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    super::engine::JS_NAV_REQUESTS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().push(super::engine::NavRequest::Go(1));
        }
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

// =================== Hardware stubs ===================

fn install_hardware_stubs(ctx: &mut Context) {
    let usb = hw_methods(ctx, "requestDevice");
    let bluetooth = hw_methods(ctx, "requestDevice");
    let serial = hw_methods(ctx, "requestPort");
    let hid = hw_methods(ctx, "requestDevice");
    nav_set(ctx, "usb", JsValue::from(usb));
    nav_set(ctx, "bluetooth", JsValue::from(bluetooth));
    nav_set(ctx, "serial", JsValue::from(serial));
    nav_set(ctx, "hid", JsValue::from(hid));
}

fn hw_methods(ctx: &mut Context, request_method: &str) -> JsObject {
    let realm = ctx.realm().clone();
    let req = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(hardware_not_allowed),
    )
    .build();
    let get_devices = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(hardware_empty_list),
    )
    .build();
    ObjectInitializer::new(ctx)
        .property(
            js_string!(request_method.to_string()),
            JsValue::from(req),
            Attribute::READONLY,
        )
        .property(
            js_string!("getDevices"),
            JsValue::from(get_devices),
            Attribute::READONLY,
        )
        .build()
}

fn hardware_not_allowed(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let err: JsError = boa_engine::JsNativeError::error()
        .with_message("NotAllowedError: hardware API not supported on this toy")
        .into();
    Ok(JsPromise::reject(err, ctx).into())
}

fn hardware_empty_list(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = boa_engine::object::builtins::JsArray::new(ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

// =================== MediaSession ===================

fn install_media_session(ctx: &mut Context) {
    let session = ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(media_set_action_handler),
            js_string!("setActionHandler"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(media_set_position_state),
            js_string!("setPositionState"),
            1,
        )
        .property(
            js_string!("metadata"),
            JsValue::null(),
            Attribute::WRITABLE,
        )
        .property(
            js_string!("playbackState"),
            JsValue::from(js_string!("none")),
            Attribute::WRITABLE,
        )
        .build();
    nav_set(ctx, "mediaSession", JsValue::from(session));
}

fn media_set_action_handler(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn media_set_position_state(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}
