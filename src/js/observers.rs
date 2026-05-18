//! Toy implementations of the three callback observers most frameworks
//! reach for.
//!
//! **MutationObserver** — fully functional for JS-initiated DOM
//! mutations. Each DOM-mutating shim in [`super::dom`] enqueues a
//! `MutationRecord` via [`push_mutation_record`]. The engine drains
//! the queue after every dispatch and fires the matching observers'
//! callbacks with a batch.
//!
//! **IntersectionObserver** — threshold-driven. Each observer
//! carries an `Options.threshold` array (default `[0]`) and a
//! per-target last-reported ratio. After every layout pass,
//! [`tick_layout_observers`] recomputes intersection of every
//! observed target against the viewport (the implicit root). It
//! fires the callback whenever:
//!  * the boolean `isIntersecting` flipped, OR
//!  * the ratio crossed any threshold in the configured list.
//! This is what makes infinite-scroll / lazy-load libraries work.
//!
//! **ResizeObserver** — also threshold-aware. Records each target's
//! last seen `(width, height)` and fires when the layout's current
//! box size differs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;
use crate::layout::BoxTree;

use super::dom as js_dom;

#[derive(Debug, Clone)]
pub struct MutationRecord {
    pub kind: MutationKind,
    pub target: NodeId,
    pub attribute_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Attributes,
    CharacterData,
    ChildList,
}

#[derive(Default)]
pub(crate) struct ObserverState {
    /// Pending mutation records emitted by DOM mutators since the
    /// previous drain.
    pub(crate) pending: Vec<MutationRecord>,
    pub(crate) mutation_observers: Vec<MutationObserverEntry>,
    pub(crate) intersection_observers: Vec<IntersectionObserverEntry>,
    pub(crate) resize_observers: Vec<ResizeObserverEntry>,
}

pub(crate) struct MutationObserverEntry {
    pub(crate) callback: JsFunction,
    /// Each (target, options) pair from `observe()` calls.
    pub(crate) watches: Vec<MutationWatch>,
}

pub(crate) struct MutationWatch {
    pub(crate) target: NodeId,
    pub(crate) attributes: bool,
    pub(crate) child_list: bool,
    pub(crate) character_data: bool,
    pub(crate) subtree: bool,
}

pub(crate) struct IntersectionObserverEntry {
    pub(crate) callback: JsFunction,
    pub(crate) targets: Vec<NodeId>,
    /// Sorted, deduped list of thresholds in [0, 1]. Defaults to
    /// `[0.0]` when the page didn't pass a threshold option.
    pub(crate) thresholds: Vec<f64>,
    /// Last-reported state per target, used to detect threshold
    /// crossings. `(was_intersecting, last_ratio)`.
    pub(crate) last_state: HashMap<NodeId, (bool, f64)>,
}

pub(crate) struct ResizeObserverEntry {
    pub(crate) callback: JsFunction,
    pub(crate) targets: Vec<NodeId>,
    /// Last-reported `(width, height)` per target so we only fire
    /// when the box actually changed size.
    pub(crate) last_size: HashMap<NodeId, (f32, f32)>,
}

thread_local! {
    /// Active observer state during script / event / timer / rAF
    /// execution. Set by `JsEngine::install_thread_locals` and cleared
    /// on uninstall.
    pub(crate) static JS_OBSERVERS: RefCell<Option<Rc<RefCell<ObserverState>>>> =
        const { RefCell::new(None) };
}

/// Append a mutation record to the engine's queue. Called from the
/// DOM-mutating JS shims (setAttribute / textContent= / appendChild /
/// etc.). No-op when no observer state is installed (so unit tests
/// that don't spin up an engine just work).
pub fn push_mutation_record(record: MutationRecord) {
    JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().pending.push(record);
        }
    });
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("MutationObserver"),
        1,
        NativeFunction::from_fn_ptr(mutation_observer_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("IntersectionObserver"),
        2,
        NativeFunction::from_fn_ptr(intersection_observer_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("ResizeObserver"),
        1,
        NativeFunction::from_fn_ptr(resize_observer_ctor),
    )
    .ok();
}

// ============= MutationObserver =============

fn mutation_observer_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(cb) = args.first().and_then(extract_fn) else {
        return Ok(JsValue::null());
    };
    let idx = JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            let mut s = rc.borrow_mut();
            s.mutation_observers.push(MutationObserverEntry {
                callback: cb,
                watches: Vec::new(),
            });
            s.mutation_observers.len() - 1
        } else {
            usize::MAX
        }
    });

    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };
    let _ = getter;

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("__obs_idx"),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(mutation_observer_observe),
        js_string!("observe"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(mutation_observer_disconnect),
        js_string!("disconnect"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(mutation_observer_take_records),
        js_string!("takeRecords"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn mutation_observer_observe(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(idx) = read_obs_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(target_node) = node_id_from_handle(args.first(), ctx) else {
        return Ok(JsValue::undefined());
    };
    let opts = args.get(1);
    let mut watch = MutationWatch {
        target: target_node,
        attributes: false,
        child_list: false,
        character_data: false,
        subtree: false,
    };
    if let Some(opts_val) = opts {
        if let Some(obj) = opts_val.as_object() {
            watch.attributes = obj
                .get(js_string!("attributes"), ctx)
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            watch.child_list = obj
                .get(js_string!("childList"), ctx)
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            watch.character_data = obj
                .get(js_string!("characterData"), ctx)
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            watch.subtree = obj
                .get(js_string!("subtree"), ctx)
                .map(|v| v.to_boolean())
                .unwrap_or(false);
        }
    }
    // Default behaviour without an options object — observe everything.
    if !watch.attributes && !watch.child_list && !watch.character_data {
        watch.attributes = true;
        watch.child_list = true;
        watch.character_data = true;
    }
    JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            if let Some(entry) = rc.borrow_mut().mutation_observers.get_mut(idx) {
                entry.watches.push(watch);
            }
        }
    });
    Ok(JsValue::undefined())
}

fn mutation_observer_disconnect(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = read_obs_idx(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            if let Some(entry) = rc.borrow_mut().mutation_observers.get_mut(idx) {
                entry.watches.clear();
            }
        }
    });
    Ok(JsValue::undefined())
}

fn mutation_observer_take_records(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsArray::new(ctx).into())
}

// ============= IntersectionObserver =============

fn intersection_observer_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(cb) = args.first().and_then(extract_fn) else {
        return Ok(JsValue::null());
    };
    // Parse `options.threshold` — may be a single number or an
    // array of numbers in [0, 1]. Default per spec is [0].
    let thresholds = parse_thresholds(args.get(1), ctx);
    let idx = JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            let mut s = rc.borrow_mut();
            s.intersection_observers.push(IntersectionObserverEntry {
                callback: cb,
                targets: Vec::new(),
                thresholds,
                last_state: HashMap::new(),
            });
            s.intersection_observers.len() - 1
        } else {
            usize::MAX
        }
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("__intersect_idx"),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(intersection_observe),
        js_string!("observe"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(intersection_unobserve),
        js_string!("unobserve"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(intersection_disconnect),
        js_string!("disconnect"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn intersection_observe(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let (Some(idx), Some(target)) = (
        read_intersect_idx(this, ctx),
        node_id_from_handle(args.first(), ctx),
    ) {
        let mut fire_synchronously = false;
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().intersection_observers.get_mut(idx) {
                    if !entry.targets.contains(&target) {
                        entry.targets.push(target);
                        // Seed last_state with (true, 1.0) so the
                        // first layout tick only re-fires when the
                        // real geometry actually differs. Spec
                        // schedules the initial callback as a
                        // microtask; we fire synchronously which is
                        // close enough for the toy.
                        entry.last_state.insert(target, (true, 1.0));
                        fire_synchronously = true;
                    }
                }
            }
        });
        if fire_synchronously {
            fire_intersection_batch(idx, &[(target, true, 1.0)], ctx);
        }
    }
    Ok(JsValue::undefined())
}

fn intersection_unobserve(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let (Some(idx), Some(target)) = (
        read_intersect_idx(this, ctx),
        node_id_from_handle(args.first(), ctx),
    ) {
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().intersection_observers.get_mut(idx) {
                    entry.targets.retain(|n| n != &target);
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn intersection_disconnect(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(idx) = read_intersect_idx(this, ctx) {
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().intersection_observers.get_mut(idx) {
                    entry.targets.clear();
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

// ============= ResizeObserver =============

fn resize_observer_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(cb) = args.first().and_then(extract_fn) else {
        return Ok(JsValue::null());
    };
    let idx = JS_OBSERVERS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            let mut s = rc.borrow_mut();
            s.resize_observers.push(ResizeObserverEntry {
                callback: cb,
                targets: Vec::new(),
                last_size: HashMap::new(),
            });
            s.resize_observers.len() - 1
        } else {
            usize::MAX
        }
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("__resize_idx"),
        JsValue::from(idx as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(resize_observe),
        js_string!("observe"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(resize_unobserve),
        js_string!("unobserve"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(resize_disconnect),
        js_string!("disconnect"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn resize_observe(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let (Some(idx), Some(target)) = (
        read_resize_idx(this, ctx),
        node_id_from_handle(args.first(), ctx),
    ) {
        let mut fire_synchronously = false;
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().resize_observers.get_mut(idx) {
                    if !entry.targets.contains(&target) {
                        entry.targets.push(target);
                        entry.last_size.insert(target, (0.0, 0.0));
                        fire_synchronously = true;
                    }
                }
            }
        });
        if fire_synchronously {
            fire_resize_batch(idx, &[(target, 0.0, 0.0)], ctx);
        }
    }
    Ok(JsValue::undefined())
}

fn resize_unobserve(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let (Some(idx), Some(target)) = (
        read_resize_idx(this, ctx),
        node_id_from_handle(args.first(), ctx),
    ) {
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().resize_observers.get_mut(idx) {
                    entry.targets.retain(|n| n != &target);
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

fn resize_disconnect(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(idx) = read_resize_idx(this, ctx) {
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().resize_observers.get_mut(idx) {
                    entry.targets.clear();
                }
            }
        });
    }
    Ok(JsValue::undefined())
}

// ============= helpers + drain =============

fn parse_thresholds(opts: Option<&JsValue>, ctx: &mut Context) -> Vec<f64> {
    let default = vec![0.0_f64];
    let Some(opts) = opts else { return default };
    let Some(obj) = opts.as_object() else { return default };
    let Ok(t) = obj.get(js_string!("threshold"), ctx) else {
        return default;
    };
    if t.is_undefined() || t.is_null() {
        return default;
    }
    // Either an array or a single number.
    if let Some(o) = t.as_object() {
        if let Ok(arr) = boa_engine::object::builtins::JsArray::from_object(o.clone()) {
            let len = arr.length(ctx).unwrap_or(0) as usize;
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                if let Ok(v) = arr.get(i as u64, ctx) {
                    if let Ok(n) = v.to_number(ctx) {
                        if (0.0..=1.0).contains(&n) {
                            out.push(n);
                        }
                    }
                }
            }
            if out.is_empty() {
                return default;
            }
            out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            out.dedup();
            return out;
        }
    }
    if let Ok(n) = t.to_number(ctx) {
        if (0.0..=1.0).contains(&n) {
            return vec![n];
        }
    }
    default
}

fn extract_fn(v: &JsValue) -> Option<JsFunction> {
    let obj = v.as_object()?;
    JsFunction::from_object(obj.clone())
}

fn read_obs_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!("__obs_idx"), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn read_intersect_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!("__intersect_idx"), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn read_resize_idx(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!("__resize_idx"), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn node_id_from_handle(val: Option<&JsValue>, ctx: &mut Context) -> Option<NodeId> {
    let v = val?;
    let obj = v.as_object()?;
    let raw = obj.get(js_string!("__nodeId"), ctx).ok()?;
    Some(NodeId::from_raw(raw.to_u32(ctx).ok()?))
}

/// Drain the pending mutation queue and dispatch records to every
/// registered MutationObserver whose watch list matches. Called by the
/// engine after each script / event / timer / rAF tick.
pub fn drain_mutation_records(ctx: &mut Context) {
    let (records, callbacks) = JS_OBSERVERS.with(|slot| -> (Vec<MutationRecord>, Vec<(JsFunction, Vec<MutationRecord>)>) {
        let Some(rc) = slot.borrow().as_ref().cloned() else {
            return (Vec::new(), Vec::new());
        };
        let mut state = rc.borrow_mut();
        if state.pending.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let drained = std::mem::take(&mut state.pending);
        let mut callbacks: Vec<(JsFunction, Vec<MutationRecord>)> = Vec::new();
        for obs in &state.mutation_observers {
            let mut hits: Vec<MutationRecord> = Vec::new();
            for rec in &drained {
                if obs
                    .watches
                    .iter()
                    .any(|w| watch_matches(w, rec))
                {
                    hits.push(rec.clone());
                }
            }
            if !hits.is_empty() {
                callbacks.push((obs.callback.clone(), hits));
            }
        }
        (drained, callbacks)
    });
    let _ = records;

    for (cb, recs) in callbacks {
        let arr = JsArray::new(ctx);
        for rec in recs {
            let kind = match rec.kind {
                MutationKind::Attributes => "attributes",
                MutationKind::CharacterData => "characterData",
                MutationKind::ChildList => "childList",
            };
            let target_handle = js_dom::make_element_handle(ctx, rec.target);
            let mut b = ObjectInitializer::new(ctx);
            b.property(
                js_string!("type"),
                JsValue::from(js_string!(kind)),
                Attribute::READONLY,
            );
            b.property(
                js_string!("target"),
                JsValue::from(target_handle),
                Attribute::READONLY,
            );
            if let Some(name) = rec.attribute_name {
                b.property(
                    js_string!("attributeName"),
                    JsValue::from(js_string!(name)),
                    Attribute::READONLY,
                );
            }
            let _ = arr.push(JsValue::from(b.build()), ctx);
        }
        let _ = cb.call(&JsValue::undefined(), &[arr.into()], ctx);
    }
}

/// Recompute IntersectionObserver / ResizeObserver state against
/// the current layout box tree. Should be called after every layout
/// pass — initial render, scroll, resize, DOM mutation that
/// invalidates layout.
///
/// Fires JS callbacks for any observers whose target crossed a
/// threshold (Intersection) or changed size (Resize) since the
/// previous tick.
pub fn tick_layout_observers(box_tree: &BoxTree, ctx: &mut Context) {
    // Snapshot the targets + thresholds we need from the observer
    // state, compute fresh ratios, then commit updates + collect
    // callbacks-to-fire. Doing it in two passes keeps the borrow on
    // the observer state away from `cb.call()` which may need it
    // again (timers, nested observers, etc.).
    let viewport = box_tree.viewport;
    let snapshot = JS_OBSERVERS.with(|slot| -> Option<ObserverSnapshot> {
        let rc = slot.borrow().as_ref().cloned()?;
        let s = rc.borrow();
        let intersect = s
            .intersection_observers
            .iter()
            .enumerate()
            .map(|(idx, e)| IntersectSnap {
                idx,
                targets: e.targets.clone(),
                thresholds: e.thresholds.clone(),
                last_state: e.last_state.clone(),
            })
            .collect();
        let resize = s
            .resize_observers
            .iter()
            .enumerate()
            .map(|(idx, e)| ResizeSnap {
                idx,
                targets: e.targets.clone(),
                last_size: e.last_size.clone(),
            })
            .collect();
        Some(ObserverSnapshot { intersect, resize })
    });
    let Some(snapshot) = snapshot else { return };

    // ---- Intersection ----
    for snap in &snapshot.intersect {
        let mut to_fire: Vec<(NodeId, bool, f64)> = Vec::new();
        let mut new_state: HashMap<NodeId, (bool, f64)> = snap.last_state.clone();
        for target in &snap.targets {
            let Some(b) = box_tree.get(*target) else {
                continue;
            };
            let ratio = intersection_ratio(b.rect, viewport);
            let is_intersecting = ratio > 0.0;
            let prev = snap.last_state.get(target).copied();
            let crossed = match prev {
                None => true, // first observation always fires
                Some((prev_intersecting, prev_ratio)) => {
                    prev_intersecting != is_intersecting
                        || crossed_threshold(&snap.thresholds, prev_ratio, ratio)
                }
            };
            if crossed {
                to_fire.push((*target, is_intersecting, ratio));
            }
            new_state.insert(*target, (is_intersecting, ratio));
        }
        if to_fire.is_empty() {
            continue;
        }
        // Commit new state.
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(e) =
                    rc.borrow_mut().intersection_observers.get_mut(snap.idx)
                {
                    e.last_state = new_state;
                }
            }
        });
        // Fire callback once with all matching entries.
        fire_intersection_batch(snap.idx, &to_fire, ctx);
    }

    // ---- Resize ----
    for snap in &snapshot.resize {
        let mut to_fire: Vec<(NodeId, f32, f32)> = Vec::new();
        let mut new_sizes: HashMap<NodeId, (f32, f32)> = snap.last_size.clone();
        for target in &snap.targets {
            let Some(b) = box_tree.get(*target) else {
                continue;
            };
            let (w, h) = (b.rect.width, b.rect.height);
            let prev = snap.last_size.get(target).copied();
            let changed = match prev {
                None => true,
                Some((pw, ph)) => (pw - w).abs() > 0.5 || (ph - h).abs() > 0.5,
            };
            if changed {
                to_fire.push((*target, w, h));
            }
            new_sizes.insert(*target, (w, h));
        }
        if to_fire.is_empty() {
            continue;
        }
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(e) = rc.borrow_mut().resize_observers.get_mut(snap.idx) {
                    e.last_size = new_sizes;
                }
            }
        });
        fire_resize_batch(snap.idx, &to_fire, ctx);
    }
}

struct ObserverSnapshot {
    intersect: Vec<IntersectSnap>,
    resize: Vec<ResizeSnap>,
}

struct IntersectSnap {
    idx: usize,
    targets: Vec<NodeId>,
    thresholds: Vec<f64>,
    last_state: HashMap<NodeId, (bool, f64)>,
}

struct ResizeSnap {
    idx: usize,
    targets: Vec<NodeId>,
    last_size: HashMap<NodeId, (f32, f32)>,
}

fn intersection_ratio(target: crate::layout::Rect, root: crate::layout::Rect) -> f64 {
    let target_area = (target.width as f64).max(0.0) * (target.height as f64).max(0.0);
    if target_area <= 0.0 {
        return 0.0;
    }
    let ix = target.x.max(root.x);
    let iy = target.y.max(root.y);
    let ir = (target.x + target.width).min(root.x + root.width);
    let ib = (target.y + target.height).min(root.y + root.height);
    let iw = (ir - ix).max(0.0);
    let ih = (ib - iy).max(0.0);
    let inter = iw as f64 * ih as f64;
    (inter / target_area).clamp(0.0, 1.0)
}

fn crossed_threshold(thresholds: &[f64], prev: f64, curr: f64) -> bool {
    let (lo, hi) = if prev <= curr { (prev, curr) } else { (curr, prev) };
    thresholds.iter().any(|&t| t >= lo && t <= hi && (t - prev).abs() > f64::EPSILON)
}

fn fire_intersection_batch(idx: usize, hits: &[(NodeId, bool, f64)], ctx: &mut Context) {
    let cb = JS_OBSERVERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| {
                rc.borrow()
                    .intersection_observers
                    .get(idx)
                    .map(|e| e.callback.clone())
            })
    });
    let Some(cb) = cb else { return };
    let entries = JsArray::new(ctx);
    for (target, is_intersecting, ratio) in hits {
        let target_handle = js_dom::make_element_handle(ctx, *target);
        let entry = ObjectInitializer::new(ctx)
            .property(
                js_string!("isIntersecting"),
                JsValue::from(*is_intersecting),
                Attribute::READONLY,
            )
            .property(
                js_string!("intersectionRatio"),
                JsValue::from(*ratio),
                Attribute::READONLY,
            )
            .property(
                js_string!("target"),
                JsValue::from(target_handle),
                Attribute::READONLY,
            )
            .build();
        let _ = entries.push(JsValue::from(entry), ctx);
    }
    let _ = cb.call(&JsValue::undefined(), &[entries.into()], ctx);
}

fn fire_resize_batch(idx: usize, hits: &[(NodeId, f32, f32)], ctx: &mut Context) {
    let cb = JS_OBSERVERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().resize_observers.get(idx).map(|e| e.callback.clone()))
    });
    let Some(cb) = cb else { return };
    let entries = JsArray::new(ctx);
    for (target, w, h) in hits {
        let target_handle = js_dom::make_element_handle(ctx, *target);
        let content_rect = ObjectInitializer::new(ctx)
            .property(
                js_string!("width"),
                JsValue::from(*w as f64),
                Attribute::READONLY,
            )
            .property(
                js_string!("height"),
                JsValue::from(*h as f64),
                Attribute::READONLY,
            )
            .build();
        let entry = ObjectInitializer::new(ctx)
            .property(
                js_string!("target"),
                JsValue::from(target_handle),
                Attribute::READONLY,
            )
            .property(
                js_string!("contentRect"),
                JsValue::from(content_rect),
                Attribute::READONLY,
            )
            .build();
        let _ = entries.push(JsValue::from(entry), ctx);
    }
    let _ = cb.call(&JsValue::undefined(), &[entries.into()], ctx);
}

fn watch_matches(watch: &MutationWatch, rec: &MutationRecord) -> bool {
    let kind_match = match rec.kind {
        MutationKind::Attributes => watch.attributes,
        MutationKind::CharacterData => watch.character_data,
        MutationKind::ChildList => watch.child_list,
    };
    if !kind_match {
        return false;
    }
    // Subtree matching needs DOM walking; for the toy we treat
    // `subtree: true` as "all targets", which is permissive but won't
    // cause false negatives.
    if watch.subtree {
        return true;
    }
    rec.target == watch.target
}
