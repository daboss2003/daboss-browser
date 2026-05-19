//! SharedWorker (toy).
//!
//! One OS thread per `(url, name)` pair. Subsequent
//! `new SharedWorker(url, name)` calls reuse the same thread,
//! handing the page a fresh `MessagePort` that connects to it.
//! Each page sees an `onconnect` event on the worker side carrying
//! a port; pages use `port.postMessage(...)` / `port.onmessage` to
//! talk to the worker.
//!
//! Wire format: one main→worker `mpsc::Sender<WorkerInbox>` carries
//! both `Connect` notifications and per-port `Message` payloads,
//! each tagged with a port id. Worker→main goes through one
//! shared `VecDeque<(port_id, data)>` so the engine drains all
//! ports in a single sweep.
//!
//! Out of scope: structured-clone transferables, importScripts,
//! `port.close()` revoking the worker's view of the port (we
//! silently swallow subsequent sends), `SharedWorker.onerror`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction, Source,
};

/// Main→worker traffic. Connect creates a new port on the worker
/// side; Message routes through an existing port id.
enum WorkerInbox {
    Connect { port_id: u64 },
    Message { port_id: u64, data: String },
    Close { port_id: u64 },
}

pub struct SharedWorkerEntry {
    /// Sender shared with every port created against this worker.
    pub outgoing: mpsc::Sender<WorkerInbox>,
    /// Worker→main inbox. Drained by `drain_shared_worker_messages`.
    pub incoming: Arc<Mutex<VecDeque<(u64, String)>>>,
    /// Set true to ask the worker thread to exit.
    pub stop: Arc<AtomicBool>,
    /// Per-port JS handles on the main side, keyed by port id.
    pub port_handles: HashMap<u64, JsObject>,
    _thread: JoinHandle<()>,
}

pub type SharedWorkerRegistry = Rc<RefCell<HashMap<(String, String), SharedWorkerEntry>>>;

thread_local! {
    pub(crate) static JS_SHARED_WORKERS: RefCell<Option<SharedWorkerRegistry>> =
        const { RefCell::new(None) };
    /// Inside the worker thread's Context: the inbox + the
    /// shared outbox so the global `postMessage`/port methods can
    /// find them.
    static SW_THREAD_INBOX: RefCell<Option<mpsc::Receiver<WorkerInbox>>> = RefCell::new(None);
    static SW_THREAD_OUTBOX: RefCell<Option<Arc<Mutex<VecDeque<(u64, String)>>>>> =
        RefCell::new(None);
    static SW_THREAD_STOP: RefCell<Option<Arc<AtomicBool>>> = RefCell::new(None);
    /// On the worker side: port_id → port JS object so incoming
    /// `Message`s can fire the right `onmessage`.
    static SW_THREAD_PORTS: RefCell<HashMap<u64, JsObject>> = RefCell::new(HashMap::new());
}

static NEXT_PORT_ID: AtomicU64 = AtomicU64::new(1);

const SW_KEY: &str = "__sw_key";
const PORT_ID_KEY: &str = "__sw_port_id";
const PORT_SIDE_KEY: &str = "__sw_port_side";

fn next_port_id() -> u64 {
    NEXT_PORT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("SharedWorker"),
        1,
        NativeFunction::from_fn_ptr(shared_worker_ctor),
    )
    .ok();
    // `MessagePort` is also exposed as a global type for
    // `instanceof` checks. Methods are wired per-instance.
    let ctor = boa_engine::object::FunctionObjectBuilder::new(
        &ctx.realm().clone(),
        NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::null())),
    )
    .build();
    let _ = ctx.register_global_property(
        js_string!("MessagePort"),
        JsValue::from(ctor),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn shared_worker_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(url_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let url = url_val.to_string(ctx)?.to_std_string_escaped();
    let name = args
        .get(1)
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();

    let Some(registry) = JS_SHARED_WORKERS.with(|r| r.borrow().clone()) else {
        return Ok(JsValue::null());
    };

    // Resolve the URL up front so the cache key is consistent.
    let base = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let Some(resolved) = (match base {
        Some(b) => b.join(&url).ok(),
        None => url::Url::parse(&url).ok(),
    }) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message(format!("SharedWorker: invalid URL {url}"))
            .into());
    };
    let key = (resolved.to_string(), name.clone());

    // Spawn the worker thread on first observation of this key.
    if !registry.borrow().contains_key(&key) {
        let client = super::engine::JS_FETCH_CLIENT.with(|c| c.borrow().clone());
        let source = match client {
            Some(c) => match c.get(&resolved.to_string()) {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    String::from_utf8_lossy(&resp.body).into_owned()
                }
                Ok(resp) => {
                    return Err(boa_engine::JsNativeError::error()
                        .with_message(format!("SharedWorker fetch HTTP {}", resp.status))
                        .into());
                }
                Err(e) => {
                    return Err(boa_engine::JsNativeError::error()
                        .with_message(format!("SharedWorker fetch: {e}"))
                        .into());
                }
            },
            None => {
                return Err(boa_engine::JsNativeError::error()
                    .with_message("SharedWorker: no fetch client installed")
                    .into());
            }
        };

        let (tx, rx) = mpsc::channel::<WorkerInbox>();
        let incoming: Arc<Mutex<VecDeque<(u64, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let incoming_for_thread = incoming.clone();
        let stop_for_thread = stop.clone();
        let thread = std::thread::spawn(move || {
            SW_THREAD_INBOX.with(|s| *s.borrow_mut() = Some(rx));
            SW_THREAD_OUTBOX.with(|s| *s.borrow_mut() = Some(incoming_for_thread));
            SW_THREAD_STOP.with(|s| *s.borrow_mut() = Some(stop_for_thread.clone()));
            let mut wctx = Context::default();
            install_worker_globals(&mut wctx);
            if let Err(e) = wctx.eval(Source::from_bytes(source.as_bytes())) {
                eprintln!("[shared-worker] init script threw: {e}");
            }
            shared_worker_pump(&mut wctx, &stop_for_thread);
        });

        registry.borrow_mut().insert(
            key.clone(),
            SharedWorkerEntry {
                outgoing: tx,
                incoming,
                stop,
                port_handles: HashMap::new(),
                _thread: thread,
            },
        );
    }

    // Allocate a fresh port for this connection and tell the worker
    // a new port is online so it can fire `onconnect`.
    let port_id = next_port_id();
    let (port_handle, sw_handle) = {
        let mut reg = registry.borrow_mut();
        let entry = reg.get_mut(&key).expect("entry just inserted");
        let _ = entry
            .outgoing
            .send(WorkerInbox::Connect { port_id });
        let port_obj = build_port_handle(ctx, &key, port_id, /* is_main_side */ true);
        entry.port_handles.insert(port_id, port_obj.clone());
        let sw_obj = build_shared_worker_handle(ctx, &key, port_obj.clone());
        (port_obj, sw_obj)
    };
    let _ = port_handle;
    Ok(JsValue::from(sw_handle))
}

fn build_shared_worker_handle(
    ctx: &mut Context,
    key: &(String, String),
    port: JsObject,
) -> JsObject {
    ObjectInitializer::new(ctx)
        .property(
            js_string!(SW_KEY),
            JsValue::from(js_string!(format!("{}|{}", key.0, key.1))),
            Attribute::READONLY,
        )
        .property(js_string!("port"), JsValue::from(port), Attribute::READONLY)
        .property(js_string!("onerror"), JsValue::null(), Attribute::all())
        .build()
}

fn build_port_handle(
    ctx: &mut Context,
    key: &(String, String),
    port_id: u64,
    is_main_side: bool,
) -> JsObject {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(SW_KEY),
        JsValue::from(js_string!(format!("{}|{}", key.0, key.1))),
        Attribute::READONLY,
    );
    b.property(
        js_string!(PORT_ID_KEY),
        JsValue::from(port_id as u32),
        Attribute::READONLY,
    );
    b.property(
        js_string!(PORT_SIDE_KEY),
        JsValue::from(is_main_side),
        Attribute::READONLY,
    );
    b.property(js_string!("onmessage"), JsValue::null(), Attribute::all());
    b.property(js_string!("onmessageerror"), JsValue::null(), Attribute::all());
    b.function(
        NativeFunction::from_fn_ptr(port_post_message),
        js_string!("postMessage"),
        1,
    );
    b.function(NativeFunction::from_fn_ptr(port_start), js_string!("start"), 0);
    b.function(NativeFunction::from_fn_ptr(port_close), js_string!("close"), 0);
    b.build()
}

fn read_port_key(this: &JsValue, ctx: &mut Context) -> Option<((String, String), u64, bool)> {
    let obj = this.as_object()?;
    let key_str = obj
        .get(js_string!(SW_KEY), ctx)
        .ok()?
        .to_string(ctx)
        .ok()?
        .to_std_string_escaped();
    let (url, name) = key_str.split_once('|').unwrap_or(("", ""));
    let port_id = obj.get(js_string!(PORT_ID_KEY), ctx).ok()?.to_u32(ctx).ok()? as u64;
    let is_main = obj
        .get(js_string!(PORT_SIDE_KEY), ctx)
        .ok()?
        .to_boolean();
    Some(((url.to_string(), name.to_string()), port_id, is_main))
}

fn port_post_message(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(msg) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let payload = msg.to_string(ctx)?.to_std_string_escaped();
    let Some((key, port_id, is_main_side)) = read_port_key(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    if is_main_side {
        // Main → worker.
        if let Some(reg) = JS_SHARED_WORKERS.with(|r| r.borrow().clone()) {
            if let Some(entry) = reg.borrow().get(&key) {
                let _ = entry.outgoing.send(WorkerInbox::Message {
                    port_id,
                    data: payload,
                });
            }
        }
    } else {
        // Worker → main. Drop into the worker's outbox.
        SW_THREAD_OUTBOX.with(|slot| {
            if let Some(q) = slot.borrow().as_ref() {
                if let Ok(mut q) = q.lock() {
                    q.push_back((port_id, payload));
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn port_start(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Spec semantics: required call to begin dispatching queued
    // messages. We dispatch on arrival, so start() is a no-op.
    Ok(JsValue::undefined())
}

fn port_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some((key, port_id, is_main_side)) = read_port_key(this, ctx) {
        if is_main_side {
            if let Some(reg) = JS_SHARED_WORKERS.with(|r| r.borrow().clone()) {
                if let Some(entry) = reg.borrow().get(&key) {
                    let _ = entry.outgoing.send(WorkerInbox::Close { port_id });
                }
            }
        } else {
            SW_THREAD_PORTS.with(|m| {
                m.borrow_mut().remove(&port_id);
            });
        }
    }
    Ok(JsValue::undefined())
}

/// Drain every shared worker's outbox and dispatch port `onmessage`
/// handlers on the main thread.
pub fn drain_shared_worker_messages(ctx: &mut Context) {
    let Some(registry) = JS_SHARED_WORKERS.with(|r| r.borrow().clone()) else {
        return;
    };
    // Snapshot per-worker (key, drained_messages, port_handles) so
    // we can drop the registry borrow before invoking JS.
    let snapshot: Vec<((String, String), Vec<(u64, String)>, HashMap<u64, JsObject>)> = {
        let reg = registry.borrow();
        reg.iter()
            .filter_map(|(key, entry)| {
                let msgs: Vec<(u64, String)> = entry
                    .incoming
                    .lock()
                    .map(|mut q| q.drain(..).collect())
                    .unwrap_or_default();
                if msgs.is_empty() {
                    None
                } else {
                    Some((key.clone(), msgs, entry.port_handles.clone()))
                }
            })
            .collect()
    };
    for (_key, messages, port_handles) in snapshot {
        for (port_id, data) in messages {
            let Some(port) = port_handles.get(&port_id).cloned() else {
                continue;
            };
            dispatch_port_message(ctx, &port, data);
        }
    }
}

fn dispatch_port_message(ctx: &mut Context, port: &JsObject, data: String) {
    let Ok(handler_val) = port.get(js_string!("onmessage"), ctx) else {
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
        &JsValue::from(port.clone()),
        &[JsValue::from(event_obj)],
        ctx,
    );
}

// ============ worker-thread side ============

fn install_worker_globals(ctx: &mut Context) {
    super::install_console(ctx);
    let self_obj = ObjectInitializer::new(ctx)
        .property(js_string!("onconnect"), JsValue::null(), Attribute::all())
        .property(js_string!("name"), JsValue::from(js_string!("")), Attribute::all())
        .function(NativeFunction::from_fn_ptr(worker_self_close), js_string!("close"), 0)
        .build();
    let global = ctx.global_object();
    let _ = global.set(js_string!("self"), JsValue::from(self_obj), false, ctx);
}

fn worker_self_close(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    SW_THREAD_STOP.with(|slot| {
        if let Some(s) = slot.borrow().as_ref() {
            s.store(true, Ordering::Relaxed);
        }
    });
    Ok(JsValue::undefined())
}

fn shared_worker_pump(ctx: &mut Context, stop: &Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let msg = SW_THREAD_INBOX.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|rx| rx.recv_timeout(std::time::Duration::from_millis(100)).ok())
        });
        let Some(msg) = msg else { continue };
        match msg {
            WorkerInbox::Connect { port_id } => handle_connect(ctx, port_id),
            WorkerInbox::Message { port_id, data } => handle_message(ctx, port_id, data),
            WorkerInbox::Close { port_id } => {
                SW_THREAD_PORTS.with(|m| {
                    m.borrow_mut().remove(&port_id);
                });
            }
        }
        ctx.run_jobs();
    }
}

fn handle_connect(ctx: &mut Context, port_id: u64) {
    let port = build_port_handle(
        ctx,
        &("worker-side".to_string(), String::new()),
        port_id,
        /* is_main_side */ false,
    );
    SW_THREAD_PORTS.with(|m| {
        m.borrow_mut().insert(port_id, port.clone());
    });
    // Fire `self.onconnect(event)` where event has `.ports` and
    // `.source` pointing at the new port.
    let global = ctx.global_object();
    let self_val = match global.get(js_string!("self"), ctx) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(self_obj) = self_val.as_object() else { return };
    let Ok(handler_val) = self_obj.get(js_string!("onconnect"), ctx) else {
        return;
    };
    let Some(handler_obj) = handler_val.as_object() else { return };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return;
    };
    let ports = JsArray::new(ctx);
    let _ = ports.push(JsValue::from(port.clone()), ctx);
    let event_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("ports"),
            JsValue::from(ports),
            Attribute::READONLY,
        )
        .property(js_string!("source"), JsValue::from(port), Attribute::READONLY)
        .build();
    let _ = handler.call(&self_val, &[JsValue::from(event_obj)], ctx);
}

fn handle_message(ctx: &mut Context, port_id: u64, data: String) {
    let port = SW_THREAD_PORTS.with(|m| m.borrow().get(&port_id).cloned());
    let Some(port) = port else { return };
    dispatch_port_message(ctx, &port, data);
}
