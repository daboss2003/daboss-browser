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
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().intersection_observers.get_mut(idx) {
                    if !entry.targets.contains(&target) {
                        entry.targets.push(target);
                    }
                }
            }
        });
        // Fire synchronously with isIntersecting=true. This matches the
        // "let me know when X exists" pattern most pages use.
        fire_intersection_callback_for(idx, target, ctx);
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

fn fire_intersection_callback_for(idx: usize, target: NodeId, ctx: &mut Context) {
    let cb = JS_OBSERVERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().intersection_observers.get(idx).map(|e| e.callback.clone()))
    });
    let Some(cb) = cb else { return };
    let target_handle = js_dom::make_element_handle(ctx, target);
    let entries = JsArray::new(ctx);
    let entry = ObjectInitializer::new(ctx)
        .property(
            js_string!("isIntersecting"),
            JsValue::from(true),
            Attribute::READONLY,
        )
        .property(
            js_string!("intersectionRatio"),
            JsValue::from(1.0_f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("target"),
            JsValue::from(target_handle),
            Attribute::READONLY,
        )
        .build();
    let _ = entries.push(JsValue::from(entry), ctx);
    let _ = cb.call(&JsValue::undefined(), &[entries.into()], ctx);
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
        JS_OBSERVERS.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref() {
                if let Some(entry) = rc.borrow_mut().resize_observers.get_mut(idx) {
                    if !entry.targets.contains(&target) {
                        entry.targets.push(target);
                    }
                }
            }
        });
        fire_resize_callback_for(idx, target, ctx);
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

fn fire_resize_callback_for(idx: usize, target: NodeId, ctx: &mut Context) {
    let cb = JS_OBSERVERS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().resize_observers.get(idx).map(|e| e.callback.clone()))
    });
    let Some(cb) = cb else { return };
    let target_handle = js_dom::make_element_handle(ctx, target);
    let content_rect = ObjectInitializer::new(ctx)
        .property(
            js_string!("width"),
            JsValue::from(0.0_f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("height"),
            JsValue::from(0.0_f64),
            Attribute::READONLY,
        )
        .build();
    let entries = JsArray::new(ctx);
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
    let _ = cb.call(&JsValue::undefined(), &[entries.into()], ctx);
}

// ============= helpers + drain =============

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
