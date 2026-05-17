//! Web Workers (toy).
//!
//! Each `new Worker(url)` fetches the script source and spawns a
//! dedicated OS thread. Inside that thread we stand up a fresh
//! `boa::Context` with a minimal worker-scope global surface:
//! `console.log`, `self.postMessage`, `self.onmessage`. Bidirectional
//! `postMessage` uses `mpsc` channels — main → worker pushes JSON-y
//! strings the worker drains in a tight loop, worker → main pushes
//! into a queue the JS engine drains alongside microtasks.
//!
//! Scope cut for the toy:
//!  * No `importScripts`.
//!  * No transferable objects — messages are stringified.
//!  * No SharedWorker or ServiceWorker (the latter has its own
//!    module).
//!  * No DOM access from the worker (matches spec; workers have a
//!    `WorkerGlobalScope`, not a `Document`).
//!  * Termination: `terminate()` sets a flag the worker thread polls.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer,
    property::Attribute, Context, JsResult, JsValue, NativeFunction, Source,
};

pub struct WorkerEntry {
    /// Channel for messages going FROM main thread TO the worker.
    pub outgoing: mpsc::Sender<String>,
    /// Queue for messages coming back from the worker — drained on
    /// each engine tick.
    pub incoming: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    pub stop: Arc<AtomicBool>,
    pub handle: Option<boa_engine::JsObject>,
    _thread: JoinHandle<()>,
}

pub type WorkerRegistry = Rc<RefCell<Vec<Option<WorkerEntry>>>>;

thread_local! {
    pub(crate) static JS_WORKERS: RefCell<Option<WorkerRegistry>> = const { RefCell::new(None) };
}

thread_local! {
    /// Inside the worker thread's boa Context, we stash the queues
    /// here so `self.postMessage` / `onmessage` can find them.
    static WORKER_INBOUND: RefCell<Option<mpsc::Receiver<String>>> = RefCell::new(None);
    static WORKER_OUTBOUND: RefCell<Option<Arc<std::sync::Mutex<std::collections::VecDeque<String>>>>>
        = RefCell::new(None);
    static WORKER_STOP: RefCell<Option<Arc<AtomicBool>>> = RefCell::new(None);
}

const WORKER_IDX_KEY: &str = "__worker_idx";

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("Worker"),
        1,
        NativeFunction::from_fn_ptr(worker_constructor),
    )
    .ok();
}

fn worker_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(url_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let url = url_val.to_string(ctx)?.to_std_string_escaped();
    let Some(registry) = JS_WORKERS.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::null());
    };

    // Fetch the worker script through the same SSRF-guarded client.
    let client = super::engine::JS_FETCH_CLIENT.with(|c| c.borrow().clone());
    let base = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let resolved = match base {
        Some(b) => b.join(&url).ok(),
        None => url::Url::parse(&url).ok(),
    };
    let Some(resolved) = resolved else {
        return Err(boa_engine::JsNativeError::error()
            .with_message(format!("Worker: invalid URL {url}"))
            .into());
    };
    let source = match client {
        Some(c) => match c.get(&resolved.to_string()) {
            Ok(resp) if (200..300).contains(&resp.status) => {
                String::from_utf8_lossy(&resp.body).into_owned()
            }
            Ok(resp) => {
                return Err(boa_engine::JsNativeError::error()
                    .with_message(format!("Worker fetch HTTP {}", resp.status))
                    .into());
            }
            Err(e) => {
                return Err(boa_engine::JsNativeError::error()
                    .with_message(format!("Worker fetch: {e}"))
                    .into());
            }
        },
        None => {
            return Err(boa_engine::JsNativeError::error()
                .with_message("Worker: no fetch client installed")
                .into());
        }
    };

    let (out_tx, out_rx) = mpsc::channel::<String>();
    let incoming: Arc<std::sync::Mutex<std::collections::VecDeque<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let incoming_for_worker = incoming.clone();
    let stop_for_worker = stop.clone();
    let thread = std::thread::spawn(move || {
        WORKER_INBOUND.with(|s| *s.borrow_mut() = Some(out_rx));
        WORKER_OUTBOUND.with(|s| *s.borrow_mut() = Some(incoming_for_worker));
        WORKER_STOP.with(|s| *s.borrow_mut() = Some(stop_for_worker.clone()));

        let mut wctx = Context::default();
        install_worker_globals(&mut wctx);
        // Run the worker's top-level script.
        if let Err(e) = wctx.eval(Source::from_bytes(source.as_bytes())) {
            eprintln!("[worker] init script threw: {e}");
        }
        // Pump messages until terminated.
        worker_pump(&mut wctx, &stop_for_worker);
    });

    let idx = {
        let mut reg = registry.borrow_mut();
        reg.push(Some(WorkerEntry {
            outgoing: out_tx,
            incoming,
            stop,
            handle: None,
            _thread: thread,
        }));
        reg.len() - 1
    };

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(WORKER_IDX_KEY),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.property(js_string!("onmessage"), JsValue::null(), Attribute::all());
    b.property(js_string!("onerror"), JsValue::null(), Attribute::all());
    b.function(
        NativeFunction::from_fn_ptr(worker_post_message),
        js_string!("postMessage"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(worker_terminate),
        js_string!("terminate"),
        0,
    );
    let handle = b.build();
    if let Some(slot) = registry.borrow_mut().get_mut(idx).and_then(|s| s.as_mut()) {
        slot.handle = Some(handle.clone());
    }
    Ok(JsValue::from(handle))
}

fn worker_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WORKER_IDX_KEY), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn worker_post_message(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = worker_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(msg_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let payload = msg_val.to_string(ctx)?.to_std_string_escaped();
    if let Some(registry) = JS_WORKERS.with(|r| r.borrow().clone()) {
        if let Some(Some(entry)) = registry.borrow().get(idx) {
            let _ = entry.outgoing.send(payload);
        }
    }
    Ok(JsValue::undefined())
}

fn worker_terminate(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = worker_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    if let Some(registry) = JS_WORKERS.with(|r| r.borrow().clone()) {
        if let Some(Some(entry)) = registry.borrow().get(idx) {
            entry.stop.store(true, Ordering::Relaxed);
        }
    }
    Ok(JsValue::undefined())
}

/// Drain incoming messages from every worker and dispatch
/// `onmessage` handlers on the main thread.
pub fn drain_worker_messages(ctx: &mut Context) {
    let Some(registry) = JS_WORKERS.with(|r| r.borrow().clone()) else {
        return;
    };
    let snapshots: Vec<(usize, Vec<String>)> = {
        let reg = registry.borrow();
        reg.iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                slot.as_ref().map(|entry| {
                    let msgs: Vec<String> = entry
                        .incoming
                        .lock()
                        .map(|mut q| q.drain(..).collect())
                        .unwrap_or_default();
                    (i, msgs)
                })
            })
            .filter(|(_, msgs)| !msgs.is_empty())
            .collect()
    };
    for (idx, messages) in snapshots {
        for msg in messages {
            dispatch_message(ctx, &registry, idx, msg);
        }
    }
}

fn dispatch_message(ctx: &mut Context, registry: &WorkerRegistry, idx: usize, data: String) {
    let handle = {
        let borrow = registry.borrow();
        borrow.get(idx).and_then(|s| s.as_ref()).and_then(|e| e.handle.clone())
    };
    let Some(handle) = handle else { return };
    let Ok(handler_val) = handle.get(js_string!("onmessage"), ctx) else {
        return;
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return;
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return;
    };
    let event_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("data"),
            JsValue::from(js_string!(data)),
            Attribute::READONLY,
        )
        .build();
    let _ = handler.call(
        &JsValue::from(handle),
        &[JsValue::from(event_obj)],
        ctx,
    );
}

// ============ Worker-side global setup ============

fn install_worker_globals(ctx: &mut Context) {
    super::install_console(ctx);
    // `self` is an object with postMessage + onmessage.
    let realm = ctx.realm().clone();
    let _ = realm;
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!("onmessage"), JsValue::null(), Attribute::all());
    b.function(
        NativeFunction::from_fn_ptr(worker_self_post_message),
        js_string!("postMessage"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(worker_self_close),
        js_string!("close"),
        0,
    );
    let self_obj = b.build();
    let global = ctx.global_object();
    let _ = global.set(
        js_string!("self"),
        JsValue::from(self_obj.clone()),
        false,
        ctx,
    );
    // Also expose `postMessage` / `onmessage` as bare globals so code
    // that doesn't qualify with `self.` works too.
    let _ = global.set(
        js_string!("postMessage"),
        self_obj.get(js_string!("postMessage"), ctx).unwrap_or(JsValue::undefined()),
        false,
        ctx,
    );
}

fn worker_self_post_message(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(msg) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let payload = msg.to_string(ctx)?.to_std_string_escaped();
    WORKER_OUTBOUND.with(|slot| {
        if let Some(q) = slot.borrow().as_ref() {
            if let Ok(mut q) = q.lock() {
                q.push_back(payload);
            }
        }
    });
    Ok(JsValue::undefined())
}

fn worker_self_close(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    WORKER_STOP.with(|slot| {
        if let Some(s) = slot.borrow().as_ref() {
            s.store(true, Ordering::Relaxed);
        }
    });
    Ok(JsValue::undefined())
}

/// Worker thread main loop: drain inbound channel, invoke
/// `self.onmessage` for each message until `stop` is set.
fn worker_pump(ctx: &mut Context, stop: &Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Try to receive with a short timeout so we exit promptly on stop.
        let msg = WORKER_INBOUND.with(|slot| {
            let guard = slot.borrow();
            guard
                .as_ref()
                .and_then(|rx| rx.recv_timeout(std::time::Duration::from_millis(100)).ok())
        });
        let Some(msg) = msg else { continue };
        let global = ctx.global_object();
        let self_val = match global.get(js_string!("self"), ctx) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(self_obj) = self_val.as_object() else { continue };
        let Ok(handler_val) = self_obj.get(js_string!("onmessage"), ctx) else {
            continue;
        };
        let Some(handler_obj) = handler_val.as_object() else { continue };
        let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
            continue;
        };
        let event_obj = ObjectInitializer::new(ctx)
            .property(
                js_string!("data"),
                JsValue::from(js_string!(msg)),
                Attribute::READONLY,
            )
            .build();
        let this_val = JsValue::from(self_obj.clone());
        let _ = handler.call(&this_val, &[JsValue::from(event_obj)], ctx);
        ctx.run_jobs();
    }
}
