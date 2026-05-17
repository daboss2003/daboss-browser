//! `navigator.mediaDevices.getUserMedia` (toy).
//!
//! Exposes the JS surface scripts probe for: `getUserMedia` returns a
//! Promise. The toy resolves it with a `MediaStream` that contains no
//! real tracks — equivalent to the user granting permission but the
//! hardware enumerating empty. Pages don't crash on the API call;
//! demos that actually exercise the video frames just get nothing.
//!
//! Real camera/mic capture requires platform-specific crates
//! (`nokhwa` for cameras, cpal capture mode for mic) plus per-frame
//! delivery into a pixmap pipeline. That's a focused follow-up — the
//! API contract here is in place so a Service Worker or WebRTC peer
//! can already construct streams without errors at the JS layer.

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let get_user_media = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(get_user_media),
    )
    .build();
    let enumerate_devices = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(enumerate_devices),
    )
    .build();
    let media_devices = ObjectInitializer::new(ctx)
        .property(
            js_string!("getUserMedia"),
            JsValue::from(get_user_media),
            Attribute::READONLY,
        )
        .property(
            js_string!("enumerateDevices"),
            JsValue::from(enumerate_devices),
            Attribute::READONLY,
        )
        .build();
    // Hang `mediaDevices` off `navigator`.
    let global = ctx.global_object();
    let navigator = global
        .get(js_string!("navigator"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned());
    if let Some(nav) = navigator {
        let _ = nav.set(
            js_string!("mediaDevices"),
            JsValue::from(media_devices),
            false,
            ctx,
        );
    }
}

fn get_user_media(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let stream = make_media_stream(ctx);
    Ok(JsPromise::resolve(stream, ctx).into())
}

fn enumerate_devices(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

fn make_media_stream(ctx: &mut Context) -> JsValue {
    let id = format!(
        "stream-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("active"),
        JsValue::from(true),
        Attribute::READONLY,
    );
    b.property(
        js_string!("id"),
        JsValue::from(js_string!(id)),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(stream_empty_array),
        js_string!("getTracks"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(stream_empty_array),
        js_string!("getVideoTracks"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(stream_empty_array),
        js_string!("getAudioTracks"),
        0,
    );
    JsValue::from(b.build())
}

fn stream_empty_array(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsArray::new(ctx).into())
}
