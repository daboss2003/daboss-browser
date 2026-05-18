//! `navigator.mediaDevices.getUserMedia` backed by `nokhwa`
//! (camera) + `cpal` input (microphone).
//!
//! The constructor opens whichever hardware tracks the constraints
//! ask for, then hands JS a `MediaStream` whose `__capture_idx`
//! identifies a `CaptureStream` in a per-engine registry. Paint
//! reads the live frame from that registry when a `<video>` element
//! has `srcObject` set to the stream.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::capture::CaptureStream;
use crate::dom::NodeId;

pub type CaptureRegistry = Rc<RefCell<Vec<Option<CaptureStream>>>>;
/// Maps a `<video>` (or `<audio>`) element to the capture index its
/// `srcObject` was set to. Paint consults this to pull live camera
/// frames from the [`CaptureRegistry`].
pub type CaptureBindings = Rc<RefCell<HashMap<NodeId, usize>>>;

thread_local! {
    pub(crate) static JS_CAPTURE_REGISTRY: RefCell<Option<CaptureRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static JS_CAPTURE_BINDINGS: RefCell<Option<CaptureBindings>> =
        const { RefCell::new(None) };
}

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

fn get_user_media(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Parse the constraints — `{video: true, audio: true}` is the
    // common form. Anything truthy on either axis turns the device
    // on; explicit `false` skips it.
    let (want_video, want_audio) = match args.first() {
        Some(v) => {
            let obj = v.as_object();
            let mut truthy = |name| -> bool {
                obj.as_ref()
                    .and_then(|o| o.get(js_string!(name), ctx).ok())
                    .map(|val| val.to_boolean())
                    .unwrap_or(false)
            };
            (truthy("video"), truthy("audio"))
        }
        None => (true, false),
    };
    let stream_idx = match CaptureStream::open(want_video, want_audio) {
        Some(s) => {
            JS_CAPTURE_REGISTRY.with(|slot| -> Option<usize> {
                let rc = slot.borrow().as_ref().cloned()?;
                let mut reg = rc.borrow_mut();
                reg.push(Some(s));
                Some(reg.len() - 1)
            })
        }
        None => None,
    };
    let stream = match stream_idx {
        Some(idx) => make_media_stream(ctx, Some(idx as u32)),
        None => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(
                    JsValue::from(js_string!(
                        "NotAllowedError: getUserMedia hardware unavailable"
                    )),
                ),
                ctx,
            )
            .into());
        }
    };
    Ok(JsPromise::resolve(stream, ctx).into())
}

fn enumerate_devices(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

fn make_media_stream(ctx: &mut Context, capture_idx: Option<u32>) -> JsValue {
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
    if let Some(idx) = capture_idx {
        b.property(
            js_string!("__capture_idx"),
            JsValue::from(idx),
            Attribute::READONLY,
        );
    }
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
