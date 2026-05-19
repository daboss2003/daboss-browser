# Daboss roadmap — tier 2/3 grind

Tier-1 items (JIT, multi-process, accessibility tree, EME) are out
of scope — explicitly deferred forever. Everything else gets ground
through one item per session.

Each item links to its commit when shipped. Notes capture the
approach and any leftover sharp edges so a future session can pick
up the work without re-deriving context.

## Status legend

- [ ] pending — not started
- [~] in flight — partially done, see notes
- [x] shipped — links to commit

## In flight

(nothing in flight — next item picks up from Pending)

## Just shipped

- [x] `24e8829` **CSS subgrid (columns)** — `ComputedStyle` gains
      `subgrid_columns` / `subgrid_rows` flags. Cascade parses
      `grid-template-{columns,rows}: subgrid` into the flag. Layout
      uses a thread-local `SUBGRID_PARENT` that the parent grid
      populates with the column-width slice + col-gap for each
      item before recursing. A subgrid child consumes the slice as
      its own `column_widths`. Two follow-up fixes fell out: the
      shorthand `grid-column: span N` (no `/`) now parses as a
      single `Span` value, and auto-placement reads `Span` from
      either `column_start` or `column_end`. Row subgrid sets the
      flag but rows can't inherit cheaply (parent row heights
      aren't known until after children lay out); behaves as auto.
- [x] **Real bidi text shaping** (this session) — cosmic-text
      already runs the Unicode bidi algorithm per line during
      shape, so mixed Arabic/Hebrew/Latin runs were already
      visually reordered. What was missing:
      * `TextAlign::Start` / `TextAlign::End` variants +
        `.resolved(direction)` mapper. Parsed in cascade.
      * `dir="rtl"` HTML attribute mapped to
        `Direction::Rtl` during cascade, auto-flipping default
        `text-align: Left` to `Right`.
      * Author `text-align` is now propagated onto every
        `BufferLine` via `set_align(Some(Align::*))` so
        per-line alignment honours CSS instead of
        cosmic-text's default LTR=Left / RTL=Right behaviour.
      Tests cover `dir="rtl"` flipping direction +
      `text-align: start` resolving via direction.
- [x] **Real WebGL 2 surface** (this session) —
  `getContext("webgl2")` now routes to a versioned constructor
  that, on top of the existing WebGL 1 entry points, adds:
  VAOs, sampler / query / sync / transform-feedback handle
  constructors, instanced draws + vertex-attrib divisor +
  integer attrib pointer, 3D / array textures + immutable
  storage, MRT (drawBuffers / clearBufferxx), uniform buffer
  block surface, the `uniform*ui` + non-square matrix uniform
  setters, blit/invalidate/readPixels/renderbuffer-multisample,
  and ~90 new GLenum constants pages probe (UNIFORM_BUFFER,
  RGBA8, COLOR_ATTACHMENT[0-7], SYNC_GPU_COMMANDS_COMPLETE,
  etc.). Mostly state-tracking stubs that accept the call shape
  so feature-detection + initial setup don't trip; real
  driver-level wiring to wgpu equivalents is incremental
  follow-up.

## Pending (each is its own session)

- [ ] **CSS anchor positioning** — `anchor-name` / `position-anchor`
  / `anchor()`. Layout-time bipartite tracking.
- [ ] **CSS masking** — `mask-image` / `mask-mode`. Paint-side
  per-pixel multiply against the mask alpha.
- [ ] **Source maps** — fetch `//# sourceMappingURL=` for each
  script/stylesheet, parse VLQ mappings, hook into devtools so
  the Sources panel shows the original file.
- [ ] **Per-tile damage tracking** — split each layer's pixmap
  into 256×256 tiles, hash per tile. Only re-render the dirty
  tiles. Needs subtree → tile invalidation mapping.
- [ ] **Compositor thread / GPU rasterization** — run paint on a
  dedicated thread; raster glyphs / paths on the GPU via wgpu
  compute. This is the biggest architectural lift left.
- [ ] **DevTools Sources panel with breakpoints** — needs boa
  instrumentation hooks so we can pause on breakpoint lines +
  step. Probably start with read-only source view + console-eval
  on selected line.
- [ ] **Storage partitioning by top-level origin** — every
  per-origin store (cookies, localStorage, IDB, OPFS, cache,
  SW registrations, push subs) currently keys on the inner
  origin only. Re-key as `(top-level-origin, inner-origin)`.
- [ ] **First-Party Sets / CHIPS** — parses but doesn't enforce.
  Tied to storage partitioning.
- [ ] **CSS Houdini paint/layout/animation worklets actually
  executing** — needs a separate boa Context per worklet,
  custom-paint canvas API, glue to call `paint()` during
  rendering.
- [ ] **WebExtensions runtime (real)** — implement enough of
  `chrome.*` so MV3 extensions can load. Massive.

## Completed

- [x] Shadow DOM style scoping (this session) — Stylesheet gains
      `scope` + `is_ua`. `collect_scoped` walks into
      `__shadow_root__` subtrees and tags their `<style>` rules
      with the host shadow's NodeId. `compute_one` and
      `compute_pseudo_style` gate matching by
      `sheet_scope_allows(sheet, dom, node, node_shadow_root)`.
      UA rules ignore scope; page rules don't cross into shadow
      trees; shadow rules don't escape to the light tree.
      `flatten_for_viewport` preserves both fields. Tests cover
      both directions of leakage.
- [x] `0de7460` Custom Elements lifecycle + aspect-ratio in layout
- [x] `1089295` PSL subset + CSS :has() + color-mix()
- [x] `577b474` Intl locale data for 10 locales
- [x] `54476ba` Layer cache LRU eviction
- [x] `daea3a9` DevTools Storage panel
- [x] `35f8122` Web API stubs (Payment / Locks / Pressure / Idle /
      StorageBuckets / DocPiP / WebXR)
- [x] `d2df405` HSTS preload list
- [x] `7fe0e15` CSS Houdini + WebExtensions surface
