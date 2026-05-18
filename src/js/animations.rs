//! Web Animations API — `element.animate(keyframes, options)` returns
//! an `Animation` whose state is advanced from the engine tick.
//!
//! Each frame the engine calls [`advance_animations(now_ms)`], which:
//!   1. computes elapsed time per animation (honouring delay,
//!      direction, fill, iterations, playbackRate),
//!   2. interpolates the bracketing keyframes per property using the
//!      configured easing function,
//!   3. writes the resulting CSS to the target element's `style`
//!      attribute, so the next cascade picks the values up
//!      naturally.
//!
//! Spec gaps we accept for the toy:
//!   * `KeyframeEffect.composite` modes — we always use replace.
//!   * `Animation.timeline` is the document timeline only.
//!   * `getAnimations()` lives on `document` and returns the global
//!     registry's running animations.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;

const ANIM_ID_KEY: &str = "__anim_id";

#[derive(Clone)]
pub struct Keyframe {
    /// 0.0..1.0 progress fraction.
    pub offset: f32,
    pub props: Vec<(String, String)>,
}

#[derive(Copy, Clone, Debug)]
pub enum Easing {
    Linear,
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
    StepStart,
    StepEnd,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
    Auto,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlayState {
    Idle,
    Running,
    Paused,
    Finished,
}

pub struct AnimationEntry {
    pub node: NodeId,
    pub keyframes: Vec<Keyframe>,
    pub duration_ms: f32,
    pub iterations: f32,
    pub delay_ms: f32,
    pub easing: Easing,
    pub direction: Direction,
    pub fill: FillMode,
    pub state: PlayState,
    /// `performance.now()` reading when play() was called (or first
    /// auto-played).
    pub start_time_ms: f64,
    /// Accumulated paused offset — when paused, we hold the time
    /// progress that's been consumed so resuming continues from the
    /// same place.
    pub paused_progress_ms: f32,
    pub playback_rate: f32,
    /// Promise pair for `.finished`. Resolved once when the animation
    /// transitions to Finished.
    pub finished_resolve: Option<JsFunction>,
    pub finished_reject: Option<JsFunction>,
    /// JS handle so `.onfinish` / `.oncancel` resolve correctly.
    pub handle: Option<boa_engine::JsObject>,
}

thread_local! {
    pub(crate) static ANIMATIONS: RefCell<HashMap<u32, AnimationEntry>> =
        RefCell::new(HashMap::new());
    pub(crate) static ANIM_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_anim_id() -> u32 {
    ANIM_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(_ctx: &mut Context) {
    // No globals; `element.animate()` lives on element handles
    // (wired in `dom.rs`) and `document.getAnimations()` is wired
    // there too.
}

/// `element.animate(keyframes, options)` — entry point from dom.rs.
pub fn element_animate(
    node: NodeId,
    keyframes_val: &JsValue,
    options_val: &JsValue,
    ctx: &mut Context,
) -> JsValue {
    let keyframes = parse_keyframes(keyframes_val, ctx);
    let (duration_ms, iterations, delay_ms, easing, direction, fill) =
        parse_options(options_val, ctx);
    let now_ms = performance_now_ms(ctx);
    let id = next_anim_id();
    ANIMATIONS.with(|r| {
        r.borrow_mut().insert(
            id,
            AnimationEntry {
                node,
                keyframes,
                duration_ms,
                iterations,
                delay_ms,
                easing,
                direction,
                fill,
                state: PlayState::Running,
                start_time_ms: now_ms,
                paused_progress_ms: 0.0,
                playback_rate: 1.0,
                finished_resolve: None,
                finished_reject: None,
                handle: None,
            },
        );
    });
    build_animation_object(ctx, id)
}

fn performance_now_ms(ctx: &mut Context) -> f64 {
    let global = ctx.global_object();
    let perf = global.get(js_string!("performance"), ctx).ok();
    let now_fn = perf
        .as_ref()
        .and_then(|p| p.as_object())
        .and_then(|p| p.get(js_string!("now"), ctx).ok())
        .and_then(|v| v.as_object().cloned())
        .and_then(JsFunction::from_object);
    match now_fn {
        Some(f) => f
            .call(&JsValue::undefined(), &[], ctx)
            .ok()
            .and_then(|v| v.to_number(ctx).ok())
            .unwrap_or(0.0),
        None => 0.0,
    }
}

fn build_animation_object(ctx: &mut Context, anim_id: u32) -> JsValue {
    let realm = ctx.realm().clone();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(ANIM_ID_KEY), JsValue::from(anim_id), Attribute::READONLY);
    for name in [
        "onfinish",
        "oncancel",
        "onremove",
    ] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("play", NativeFunction::from_fn_ptr(anim_play), 0),
        ("pause", NativeFunction::from_fn_ptr(anim_pause), 0),
        ("cancel", NativeFunction::from_fn_ptr(anim_cancel), 0),
        ("finish", NativeFunction::from_fn_ptr(anim_finish), 0),
        ("reverse", NativeFunction::from_fn_ptr(anim_reverse), 0),
        ("updatePlaybackRate", NativeFunction::from_fn_ptr(anim_set_playback_rate), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    let handle = b.build();
    // Live accessors.
    let getters: &[(&str, fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>)] = &[
        ("playState", anim_get_play_state),
        ("currentTime", anim_get_current_time),
        ("playbackRate", anim_get_playback_rate),
    ];
    for (name, f) in getters {
        let g = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(*f),
        )
        .build();
        let _ = handle.define_property_or_throw(
            js_string!(name.to_string()),
            boa_engine::property::PropertyDescriptor::builder()
                .get(g)
                .enumerable(true)
                .configurable(true),
            ctx,
        );
    }
    // `.finished` returns a Promise that resolves when the animation
    // hits its end. We build it eagerly and stash the resolvers.
    let (finished, resolvers) = JsPromise::new_pending(ctx);
    ANIMATIONS.with(|r| {
        if let Some(entry) = r.borrow_mut().get_mut(&anim_id) {
            entry.finished_resolve = Some(resolvers.resolve);
            entry.finished_reject = Some(resolvers.reject);
            entry.handle = Some(handle.clone());
        }
    });
    let _ = handle.set(
        js_string!("finished"),
        JsValue::from(finished),
        false,
        ctx,
    );
    JsValue::from(handle)
}

fn anim_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(ANIM_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn with_anim<R>(this: &JsValue, ctx: &mut Context, f: impl FnOnce(&mut AnimationEntry) -> R) -> Option<R> {
    let id = anim_id_of(this, ctx)?;
    ANIMATIONS.with(|r| r.borrow_mut().get_mut(&id).map(f))
}

fn anim_play(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = performance_now_ms(ctx);
    with_anim(this, ctx, |a| {
        match a.state {
            PlayState::Paused => {
                a.start_time_ms = now - a.paused_progress_ms as f64 / a.playback_rate as f64;
                a.state = PlayState::Running;
            }
            PlayState::Idle | PlayState::Finished => {
                a.start_time_ms = now;
                a.paused_progress_ms = 0.0;
                a.state = PlayState::Running;
            }
            PlayState::Running => {}
        }
    });
    Ok(JsValue::undefined())
}

fn anim_pause(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = performance_now_ms(ctx);
    with_anim(this, ctx, |a| {
        if matches!(a.state, PlayState::Running) {
            let elapsed = ((now - a.start_time_ms) * a.playback_rate as f64) as f32;
            a.paused_progress_ms = elapsed.max(0.0);
            a.state = PlayState::Paused;
        }
    });
    Ok(JsValue::undefined())
}

fn anim_cancel(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = match anim_id_of(this, ctx) {
        Some(i) => i,
        None => return Ok(JsValue::undefined()),
    };
    // Snapshot the reject + handle before removing so we can fire the
    // cancel event after the borrow.
    let (reject, handle) = ANIMATIONS.with(|r| {
        let mut map = r.borrow_mut();
        let entry = match map.remove(&id) {
            Some(e) => e,
            None => return (None, None),
        };
        (entry.finished_reject, entry.handle)
    });
    if let Some(rej) = reject {
        let _ = rej.call(
            &JsValue::undefined(),
            &[JsValue::from(js_string!("cancelled"))],
            ctx,
        );
    }
    if let Some(h) = handle {
        fire_handler(&h, "oncancel", ctx);
    }
    Ok(JsValue::undefined())
}

fn anim_finish(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = performance_now_ms(ctx);
    let (resolve, handle) = with_anim(this, ctx, |a| {
        a.state = PlayState::Finished;
        a.start_time_ms = now - (a.duration_ms * a.iterations.min(1.0)) as f64;
        (a.finished_resolve.take(), a.handle.clone())
    })
    .unwrap_or((None, None));
    if let Some(res) = resolve {
        let _ = res.call(&JsValue::undefined(), &[], ctx);
    }
    if let Some(h) = handle {
        fire_handler(&h, "onfinish", ctx);
    }
    Ok(JsValue::undefined())
}

fn anim_reverse(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    with_anim(this, ctx, |a| {
        a.playback_rate = -a.playback_rate;
    });
    Ok(JsValue::undefined())
}

fn anim_set_playback_rate(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let rate = args
        .first()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(1.0) as f32;
    with_anim(this, ctx, |a| a.playback_rate = rate.max(0.001).min(100.0));
    Ok(JsValue::undefined())
}

fn anim_get_play_state(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let state = with_anim(this, ctx, |a| a.state).unwrap_or(PlayState::Idle);
    Ok(JsValue::from(js_string!(play_state_str(state).to_string())))
}

fn play_state_str(s: PlayState) -> &'static str {
    match s {
        PlayState::Idle => "idle",
        PlayState::Running => "running",
        PlayState::Paused => "paused",
        PlayState::Finished => "finished",
    }
}

fn anim_get_current_time(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = performance_now_ms(ctx);
    let t = with_anim(this, ctx, |a| current_time(a, now)).unwrap_or(0.0);
    Ok(JsValue::from(t as f64))
}

fn anim_get_playback_rate(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        with_anim(this, ctx, |a| a.playback_rate).unwrap_or(1.0) as f64,
    ))
}

fn fire_handler(obj: &boa_engine::JsObject, name: &str, ctx: &mut Context) {
    let Ok(v) = obj.get(js_string!(name.to_string()), ctx) else {
        return;
    };
    let Some(handler_obj) = v.as_object() else {
        return;
    };
    let Some(f) = JsFunction::from_object(handler_obj.clone()) else {
        return;
    };
    let _ = f.call(&JsValue::from(obj.clone()), &[], ctx);
}

fn current_time(a: &AnimationEntry, now_ms: f64) -> f32 {
    if matches!(a.state, PlayState::Paused) {
        return a.paused_progress_ms;
    }
    if matches!(a.state, PlayState::Idle) {
        return 0.0;
    }
    ((now_ms - a.start_time_ms) * a.playback_rate as f64) as f32
}

// ============ keyframe + options parsing ============

fn parse_keyframes(val: &JsValue, ctx: &mut Context) -> Vec<Keyframe> {
    let Some(arr_obj) = val.as_object() else {
        return Vec::new();
    };
    let len = arr_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    if len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let Ok(item) = arr_obj.get(i, ctx) else { continue };
        let Some(obj) = item.as_object() else { continue };
        let offset = obj
            .get(js_string!("offset"), ctx)
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_null())
            .and_then(|v| v.to_number(ctx).ok())
            .unwrap_or_else(|| {
                if len <= 1 {
                    1.0
                } else {
                    i as f64 / (len - 1) as f64
                }
            });
        // Collect every own property except `offset` / `easing` /
        // `composite` as a CSS property → value pair.
        let mut props: Vec<(String, String)> = Vec::new();
        let keys = obj.own_property_keys(ctx).ok().unwrap_or_default();
        for k in keys {
            let name = match k {
                boa_engine::property::PropertyKey::String(s) => s.to_std_string_escaped(),
                _ => continue,
            };
            if matches!(name.as_str(), "offset" | "easing" | "composite") {
                continue;
            }
            let Ok(v) = obj.get(js_string!(name.clone()), ctx) else {
                continue;
            };
            let val_str = v
                .to_string(ctx)
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // Convert camelCase to kebab-case for CSS.
            props.push((camel_to_kebab(&name), val_str));
        }
        out.push(Keyframe {
            offset: offset as f32,
            props,
        });
    }
    out.sort_by(|a, b| a.offset.partial_cmp(&b.offset).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn camel_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_options(
    val: &JsValue,
    ctx: &mut Context,
) -> (f32, f32, f32, Easing, Direction, FillMode) {
    if val.is_number() {
        // Shorthand: `el.animate(keyframes, 500)` — 500ms duration.
        let d = val.to_number(ctx).unwrap_or(0.0) as f32;
        return (d, 1.0, 0.0, Easing::Linear, Direction::Normal, FillMode::None);
    }
    let Some(obj) = val.as_object() else {
        return (0.0, 1.0, 0.0, Easing::Linear, Direction::Normal, FillMode::None);
    };
    let duration = obj
        .get(js_string!("duration"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0) as f32;
    let iterations = obj
        .get(js_string!("iterations"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| if n.is_infinite() { f32::INFINITY } else { n as f32 })
        .unwrap_or(1.0);
    let delay = obj
        .get(js_string!("delay"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0) as f32;
    let easing = obj
        .get(js_string!("easing"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| parse_easing(&s.to_std_string_escaped()))
        .unwrap_or(Easing::Linear);
    let direction = obj
        .get(js_string!("direction"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| parse_direction(&s.to_std_string_escaped()))
        .unwrap_or(Direction::Normal);
    let fill = obj
        .get(js_string!("fill"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| parse_fill(&s.to_std_string_escaped()))
        .unwrap_or(FillMode::None);
    (duration, iterations, delay, easing, direction, fill)
}

fn parse_easing(s: &str) -> Easing {
    match s.trim() {
        "linear" => Easing::Linear,
        "ease" => Easing::Ease,
        "ease-in" => Easing::EaseIn,
        "ease-out" => Easing::EaseOut,
        "ease-in-out" => Easing::EaseInOut,
        "step-start" => Easing::StepStart,
        "step-end" => Easing::StepEnd,
        _ => Easing::Linear,
    }
}

fn parse_direction(s: &str) -> Direction {
    match s {
        "reverse" => Direction::Reverse,
        "alternate" => Direction::Alternate,
        "alternate-reverse" => Direction::AlternateReverse,
        _ => Direction::Normal,
    }
}

fn parse_fill(s: &str) -> FillMode {
    match s {
        "forwards" => FillMode::Forwards,
        "backwards" => FillMode::Backwards,
        "both" => FillMode::Both,
        "auto" => FillMode::Auto,
        _ => FillMode::None,
    }
}

fn ease(t: f32, e: Easing) -> f32 {
    match e {
        Easing::Linear => t,
        Easing::Ease => cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
        Easing::EaseIn => cubic_bezier(0.42, 0.0, 1.0, 1.0, t),
        Easing::EaseOut => cubic_bezier(0.0, 0.0, 0.58, 1.0, t),
        Easing::EaseInOut => cubic_bezier(0.42, 0.0, 0.58, 1.0, t),
        Easing::StepStart => 1.0,
        Easing::StepEnd => {
            if t >= 1.0 {
                1.0
            } else {
                0.0
            }
        }
    }
}

fn cubic_bezier(p1x: f32, p1y: f32, p2x: f32, p2y: f32, t: f32) -> f32 {
    // Solve x(u) = t via Newton's method to get u, then evaluate y(u).
    let cx = 3.0 * p1x;
    let bx = 3.0 * (p2x - p1x) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * p1y;
    let by = 3.0 * (p2y - p1y) - cy;
    let ay = 1.0 - cy - by;
    let mut u = t;
    for _ in 0..8 {
        let x = ((ax * u + bx) * u + cx) * u - t;
        let dx = (3.0 * ax * u + 2.0 * bx) * u + cx;
        if dx.abs() < 1e-6 {
            break;
        }
        u -= x / dx;
    }
    ((ay * u + by) * u + cy) * u
}

// ============ tick + write-back ============

/// Advance every live animation. Called once per engine tick (from
/// rAF / animation loop). Writes effective values into each target
/// element's `style` attribute so the next cascade picks them up.
pub fn advance_animations(now_ms: f64) {
    // Snapshot ids first so we can mutate the map while iterating.
    let ids: Vec<u32> = ANIMATIONS.with(|r| r.borrow().keys().copied().collect());
    for id in ids {
        let action = ANIMATIONS.with(|r| -> Option<AnimAction> {
            let mut map = r.borrow_mut();
            let entry = map.get_mut(&id)?;
            sample(entry, now_ms)
        });
        let Some(action) = action else { continue };
        apply_resolved_style(action.node, &action.resolved);
        if action.finished {
            let (resolve, handle) = ANIMATIONS.with(|r| {
                let mut map = r.borrow_mut();
                let entry = map.get_mut(&id);
                entry
                    .map(|e| {
                        e.state = PlayState::Finished;
                        (e.finished_resolve.take(), e.handle.clone())
                    })
                    .unwrap_or((None, None))
            });
            // Resolving the Promise + onfinish need a Context; we
            // park them in a queue picked up at the next tick.
            queue_finished(id, resolve, handle);
        }
    }
}

struct AnimAction {
    node: NodeId,
    resolved: Vec<(String, String)>,
    finished: bool,
}

fn sample(a: &mut AnimationEntry, now_ms: f64) -> Option<AnimAction> {
    if !matches!(a.state, PlayState::Running | PlayState::Paused) {
        return None;
    }
    let elapsed = match a.state {
        PlayState::Paused => a.paused_progress_ms,
        _ => ((now_ms - a.start_time_ms) * a.playback_rate as f64) as f32,
    };
    let elapsed_post_delay = elapsed - a.delay_ms;
    if elapsed_post_delay < 0.0 {
        // Before delay — fill: backwards / both render the first
        // keyframe; everything else holds nothing.
        if matches!(a.fill, FillMode::Backwards | FillMode::Both) {
            let resolved = resolve(a, 0.0);
            return Some(AnimAction {
                node: a.node,
                resolved,
                finished: false,
            });
        }
        return None;
    }
    let dur = a.duration_ms.max(0.001);
    let total = dur * if a.iterations.is_infinite() { 1.0e9 } else { a.iterations };
    let mut finished = false;
    let raw_progress = if matches!(a.state, PlayState::Paused) {
        elapsed_post_delay
    } else {
        elapsed_post_delay
    };
    if raw_progress >= total {
        finished = true;
    }
    let iteration_idx = (raw_progress / dur).floor();
    let iteration_t = (raw_progress / dur) - iteration_idx;
    let iteration_t = iteration_t.clamp(0.0, 1.0);
    // Apply direction: maybe flip per iteration.
    let direction_forward = match a.direction {
        Direction::Normal => true,
        Direction::Reverse => false,
        Direction::Alternate => iteration_idx as i32 % 2 == 0,
        Direction::AlternateReverse => iteration_idx as i32 % 2 == 1,
    };
    let mut t = if direction_forward {
        iteration_t
    } else {
        1.0 - iteration_t
    };
    if finished {
        t = if direction_forward { 1.0 } else { 0.0 };
    }
    let resolved = resolve(a, t);
    Some(AnimAction {
        node: a.node,
        resolved,
        finished,
    })
}

fn resolve(a: &AnimationEntry, t: f32) -> Vec<(String, String)> {
    if a.keyframes.is_empty() {
        return Vec::new();
    }
    let eased = ease(t, a.easing);
    // For each property mentioned by any keyframe, find the pair
    // bracketing `eased` and interpolate (or snap).
    let mut props: HashMap<String, Vec<(f32, String)>> = HashMap::new();
    for kf in &a.keyframes {
        for (k, v) in &kf.props {
            props.entry(k.clone()).or_default().push((kf.offset, v.clone()));
        }
    }
    let mut out = Vec::new();
    for (k, mut samples) in props {
        samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let value = sample_property(&samples, eased);
        out.push((k, value));
    }
    out
}

fn sample_property(samples: &[(f32, String)], t: f32) -> String {
    if samples.is_empty() {
        return String::new();
    }
    if t <= samples[0].0 {
        return samples[0].1.clone();
    }
    if t >= samples[samples.len() - 1].0 {
        return samples[samples.len() - 1].1.clone();
    }
    for w in samples.windows(2) {
        let (t0, v0) = (&w[0].0, &w[0].1);
        let (t1, v1) = (&w[1].0, &w[1].1);
        if t >= *t0 && t <= *t1 {
            let local = if (t1 - t0).abs() < 1e-6 {
                0.0
            } else {
                (t - t0) / (t1 - t0)
            };
            return interpolate_strings(v0, v1, local);
        }
    }
    samples.last().unwrap().1.clone()
}

fn interpolate_strings(a: &str, b: &str, t: f32) -> String {
    // Number with unit: e.g. "10px" / "100%" / "1.5em" / "0.5".
    if let (Some((na, ua)), Some((nb, ub))) = (split_number_unit(a), split_number_unit(b)) {
        if ua == ub {
            let v = na + (nb - na) * t;
            return if ua.is_empty() {
                format!("{v}")
            } else {
                format!("{v}{ua}")
            };
        }
    }
    // Color via rgb()/rgba()/hex.
    if let (Some(ca), Some(cb)) = (parse_color(a), parse_color(b)) {
        let (r, g, bl, al) = (
            lerp(ca.0 as f32, cb.0 as f32, t).round().clamp(0.0, 255.0) as u8,
            lerp(ca.1 as f32, cb.1 as f32, t).round().clamp(0.0, 255.0) as u8,
            lerp(ca.2 as f32, cb.2 as f32, t).round().clamp(0.0, 255.0) as u8,
            lerp(ca.3, cb.3, t).clamp(0.0, 1.0),
        );
        return format!("rgba({r}, {g}, {bl}, {al})");
    }
    // Fallback: snap to the destination once past the midpoint.
    if t < 0.5 {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn split_number_unit(s: &str) -> Option<(f32, &str)> {
    let s = s.trim();
    let mut idx = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
            idx = i + c.len_utf8();
        } else {
            break;
        }
    }
    if idx == 0 {
        return None;
    }
    let num: f32 = s[..idx].parse().ok()?;
    Some((num, s[idx..].trim()))
}

fn parse_color(s: &str) -> Option<(u8, u8, u8, f32)> {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix('#') {
        let hex: Vec<char> = stripped.chars().collect();
        let parse2 = |i: usize| -> Option<u8> {
            let s: String = hex.get(i..i + 2)?.iter().collect();
            u8::from_str_radix(&s, 16).ok()
        };
        if hex.len() == 6 {
            return Some((parse2(0)?, parse2(2)?, parse2(4)?, 1.0));
        }
        if hex.len() == 8 {
            return Some((parse2(0)?, parse2(2)?, parse2(4)?, parse2(6)? as f32 / 255.0));
        }
        if hex.len() == 3 {
            let dup = |c: char| -> Option<u8> {
                u8::from_str_radix(&format!("{c}{c}"), 16).ok()
            };
            return Some((dup(hex[0])?, dup(hex[1])?, dup(hex[2])?, 1.0));
        }
    }
    if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() >= 3 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            let a = parts.get(3).and_then(|p| p.parse::<f32>().ok()).unwrap_or(1.0);
            return Some((r, g, b, a));
        }
    }
    None
}

fn apply_resolved_style(node: NodeId, resolved: &[(String, String)]) {
    if resolved.is_empty() {
        return;
    }
    crate::js::with_dom_mut(|dom| {
        // Pull the current style attribute, splice in / override the
        // properties we computed, and write back. Real spec keeps an
        // off-DOM "current effect" — but the inline style works for
        // the toy and survives a re-cascade.
        let mut existing: Vec<(String, String)> = current_inline_style(dom, node);
        for (k, v) in resolved {
            existing.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
            existing.push((k.clone(), v.clone()));
        }
        let joined = existing
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("; ");
        dom.set_attribute(node, "style", joined);
    });
}

fn current_inline_style(dom: &crate::dom::Dom, node: NodeId) -> Vec<(String, String)> {
    use crate::dom::NodeKind;
    let NodeKind::Element { attrs, .. } = &dom.node(node).kind else {
        return Vec::new();
    };
    let style = attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("style"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    style
        .split(';')
        .filter_map(|decl| {
            let decl = decl.trim();
            if decl.is_empty() {
                return None;
            }
            let mut parts = decl.splitn(2, ':');
            let k = parts.next()?.trim().to_string();
            let v = parts.next()?.trim().to_string();
            Some((k, v))
        })
        .collect()
}

// ============ deferred finished/onfinish queue ============

thread_local! {
    pub(crate) static FINISHED_QUEUE: RefCell<Vec<DeferredFinish>> = const { RefCell::new(Vec::new()) };
}

pub struct DeferredFinish {
    pub resolve: Option<JsFunction>,
    pub handle: Option<boa_engine::JsObject>,
}

fn queue_finished(_id: u32, resolve: Option<JsFunction>, handle: Option<boa_engine::JsObject>) {
    if resolve.is_none() && handle.is_none() {
        return;
    }
    FINISHED_QUEUE.with(|q| q.borrow_mut().push(DeferredFinish { resolve, handle }));
}

/// Drain pending `.finished` / `.onfinish` callbacks. Called by the
/// engine alongside microtask draining so the Promise resolution
/// happens with a live JS context.
pub fn drain_finished(ctx: &mut Context) {
    let pending: Vec<DeferredFinish> = FINISHED_QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for d in pending {
        if let Some(res) = d.resolve {
            let _ = res.call(&JsValue::undefined(), &[], ctx);
        }
        if let Some(h) = d.handle {
            fire_handler(&h, "onfinish", ctx);
        }
    }
}

/// `document.getAnimations()` — return Animation objects for every
/// live entry.
pub fn document_get_animations(ctx: &mut Context) -> JsValue {
    let ids: Vec<u32> = ANIMATIONS.with(|r| r.borrow().keys().copied().collect());
    let arr = JsArray::new(ctx);
    for id in ids {
        let _ = arr.push(build_animation_object(ctx, id), ctx);
    }
    arr.into()
}
