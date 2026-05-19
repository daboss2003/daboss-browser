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

- [x] **First-Party Sets + CHIPS** (this session) — new
      `net::first_party_set` module with a curated `(member,
      primary)` table covering Google, Microsoft, GitHub, and
      Wikimedia properties plus a synthetic toy set used in
      tests. Exports `primary_for(host)` and `same_party(a, b)`
      (both case-insensitive + `www.`-stripping).
      `opfs::partitioned_origin_host` now collapses partition
      keys onto the FPS primary when top and inner belong to the
      same set, so e.g. github.io storage joins github.com under
      any github.* embedder. Cookies gain a
      `partition_key: Option<String>` field; the parser
      recognises the `Partitioned` attribute and stamps the
      top-level host (collapsed through FPS) at receive time, but
      only when SameSite=None + Secure are also present per
      spec. A new `header_for_with_top(...)` variant filters
      Partitioned cookies by the request's top-level — legacy
      unpartitioned cookies still flow as before. Disk jar
      version bumped to 3 to round-trip the new field. Tests
      cover FPS member collapse for primary lookup,
      `www.`/case-insensitivity, partition-collapse storage join,
      Partitioned attribute requiring SameSite=None+Secure,
      cross-top filtering, FPS collapse during cookie parse, and
      a persistence round-trip.
- [x] `5bddcd7` **Storage partitioning by top-level origin** — disk-backed per-origin stores now key off
      `(top-level-host, inner-host)` pairs instead of the bare
      inner host. New `JS_TOP_LEVEL_BASE_URL` thread-local in
      `js::engine` (mirrors `JS_BASE_URL` for top-frame contexts;
      a future iframe-aware shell would override it with the
      embedder URL). New `opfs::partitioned_origin_host()` returns
      a sanitised path component: just the inner host when
      top == inner (so already-stored first-party data keeps
      working without migration), or `<top>__<inner>` when the
      two differ. Migrated callers: OPFS root, IndexedDB root,
      localStorage dir, Service Worker caches root +
      `ensure_caches_loaded` guard, SW registrations path +
      `replay_persisted_registrations` guard. Push subscriptions
      already reject (no real backing) so they're partitioned
      vacuously. Cookies and WebAuthn intentionally NOT moved —
      cookies need a partition_key on each Cookie entry (deferred
      to the upcoming First-Party Sets / CHIPS slice); WebAuthn
      keys off the relying-party ID per spec, which is not a
      browser-context concept. Tests cover key collapse for
      first-party, separator behaviour for cross-context, and
      cross-top isolation of a shared third-party origin.
- [x] `8e4d30a` **DevTools Sources panel with breakpoints** —
      the Sources panel is now interactive. `SourcesPanelState`
      tracks the selected source-map, selected file within the
      map, cursor line, and scroll position; the breakpoint set
      itself lives in `source_map::BREAKPOINTS` so the JS engine
      can query without depending on devtools. Hotkeys inside the
      Sources panel: `n` (next map), `s` (next source file), `↑/↓`
      (move cursor), `PgUp/PgDn` (jump 10 lines), `b` (toggle
      breakpoint at cursor). The panel renders the selected source
      with line numbers, a `>` cursor marker, and `●` markers in
      red for breakpointed lines; viewport auto-scrolls to keep
      the cursor visible. Execution hits: `run_initial_scripts`
      now installs a global `__bp_hit` callable and rewrites each
      script before evaluation — every line listed in
      `breakpoints_for(<inline #N>, 0)` gets `;__bp_hit("<key>",
      line);` prepended; a hit pushes an Info-level console
      message. Inline scripts where `sources_content[0] == script
      body` work cleanly (the common case); transpiled bundles
      where lines come from non-trivial source-map mappings would
      need the source-map's mappings consulted, which is the next
      slice. Tests cover registry refresh on panel switch, cursor
      clamp + toggle round-trip, the rewrite line-by-line, and an
      end-to-end hit producing a console line.
- [x] `7380302` **Compositor thread + GPU rasterisation** (first cut) — new `gpu_raster` module. `GpuRasterizer` owns a
      headless wgpu Device + Queue + a render pipeline that
      consumes a list of `GpuRect { x, y, w, h, color: [f32; 4] }`
      and produces a `tiny_skia::Pixmap`. Two triangles per rect,
      premultiplied alpha blending, render target is a Rgba8Unorm
      texture, copy-texture-to-buffer + sync map for readback.
      Hand-packed little-endian vertex serialisation keeps the
      crate inside `#![forbid(unsafe_code)]`. `CompositorThread`
      wraps the rasteriser on a named OS thread; callers send
      `RasterRequest` over an mpsc channel and block on a reply
      channel — the GPU work happens entirely off the UI thread.
      Tests cover a single red rect, two colour-banded rects, and
      a threaded request via `CompositorThread::spawn`. Not yet
      wired into the production paint path — that's the next
      slice (route per-tile damage rasterisation through the
      worker so dirty tiles render off-thread). Glyph
      rasterisation also still goes through cosmic-text + swash
      CPU; GPU glyphing would need an atlas pipeline that's its
      own session.
- [x] `dbc9ef6` **Per-tile damage tracking** — every
      will-change layer pixmap is conceptually diced into 256×256
      tiles. `CachedLayer` gains `tile_input_hashes` (one per
      tile), `tile_cols`, and `tile_rows`. A new
      `compute_per_tile_input_hashes` walks the subtree and folds
      each node ONLY into the tiles its box rect overlaps, so a
      narrow text mutation only touches the hash of the tile it
      sits in. Paint flow grows a third path between the existing
      fast-path (whole-subtree hash matches) and slow-path (full
      re-render): on subtree-hash mismatch we recompute per-tile
      hashes; if every tile matches its cached value we reuse the
      cached pixmap (a subtree-hash false-positive — e.g. a
      hidden attr flip with no on-screen change). If only SOME
      tiles dirtied, we paint each dirty tile in isolation —
      redirecting the painter into a tile-sized offscreen with a
      ctx that puts the layer's content at the tile's origin —
      and copy the freshly painted bytes back into a canvas that
      starts as a clone of the cached pixmap. Clean tiles keep
      their cached bytes, no re-rasterisation. Tests verify (a) a
      wide layer reports >=2 tile columns, (b) mutating a node on
      the right side leaves the leftmost tile's input hash
      unchanged, and (c) idempotent paints bump `last_used`
      without inserting duplicates.
- [x] `674b409` **Source maps + DevTools Sources panel** —
      new `source_map` module: scrape
      `//# sourceMappingURL=` / legacy `//@` from a script tail,
      parse the JSON v3 format (custom recursive-descent parser to
      avoid pulling serde for this one consumer), and base64-VLQ
      decode the `mappings` blob into a per-line table of
      `Segment { gen_col, source_index, source_line, source_col,
      name_index }`. Parsed maps live in a thread-local
      `SOURCE_MAPS` registry. The JS engine's
      `run_initial_scripts` calls into the scraper; if the URL is
      a `data:application/json;base64,...` blob it decodes inline
      and registers under `<inline #N>`. New devtools
      `Panel::Sources` between Storage and Picker lists all
      registered maps with their source file count, mapping row
      count, and a preview of the first `sourcesContent` blob
      (capped at 40 lines). External (http) source-map URLs would
      need a fetch hook the shell hasn't wired yet; data: URLs
      are the dominant production-bundle form so this covers the
      90% case.
- [x] `1b5be15` **CSS masking** — `ComputedStyle` gains
      `mask_image: Option<BackgroundImage>` (reuses the existing
      `Url` / `LinearGradient` enum) + `mask_mode: MaskMode`
      (Alpha / Luminance / MatchSource). Cascade parses
      `mask-image` (also `-webkit-mask-image`) and `mask-mode`.
      `ImageCache` gains `ImageSlot::Mask`; the bg-image walker
      now also fetches `mask-image: url(...)` sources. Paint adds
      a new `paint_subtree_with_mask` pass slotted in before
      filter: render the subtree into an offscreen pixmap (so
      nested filter/transform still work), render the mask source
      into a parallel pixmap at the element's box size, then walk
      both per-pixel and multiply the mask alpha (or Rec.601
      luminance for `mask-mode: luminance`) into the subtree's
      premultiplied RGBA before compositing. `match-source`
      collapses to alpha, which matches the dominant author use
      of alpha-PNG sprite masks. Tests cover both modes.
- [x] `1142c8d` **CSS anchor positioning** — `ComputedStyle`
      gains `anchor_name`, `position_anchor`, and per-side
      `anchor_top/right/bottom/left: Option<AnchorRef>`. Cascade
      parses `anchor-name: --foo`, `position-anchor: --foo`, and
      `anchor(<name>? <side>)` inside inset properties. Layout
      runs a post-pass after the main tree is built: it collects
      every node with `anchor-name` into a `HashMap<name, Rect>`,
      then for each `position: absolute|fixed` element with any
      inset anchor reference resolves the target edge and shifts
      the subtree. Two fall-out fixes: the CSS parser now
      recognises dashed-idents (`--foo`) as keyword values
      (previously the leading `-` routed to numeric parsing and
      the ident was silently consumed one char at a time), and
      `offset_from` returns `None` for `Value::Function { name:
      "anchor", .. }` so the plain inset path doesn't fight the
      anchor pass. Tests cover bottom-edge alignment,
      `position-anchor` defaulting, and `right: anchor(--a right)`
      pulling the element's left edge back by its width.
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
