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
    install_notifications(ctx);
    install_push(ctx);
    install_background_sync(ctx);
    install_web_transport(ctx);
    install_picture_in_picture(ctx);
    install_payment_request(ctx);
    install_web_locks(ctx);
    install_compute_pressure(ctx);
    install_idle_detector(ctx);
    install_storage_buckets(ctx);
    install_document_pip(ctx);
    install_webxr(ctx);
    install_css_houdini(ctx);
    install_webextensions_stub(ctx);
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

// =================== Notifications ===================

fn install_notifications(ctx: &mut Context) {
    // `Notification` is a constructor that ALSO exposes static
    // `requestPermission` + `permission` properties. We expose it as
    // a callable global with the static fields attached.
    let _ = ctx.register_global_callable(
        js_string!("Notification"),
        2,
        NativeFunction::from_fn_ptr(notification_ctor),
    );
    // Attach static properties on the now-registered constructor.
    let global = ctx.global_object();
    if let Ok(notif_val) = global.get(js_string!("Notification"), ctx) {
        if let Some(notif) = notif_val.as_object() {
            let realm = ctx.realm().clone();
            let req = boa_engine::object::FunctionObjectBuilder::new(
                &realm,
                NativeFunction::from_fn_ptr(notification_request_permission),
            )
            .build();
            let _ = notif.set(
                js_string!("requestPermission"),
                JsValue::from(req),
                false,
                ctx,
            );
            let _ = notif.set(
                js_string!("permission"),
                JsValue::from(js_string!("default")),
                false,
                ctx,
            );
            let _ = notif.set(
                js_string!("maxActions"),
                JsValue::from(0u32),
                false,
                ctx,
            );
        }
    }
}

fn notification_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let title = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let body = args
        .get(1)
        .and_then(|v| v.as_object().cloned())
        .and_then(|o| o.get(js_string!("body"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let realm = ctx.realm().clone();
    let close = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(noop_zero),
    )
    .build();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("title"),
                JsValue::from(js_string!(title)),
                Attribute::READONLY,
            )
            .property(
                js_string!("body"),
                JsValue::from(js_string!(body)),
                Attribute::READONLY,
            )
            .property(js_string!("close"), JsValue::from(close), Attribute::READONLY)
            .build(),
    ))
}

fn notification_request_permission(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // No OS notification backend; refuse permission honestly so
    // pages take the fallback branch.
    Ok(JsPromise::resolve(JsValue::from(js_string!("denied")), ctx).into())
}

// =================== Push API ===================

fn install_push(ctx: &mut Context) {
    // `PushManager` is conceptually on
    // `ServiceWorkerRegistration.pushManager`. We install a global
    // `PushManager` namespace so feature detection passes; the SW
    // module can pull this in when it wires the registration object.
    let realm = ctx.realm().clone();
    let subscribe = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(push_subscribe),
    )
    .build();
    let get_subscription = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(push_get_subscription),
    )
    .build();
    let permission_state = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(push_permission_state),
    )
    .build();
    let push_manager = ObjectInitializer::new(ctx)
        .property(
            js_string!("subscribe"),
            JsValue::from(subscribe),
            Attribute::READONLY,
        )
        .property(
            js_string!("getSubscription"),
            JsValue::from(get_subscription),
            Attribute::READONLY,
        )
        .property(
            js_string!("permissionState"),
            JsValue::from(permission_state),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("PushManager"),
        push_manager,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn push_subscribe(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // No push server backing. Reject so pages take the offline
    // branch rather than handing a fake endpoint they'd POST to.
    let err: JsError = boa_engine::JsNativeError::error()
        .with_message("AbortError: push subscription unavailable")
        .into();
    Ok(JsPromise::reject(err, ctx).into())
}

fn push_get_subscription(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::null(), ctx).into())
}

fn push_permission_state(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(js_string!("denied")), ctx).into())
}

// =================== Background Sync ===================

fn install_background_sync(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let sync = ObjectInitializer::new(ctx)
        .property(
            js_string!("register"),
            JsValue::from(mk(sync_register)),
            Attribute::READONLY,
        )
        .property(
            js_string!("getTags"),
            JsValue::from(mk(sync_get_tags)),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("SyncManager"),
        sync,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn sync_register(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Spec says register resolves; we resolve since the toy can't
    // schedule a true background event but pages assume success.
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn sync_get_tags(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = boa_engine::object::builtins::JsArray::new(ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

// =================== WebTransport ===================

fn install_web_transport(ctx: &mut Context) {
    // `WebTransport` is a constructor that pages new up with a URL.
    let _ = ctx.register_global_callable(
        js_string!("WebTransport"),
        1,
        NativeFunction::from_fn_ptr(web_transport_ctor),
    );
}

fn web_transport_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    // The `ready` Promise rejects (no QUIC datagram channel wired
    // through to JS yet). Pages awaiting `transport.ready` get a
    // typed error and bail.
    let err: JsError = boa_engine::JsNativeError::error()
        .with_message("WebTransport unavailable on this toy build")
        .into();
    let rejected = JsPromise::reject(err, ctx);
    let realm = ctx.realm().clone();
    let close = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(noop_zero),
    )
    .build();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("url"),
                JsValue::from(js_string!(url)),
                Attribute::READONLY,
            )
            .property(
                js_string!("ready"),
                JsValue::from(rejected.clone()),
                Attribute::READONLY,
            )
            .property(
                js_string!("closed"),
                JsValue::from(rejected),
                Attribute::READONLY,
            )
            .property(js_string!("close"), JsValue::from(close), Attribute::READONLY)
            .build(),
    ))
}

// =================== Picture-in-Picture ===================

fn install_picture_in_picture(ctx: &mut Context) {
    // `document.pictureInPictureEnabled` / `exitPictureInPicture` —
    // wire on the existing document handle. We can't easily extend
    // dom.rs from here without refactor, so install module-level
    // globals that legacy feature-detection often falls back to.
    let realm = ctx.realm().clone();
    let exit = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(pip_exit),
    )
    .build();
    let global = ctx.global_object();
    if let Ok(doc_val) = global.get(js_string!("document"), ctx) {
        if let Some(doc) = doc_val.as_object() {
            let _ = doc.set(
                js_string!("pictureInPictureEnabled"),
                JsValue::from(false),
                false,
                ctx,
            );
            let _ = doc.set(
                js_string!("pictureInPictureElement"),
                JsValue::null(),
                false,
                ctx,
            );
            let _ = doc.set(
                js_string!("exitPictureInPicture"),
                JsValue::from(exit),
                false,
                ctx,
            );
        }
    }
    // Also register the PiP `PictureInPictureWindow` global for
    // feature-detection completeness.
    let pip_window = ObjectInitializer::new(ctx).build();
    let _ = ctx.register_global_property(
        js_string!("PictureInPictureWindow"),
        pip_window,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn pip_exit(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

// =================== Payment Request ===================

fn install_payment_request(ctx: &mut Context) {
    let _ = ctx.register_global_callable(
        js_string!("PaymentRequest"),
        2,
        NativeFunction::from_fn_ptr(payment_request_ctor),
    );
}

fn payment_request_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.realm().clone();
    let show = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(payment_request_show),
    )
    .build();
    let can_make = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(payment_request_can_make),
    )
    .build();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(js_string!("show"), JsValue::from(show), Attribute::READONLY)
            .property(
                js_string!("canMakePayment"),
                JsValue::from(can_make),
                Attribute::READONLY,
            )
            .property(js_string!("abort"), JsValue::from(0u32), Attribute::READONLY)
            .build(),
    ))
}

fn payment_request_show(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let err: JsError = boa_engine::JsNativeError::error()
        .with_message("AbortError: no payment UI available")
        .into();
    Ok(JsPromise::reject(err, ctx).into())
}

fn payment_request_can_make(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(false), ctx).into())
}

// =================== Web Locks ===================

fn install_web_locks(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let request = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(locks_request),
    )
    .build();
    let query = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(locks_query),
    )
    .build();
    let locks = ObjectInitializer::new(ctx)
        .property(js_string!("request"), JsValue::from(request), Attribute::READONLY)
        .property(js_string!("query"), JsValue::from(query), Attribute::READONLY)
        .build();
    nav_set(ctx, "locks", JsValue::from(locks));
}

/// `navigator.locks.request(name, [opts], callback)` — runs the
/// callback exclusively for the given lock name. Single-threaded
/// runtime → no real contention; we invoke the callback
/// synchronously inside the resolved Promise and return its result.
fn locks_request(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Last argument is the callback; preceding args are name + opts.
    let cb_val = args.iter().rev().find(|v| v.is_callable()).cloned();
    let Some(cb_val) = cb_val else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let Some(cb_obj) = cb_val.as_object() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let Some(cb) = boa_engine::object::builtins::JsFunction::from_object(cb_obj.clone()) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    // Synthetic lock object passed to the callback.
    let lock = ObjectInitializer::new(ctx)
        .property(
            js_string!("name"),
            JsValue::from(js_string!("toy-lock")),
            Attribute::READONLY,
        )
        .property(
            js_string!("mode"),
            JsValue::from(js_string!("exclusive")),
            Attribute::READONLY,
        )
        .build();
    let result = cb
        .call(&JsValue::undefined(), &[JsValue::from(lock)], ctx)
        .unwrap_or(JsValue::undefined());
    Ok(JsPromise::resolve(result, ctx).into())
}

fn locks_query(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let held = boa_engine::object::builtins::JsArray::new(ctx);
    let pending = boa_engine::object::builtins::JsArray::new(ctx);
    let empty = ObjectInitializer::new(ctx)
        .property(js_string!("held"), JsValue::from(held), Attribute::READONLY)
        .property(
            js_string!("pending"),
            JsValue::from(pending),
            Attribute::READONLY,
        )
        .build();
    Ok(JsPromise::resolve(JsValue::from(empty), ctx).into())
}

// =================== Compute Pressure / Idle Detector ===================

fn install_compute_pressure(ctx: &mut Context) {
    let _ = ctx.register_global_callable(
        js_string!("PressureObserver"),
        1,
        NativeFunction::from_fn_ptr(pressure_observer_ctor),
    );
}

fn pressure_observer_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.realm().clone();
    let observe = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
        }),
    )
    .build();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("observe"),
                JsValue::from(observe),
                Attribute::READONLY,
            )
            .property(js_string!("unobserve"), JsValue::from(0u32), Attribute::READONLY)
            .property(js_string!("disconnect"), JsValue::from(0u32), Attribute::READONLY)
            .build(),
    ))
}

fn install_idle_detector(ctx: &mut Context) {
    let _ = ctx.register_global_callable(
        js_string!("IdleDetector"),
        0,
        NativeFunction::from_fn_ptr(idle_detector_ctor),
    );
}

fn idle_detector_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.realm().clone();
    let start = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
        }),
    )
    .build();
    let request_permission = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            Ok(JsPromise::resolve(JsValue::from(js_string!("denied")), ctx).into())
        }),
    )
    .build();
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(js_string!("start"), JsValue::from(start), Attribute::READONLY)
            .property(
                js_string!("requestPermission"),
                JsValue::from(request_permission),
                Attribute::READONLY,
            )
            .property(
                js_string!("userState"),
                JsValue::from(js_string!("active")),
                Attribute::READONLY,
            )
            .property(
                js_string!("screenState"),
                JsValue::from(js_string!("unlocked")),
                Attribute::READONLY,
            )
            .build(),
    ))
}

// =================== Storage Buckets ===================

fn install_storage_buckets(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let open_or_create = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            // Real spec returns a StorageBucket; we resolve a stub
            // with the minimal getDirectory/clear surface.
            let bucket = ObjectInitializer::new(ctx)
                .property(
                    js_string!("name"),
                    JsValue::from(js_string!("default")),
                    Attribute::READONLY,
                )
                .property(js_string!("persisted"), JsValue::from(true), Attribute::READONLY)
                .build();
            Ok(JsPromise::resolve(JsValue::from(bucket), ctx).into())
        }),
    )
    .build();
    let keys_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            let arr = boa_engine::object::builtins::JsArray::new(ctx);
            Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
        }),
    )
    .build();
    let storage_buckets = ObjectInitializer::new(ctx)
        .property(
            js_string!("open"),
            JsValue::from(open_or_create.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("keys"),
            JsValue::from(keys_fn),
            Attribute::READONLY,
        )
        .property(
            js_string!("delete"),
            JsValue::from(0u32),
            Attribute::READONLY,
        )
        .build();
    nav_set(ctx, "storageBuckets", JsValue::from(storage_buckets));
}

// =================== Document PiP ===================

fn install_document_pip(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let request_window = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            let err: JsError = boa_engine::JsNativeError::error()
                .with_message("NotSupportedError: document PiP not available")
                .into();
            Ok(JsPromise::reject(err, ctx).into())
        }),
    )
    .build();
    let doc_pip = ObjectInitializer::new(ctx)
        .property(
            js_string!("requestWindow"),
            JsValue::from(request_window),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("documentPictureInPicture"),
        doc_pip,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

// =================== CSS Houdini (Paint/Layout/Animation Worklets) ===================

fn install_css_houdini(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let make_worklet = |ctx: &mut Context| -> JsObject {
        let add_module = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
                // Real Worklets fetch + module-evaluate the URL in a
                // separate context. We accept the call so pages
                // don't crash on feature detection but actually
                // executing the worklet's paint() is not wired.
                Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
            }),
        )
        .build();
        ObjectInitializer::new(ctx)
            .property(
                js_string!("addModule"),
                JsValue::from(add_module),
                Attribute::READONLY,
            )
            .build()
    };
    let paint_worklet = make_worklet(ctx);
    let layout_worklet = make_worklet(ctx);
    let animation_worklet = make_worklet(ctx);
    let register_property = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, _: &mut Context| Ok(JsValue::undefined())),
    )
    .build();
    let supports_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        // Permissive: assume the page's property/value is supported.
        // Refining this requires the value parser; this matches what
        // most polyfill-feature-detection paths actually look at.
        NativeFunction::from_fn_ptr(|_, _, _: &mut Context| Ok(JsValue::from(true))),
    )
    .build();
    let escape_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(css_escape),
    )
    .build();
    let css = ObjectInitializer::new(ctx)
        .property(
            js_string!("paintWorklet"),
            JsValue::from(paint_worklet),
            Attribute::READONLY,
        )
        .property(
            js_string!("layoutWorklet"),
            JsValue::from(layout_worklet),
            Attribute::READONLY,
        )
        .property(
            js_string!("animationWorklet"),
            JsValue::from(animation_worklet),
            Attribute::READONLY,
        )
        .property(
            js_string!("registerProperty"),
            JsValue::from(register_property),
            Attribute::READONLY,
        )
        .property(
            js_string!("supports"),
            JsValue::from(supports_fn),
            Attribute::READONLY,
        )
        .property(
            js_string!("escape"),
            JsValue::from(escape_fn),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("CSS"),
        css,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn css_escape(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Spec: prefixes / escapes characters that would otherwise be
    // syntactically meaningful in a CSS identifier or string. Our
    // toy escapes the small set most pages actually feed in:
    // ASCII control + the characters in `\" '\\#.[]:>~+*$|^?`.
    let Some(arg) = args.first() else {
        return Ok(JsValue::from(js_string!("")));
    };
    let s = arg.to_string(ctx)?.to_std_string_escaped();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_control() || c == ' ' {
            out.push_str(&format!("\\{:x} ", c as u32));
        } else if "\"'\\#.[]:>~+*$|^?,;@".contains(c) {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    Ok(JsValue::from(js_string!(out)))
}

// =================== WebExtensions shim ===================

fn install_webextensions_stub(ctx: &mut Context) {
    // Just enough surface for pages that feature-detect
    // `chrome.runtime.id` or `browser.runtime.getManifest`. We
    // aren't actually an extension host — calls that try to
    // install or message an extension reject.
    let realm = ctx.realm().clone();
    let get_manifest = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            Ok(JsValue::from(ObjectInitializer::new(ctx).build()))
        }),
    )
    .build();
    let send_message = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            let err: JsError = boa_engine::JsNativeError::error()
                .with_message("Receiving end does not exist")
                .into();
            Ok(JsPromise::reject(err, ctx).into())
        }),
    )
    .build();
    let runtime = ObjectInitializer::new(ctx)
        .property(js_string!("id"), JsValue::null(), Attribute::READONLY)
        .property(
            js_string!("getManifest"),
            JsValue::from(get_manifest),
            Attribute::READONLY,
        )
        .property(
            js_string!("sendMessage"),
            JsValue::from(send_message),
            Attribute::READONLY,
        )
        .property(
            js_string!("lastError"),
            JsValue::null(),
            Attribute::WRITABLE,
        )
        .build();
    let chrome_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("runtime"),
            JsValue::from(runtime.clone()),
            Attribute::READONLY,
        )
        .build();
    let browser_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("runtime"),
            JsValue::from(runtime),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("chrome"),
        chrome_obj,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
    let _ = ctx.register_global_property(
        js_string!("browser"),
        browser_obj,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

// =================== WebXR ===================

fn install_webxr(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let is_supported = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            Ok(JsPromise::resolve(JsValue::from(false), ctx).into())
        }),
    )
    .build();
    let request_session = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(|_, _, ctx: &mut Context| {
            let err: JsError = boa_engine::JsNativeError::error()
                .with_message("NotSupportedError: WebXR session unavailable")
                .into();
            Ok(JsPromise::reject(err, ctx).into())
        }),
    )
    .build();
    let xr = ObjectInitializer::new(ctx)
        .property(
            js_string!("isSessionSupported"),
            JsValue::from(is_supported),
            Attribute::READONLY,
        )
        .property(
            js_string!("requestSession"),
            JsValue::from(request_session),
            Attribute::READONLY,
        )
        .build();
    nav_set(ctx, "xr", JsValue::from(xr));
}
