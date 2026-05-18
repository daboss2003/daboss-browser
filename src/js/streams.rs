//! `ReadableStream` + `WritableStream` (+ readers/writers) JS bindings.
//!
//! Toy scope:
//!   * `new ReadableStream(source)` — `source` is an object with an
//!     optional `start(controller)` and `pull(controller)` callback,
//!     plus optional `cancel(reason)`. The controller exposes
//!     `enqueue(chunk)`, `close()`, `error(reason)`.
//!   * `stream.getReader()` returns a default reader with `read()`
//!     → `Promise<{ value, done }>` and `cancel(reason)`.
//!   * `new WritableStream(sink)` — `sink` may have `start`, `write`,
//!     `close`, `abort` callbacks; `getWriter()` returns a writer
//!     with `write(chunk)`, `close()`, `abort(reason)`.
//!   * `stream.tee()` — splits into two ReadableStreams sharing the
//!     same chunks.
//!   * `stream.pipeTo(writableStream)` — drains the source into the
//!     sink, returning a Promise.
//!
//! Not implemented: byte streams (`{ type: 'bytes' }` controllers),
//! highWaterMark backpressure, `pipeThrough` transform streams.
//!
//! Internally each stream is a `VecDeque<JsValue>` chunk queue plus
//! state bits. The reader's `read()` resolves immediately when the
//! queue is non-empty and otherwise installs a wake-up callback that
//! the controller's `enqueue()` / `close()` fire.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

const READABLE_ID_KEY: &str = "__readable_id";
const READER_OWNS_KEY: &str = "__reader_owns_id";
const WRITABLE_ID_KEY: &str = "__writable_id";
const WRITER_OWNS_KEY: &str = "__writer_owns_id";
const CONTROLLER_OWNS_KEY: &str = "__controller_owns_id";

pub struct ReadableState {
    pub queue: VecDeque<JsValue>,
    pub closed: bool,
    pub errored: Option<JsValue>,
    pub locked: bool,
    /// Pending read continuations (resolve, reject) installed when
    /// `read()` is called against an empty queue.
    pub pending_reads: Vec<(JsFunction, JsFunction)>,
    pub cancel_cb: Option<JsFunction>,
    pub pull_cb: Option<JsFunction>,
    /// JS handle for the controller, so `pull` can be invoked with
    /// the same controller object the source's `start` got.
    pub controller_handle: Option<boa_engine::JsObject>,
}

pub struct WritableState {
    pub queue: VecDeque<JsValue>,
    pub closed: bool,
    pub errored: Option<JsValue>,
    pub locked: bool,
    pub write_cb: Option<JsFunction>,
    pub close_cb: Option<JsFunction>,
    pub abort_cb: Option<JsFunction>,
}

pub type ReadableRegistry = Rc<RefCell<HashMap<u32, ReadableState>>>;
pub type WritableRegistry = Rc<RefCell<HashMap<u32, WritableState>>>;

thread_local! {
    pub(crate) static JS_READABLES: RefCell<Option<ReadableRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static JS_WRITABLES: RefCell<Option<WritableRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static STREAMS_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    STREAMS_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    JS_READABLES.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    JS_WRITABLES.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    ctx.register_global_callable(
        js_string!("ReadableStream"),
        1,
        NativeFunction::from_fn_ptr(readable_stream_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("WritableStream"),
        1,
        NativeFunction::from_fn_ptr(writable_stream_ctor),
    )
    .ok();
}

// ============ ReadableStream ============

fn readable_stream_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = next_id();
    let source = args.first().cloned().unwrap_or(JsValue::undefined());
    let (start_cb, pull_cb, cancel_cb) = extract_source_callbacks(&source, ctx);
    register_readable(
        id,
        ReadableState {
            queue: VecDeque::new(),
            closed: false,
            errored: None,
            locked: false,
            pending_reads: Vec::new(),
            cancel_cb,
            pull_cb,
            controller_handle: None,
        },
    );

    // Build the controller and stash its handle.
    let controller = build_readable_controller(ctx, id);
    if let Some(reg) = JS_READABLES.with(|r| r.borrow().clone()) {
        if let Some(s) = reg.borrow_mut().get_mut(&id) {
            s.controller_handle = controller.as_object().cloned();
        }
    }

    // Fire `start(controller)` synchronously per spec.
    if let Some(start) = start_cb {
        let _ = start.call(&source, &[controller.clone()], ctx);
    }

    let stream = ObjectInitializer::new(ctx)
        .property(
            js_string!(READABLE_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .property(
            js_string!("locked"),
            JsValue::from(false),
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(readable_get_reader),
            js_string!("getReader"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_cancel),
            js_string!("cancel"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_tee),
            js_string!("tee"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_pipe_to),
            js_string!("pipeTo"),
            1,
        )
        .build();
    Ok(JsValue::from(stream))
}

fn extract_source_callbacks(
    source: &JsValue,
    ctx: &mut Context,
) -> (Option<JsFunction>, Option<JsFunction>, Option<JsFunction>) {
    let obj = match source.as_object() {
        Some(o) => o,
        None => return (None, None, None),
    };
    let mut read_fn = |name: &str| -> Option<JsFunction> {
        obj.get(js_string!(name.to_string()), ctx)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .and_then(JsFunction::from_object)
    };
    (read_fn("start"), read_fn("pull"), read_fn("cancel"))
}

fn register_readable(id: u32, state: ReadableState) {
    if let Some(reg) = JS_READABLES.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(id, state);
    }
}

fn build_readable_controller(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(CONTROLLER_OWNS_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(controller_enqueue),
        js_string!("enqueue"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(controller_close),
        js_string!("close"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(controller_error),
        js_string!("error"),
        1,
    );
    JsValue::from(b.build())
}

fn controller_id(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CONTROLLER_OWNS_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn controller_enqueue(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = controller_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let chunk = args.first().cloned().unwrap_or(JsValue::undefined());
    enqueue_chunk(ctx, id, chunk);
    Ok(JsValue::undefined())
}

fn controller_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = controller_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let pending = JS_READABLES
        .with(|r| -> Option<Vec<(JsFunction, JsFunction)>> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let s = reg.get_mut(&id)?;
            s.closed = true;
            Some(std::mem::take(&mut s.pending_reads))
        })
        .unwrap_or_default();
    // Resolve every pending read with `{ value: undefined, done: true }`.
    for (resolve, _reject) in pending {
        let result = build_read_result(ctx, JsValue::undefined(), true);
        let _ = resolve.call(&JsValue::undefined(), &[result], ctx);
    }
    Ok(JsValue::undefined())
}

fn controller_error(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = controller_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let reason = args.first().cloned().unwrap_or(JsValue::undefined());
    let pending = JS_READABLES
        .with(|r| -> Option<Vec<(JsFunction, JsFunction)>> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let s = reg.get_mut(&id)?;
            s.errored = Some(reason.clone());
            Some(std::mem::take(&mut s.pending_reads))
        })
        .unwrap_or_default();
    for (_resolve, reject) in pending {
        let _ = reject.call(&JsValue::undefined(), &[reason.clone()], ctx);
    }
    Ok(JsValue::undefined())
}

fn enqueue_chunk(ctx: &mut Context, id: u32, chunk: JsValue) {
    // If a read is pending, resolve it immediately; otherwise stash
    // the chunk on the queue for the next read().
    let resolved = JS_READABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        if s.pending_reads.is_empty() {
            s.queue.push_back(chunk.clone());
            None
        } else {
            Some(s.pending_reads.remove(0).0)
        }
    });
    if let Some(resolve) = resolved {
        let result = build_read_result(ctx, chunk, false);
        let _ = resolve.call(&JsValue::undefined(), &[result], ctx);
    }
}

fn build_read_result(ctx: &mut Context, value: JsValue, done: bool) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(js_string!("value"), value, Attribute::READONLY)
        .property(js_string!("done"), JsValue::from(done), Attribute::READONLY)
        .build()
        .into()
}

fn readable_get_reader(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = readable_id_of(this, ctx) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getReader: stream not registered")
            .into());
    };
    let already_locked = JS_READABLES
        .with(|r| {
            r.borrow()
                .as_ref()
                .and_then(|rc| rc.borrow().get(&id).map(|s| s.locked))
        })
        .unwrap_or(false);
    if already_locked {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("getReader: stream already locked to a reader")
            .into());
    }
    if let Some(reg) = JS_READABLES.with(|r| r.borrow().clone()) {
        if let Some(s) = reg.borrow_mut().get_mut(&id) {
            s.locked = true;
        }
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(js_string!("locked"), JsValue::from(true), false, ctx);
    }
    let reader = ObjectInitializer::new(ctx)
        .property(
            js_string!(READER_OWNS_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_read),
            js_string!("read"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_cancel),
            js_string!("cancel"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_release_lock),
            js_string!("releaseLock"),
            0,
        )
        .build();
    Ok(JsValue::from(reader))
}

fn readable_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(READABLE_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn reader_owns(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(READER_OWNS_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn reader_read(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = reader_owns(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    // If we have a chunk available, return it synchronously via a
    // resolved Promise.
    let (chunk, closed, errored, has_pull) = JS_READABLES
        .with(|r| -> Option<(Option<JsValue>, bool, Option<JsValue>, bool)> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let s = reg.get_mut(&id)?;
            let next = s.queue.pop_front();
            Some((next, s.closed, s.errored.clone(), s.pull_cb.is_some()))
        })
        .unwrap_or((None, false, None, false));

    if let Some(err) = errored {
        return Ok(JsPromise::reject(boa_engine::JsError::from_opaque(err), ctx).into());
    }
    if let Some(value) = chunk {
        let res = build_read_result(ctx, value, false);
        return Ok(JsPromise::resolve(res, ctx).into());
    }
    if closed {
        let res = build_read_result(ctx, JsValue::undefined(), true);
        return Ok(JsPromise::resolve(res, ctx).into());
    }

    // Build an unresolved promise; stash resolvers on the stream.
    let (promise, resolvers) = JsPromise::new_pending(ctx);
    if let Some(reg) = JS_READABLES.with(|r| r.borrow().clone()) {
        if let Some(s) = reg.borrow_mut().get_mut(&id) {
            s.pending_reads.push((resolvers.resolve, resolvers.reject));
        }
    }
    // Drive `pull(controller)` so the source can produce more.
    if has_pull {
        let (pull, controller) = JS_READABLES
            .with(|r| -> Option<(JsFunction, JsValue)> {
                let rc = r.borrow().as_ref()?.clone();
                let reg = rc.borrow();
                let s = reg.get(&id)?;
                let pull = s.pull_cb.clone()?;
                let controller = s
                    .controller_handle
                    .as_ref()
                    .map(|o| JsValue::from(o.clone()))
                    .unwrap_or(JsValue::undefined());
                Some((pull, controller))
            })
            .unzip();
        if let (Some(pull), Some(controller)) = (pull, controller) {
            let _ = pull.call(&JsValue::undefined(), &[controller], ctx);
        }
    }
    Ok(promise.into())
}

fn reader_cancel(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = reader_owns(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let reason = args.first().cloned().unwrap_or(JsValue::undefined());
    let cancel_cb = JS_READABLES
        .with(|r| -> Option<JsFunction> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let s = reg.get_mut(&id)?;
            s.closed = true;
            s.queue.clear();
            s.cancel_cb.clone()
        });
    if let Some(cb) = cancel_cb {
        let _ = cb.call(&JsValue::undefined(), &[reason.clone()], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn reader_release_lock(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = reader_owns(this, ctx) {
        if let Some(reg) = JS_READABLES.with(|r| r.borrow().clone()) {
            if let Some(s) = reg.borrow_mut().get_mut(&id) {
                s.locked = false;
            }
        }
    }
    Ok(JsValue::undefined())
}

fn readable_cancel(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = readable_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let reason = args.first().cloned().unwrap_or(JsValue::undefined());
    let cancel_cb = JS_READABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.closed = true;
        s.queue.clear();
        s.cancel_cb.clone()
    });
    if let Some(cb) = cancel_cb {
        let _ = cb.call(&JsValue::undefined(), &[reason.clone()], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn readable_tee(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Drain source into two new readable streams that share the
    // chunks. We snapshot the current queue (so already-enqueued
    // data goes to both branches) and rewire the source's cancel to
    // close both branches.
    let Some(src_id) = readable_id_of(this, ctx) else {
        let arr = boa_engine::object::builtins::JsArray::new(ctx);
        return Ok(arr.into());
    };
    let (initial_chunks, closed, errored) = JS_READABLES
        .with(|r| -> Option<(Vec<JsValue>, bool, Option<JsValue>)> {
            let rc = r.borrow().as_ref()?.clone();
            let reg = rc.borrow();
            let s = reg.get(&src_id)?;
            Some((
                s.queue.iter().cloned().collect(),
                s.closed,
                s.errored.clone(),
            ))
        })
        .unwrap_or((Vec::new(), false, None));

    let a_id = next_id();
    let b_id = next_id();
    let mk_state = |chunks: Vec<JsValue>| ReadableState {
        queue: chunks.into_iter().collect(),
        closed,
        errored: errored.clone(),
        locked: false,
        pending_reads: Vec::new(),
        cancel_cb: None,
        pull_cb: None,
        controller_handle: None,
    };
    register_readable(a_id, mk_state(initial_chunks.clone()));
    register_readable(b_id, mk_state(initial_chunks));

    let a = build_readable_handle(ctx, a_id);
    let b = build_readable_handle(ctx, b_id);
    let arr = boa_engine::object::builtins::JsArray::new(ctx);
    let _ = arr.push(a, ctx);
    let _ = arr.push(b, ctx);
    Ok(arr.into())
}

fn build_readable_handle(ctx: &mut Context, id: u32) -> JsValue {
    let stream = ObjectInitializer::new(ctx)
        .property(
            js_string!(READABLE_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .property(
            js_string!("locked"),
            JsValue::from(false),
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(readable_get_reader),
            js_string!("getReader"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_cancel),
            js_string!("cancel"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_tee),
            js_string!("tee"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(readable_pipe_to),
            js_string!("pipeTo"),
            1,
        )
        .build();
    JsValue::from(stream)
}

fn readable_pipe_to(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Drain the source synchronously into the writable. Real spec
    // uses Promises for backpressure; the toy assumes sync sinks.
    let Some(src_id) = readable_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let writable = args.first().cloned().unwrap_or(JsValue::undefined());
    let dst_id = writable_id_of(&writable, ctx);
    let Some(dst_id) = dst_id else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    loop {
        let next = JS_READABLES.with(|r| -> Option<(Option<JsValue>, bool)> {
            let rc = r.borrow().as_ref()?.clone();
            let mut reg = rc.borrow_mut();
            let s = reg.get_mut(&src_id)?;
            let next = s.queue.pop_front();
            Some((next, s.closed))
        });
        match next {
            Some((Some(chunk), _)) => {
                write_to_writable(ctx, dst_id, chunk);
            }
            Some((None, true)) | None => break,
            Some((None, false)) => break, // no more chunks for now
        }
    }
    // Close the writable at the end.
    let close_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&dst_id)?;
        s.closed = true;
        s.close_cb.clone()
    });
    if let Some(cb) = close_cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

// ============ WritableStream ============

fn writable_stream_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = next_id();
    let sink = args.first().cloned().unwrap_or(JsValue::undefined());
    let (start_cb, write_cb, close_cb, abort_cb) = extract_sink_callbacks(&sink, ctx);
    if let Some(reg) = JS_WRITABLES.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(
            id,
            WritableState {
                queue: VecDeque::new(),
                closed: false,
                errored: None,
                locked: false,
                write_cb,
                close_cb,
                abort_cb,
            },
        );
    }
    if let Some(start) = start_cb {
        let _ = start.call(&sink, &[], ctx);
    }
    let stream = ObjectInitializer::new(ctx)
        .property(
            js_string!(WRITABLE_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .property(
            js_string!("locked"),
            JsValue::from(false),
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(writable_get_writer),
            js_string!("getWriter"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(writable_abort),
            js_string!("abort"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(writable_close),
            js_string!("close"),
            0,
        )
        .build();
    Ok(JsValue::from(stream))
}

fn extract_sink_callbacks(
    sink: &JsValue,
    ctx: &mut Context,
) -> (
    Option<JsFunction>,
    Option<JsFunction>,
    Option<JsFunction>,
    Option<JsFunction>,
) {
    let obj = match sink.as_object() {
        Some(o) => o,
        None => return (None, None, None, None),
    };
    let mut read_fn = |name: &str| -> Option<JsFunction> {
        obj.get(js_string!(name.to_string()), ctx)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .and_then(JsFunction::from_object)
    };
    (
        read_fn("start"),
        read_fn("write"),
        read_fn("close"),
        read_fn("abort"),
    )
}

fn writable_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WRITABLE_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn writer_owns(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WRITER_OWNS_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn writable_get_writer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writable_id_of(this, ctx) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getWriter: stream not registered")
            .into());
    };
    let already_locked = JS_WRITABLES
        .with(|r| {
            r.borrow()
                .as_ref()
                .and_then(|rc| rc.borrow().get(&id).map(|s| s.locked))
        })
        .unwrap_or(false);
    if already_locked {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("getWriter: stream already locked to a writer")
            .into());
    }
    if let Some(reg) = JS_WRITABLES.with(|r| r.borrow().clone()) {
        if let Some(s) = reg.borrow_mut().get_mut(&id) {
            s.locked = true;
        }
    }
    let writer = ObjectInitializer::new(ctx)
        .property(
            js_string!(WRITER_OWNS_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_write),
            js_string!("write"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_close),
            js_string!("close"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_abort),
            js_string!("abort"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_release_lock),
            js_string!("releaseLock"),
            0,
        )
        .build();
    Ok(JsValue::from(writer))
}

fn writer_write(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_owns(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let chunk = args.first().cloned().unwrap_or(JsValue::undefined());
    write_to_writable(ctx, id, chunk);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn write_to_writable(ctx: &mut Context, id: u32, chunk: JsValue) {
    let write_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.queue.push_back(chunk.clone());
        s.write_cb.clone()
    });
    if let Some(cb) = write_cb {
        let _ = cb.call(&JsValue::undefined(), &[chunk], ctx);
    }
}

fn writer_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_owns(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let close_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.closed = true;
        s.close_cb.clone()
    });
    if let Some(cb) = close_cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writer_abort(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_owns(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let reason = args.first().cloned().unwrap_or(JsValue::undefined());
    let abort_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.errored = Some(reason.clone());
        s.abort_cb.clone()
    });
    if let Some(cb) = abort_cb {
        let _ = cb.call(&JsValue::undefined(), &[reason], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writer_release_lock(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_owns(this, ctx) {
        if let Some(reg) = JS_WRITABLES.with(|r| r.borrow().clone()) {
            if let Some(s) = reg.borrow_mut().get_mut(&id) {
                s.locked = false;
            }
        }
    }
    Ok(JsValue::undefined())
}

fn writable_abort(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writable_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let reason = args.first().cloned().unwrap_or(JsValue::undefined());
    let abort_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.errored = Some(reason.clone());
        s.abort_cb.clone()
    });
    if let Some(cb) = abort_cb {
        let _ = cb.call(&JsValue::undefined(), &[reason], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writable_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writable_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let close_cb = JS_WRITABLES.with(|r| -> Option<JsFunction> {
        let rc = r.borrow().as_ref()?.clone();
        let mut reg = rc.borrow_mut();
        let s = reg.get_mut(&id)?;
        s.closed = true;
        s.close_cb.clone()
    });
    if let Some(cb) = close_cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

/// Public helper for `fetch`: wrap a finite byte slice in a fresh
/// ReadableStream that yields one `Uint8Array` chunk, then closes.
/// Used to build `response.body`.
pub fn body_to_stream(ctx: &mut Context, bytes: &[u8]) -> JsValue {
    use boa_engine::object::builtins::JsUint8Array;
    let id = next_id();
    let mut queue = VecDeque::new();
    if !bytes.is_empty() {
        let arr = JsUint8Array::from_iter(bytes.iter().copied(), ctx)
            .map(JsValue::from)
            .unwrap_or(JsValue::undefined());
        queue.push_back(arr);
    }
    register_readable(
        id,
        ReadableState {
            queue,
            closed: true,
            errored: None,
            locked: false,
            pending_reads: Vec::new(),
            cancel_cb: None,
            pull_cb: None,
            controller_handle: None,
        },
    );
    build_readable_handle(ctx, id)
}
