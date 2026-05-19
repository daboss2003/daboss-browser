#![forbid(unsafe_code)]

mod audio;
mod devtools;
mod capture;
mod css;
mod dom;
mod gpu;
mod sse;
mod webgl_gpu;
mod html;
mod js;
mod layout;
mod net;
mod paint;
mod video;
mod webrtc;
mod ws;

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::rc::Rc;

use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const MAX_EXTERNAL_STYLESHEETS: usize = 30;
const MAX_IMAGES: usize = 50;
const MAX_IFRAMES: usize = 5;
const PAINT_HEIGHT_CEILING: u32 = 65_535;
/// Height (px) of the browser chrome strip at the top of the window —
/// holds the URL bar.
const CHROME_HEIGHT: u32 = 64;
const TAB_STRIP_HEIGHT: u32 = 28;
const URL_BAR_HEIGHT: u32 = CHROME_HEIGHT - TAB_STRIP_HEIGHT;
const TAB_WIDTH: u32 = 180;
const TAB_CLOSE_RADIUS: f32 = 7.0;
const NEW_TAB_BUTTON_WIDTH: u32 = 32;

/// Maximum number of preserved Pages in the back-forward cache.
const BFCACHE_CAP: usize = 5;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cosmic_text=error".into()),
        )
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let png_mode = args.iter().any(|a| a == "--png");
    let initial_url: Option<String> = args.iter().find(|a| !a.starts_with("--")).cloned();

    if png_mode {
        let Some(url) = initial_url else {
            eprintln!("usage: daboss --png <url>");
            return ExitCode::FAILURE;
        };
        return match run_png_export(&url) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    run_browser(initial_url);
    ExitCode::SUCCESS
}

// ---------------- Headless PNG mode (still useful for testing) ----------------

fn run_png_export(url_str: &str) -> Result<(), net::Error> {
    let allow_loopback = std::env::var("DABOSS_ALLOW_LOOPBACK").is_ok();
    let client = net::Client::new().with_allow_loopback(allow_loopback);
    let base_url = url::Url::parse(url_str).map_err(|e| net::Error::InvalidUrl(e.to_string()))?;
    let response = client.get_with(
        url_str,
        net::RequestContext::new().with_top_level_navigation(true),
    )?;
    eprintln!("HTTP/1.1 {} {}", response.status, response.reason);
    let body = String::from_utf8_lossy(&response.body);
    let mut dom = html::parse(&body);
    // We don't keep the engine for headless PNG export — no clicks land
    // here, so addEventListener-registered handlers couldn't fire anyway.
    let _engine = js::run_inline_scripts(&mut dom);

    let mut sheets: Vec<css::Stylesheet> = Vec::new();
    let mut ext_count = 0usize;
    for r in css::discover_stylesheets(&dom) {
        match r {
            css::StylesheetRef::Embedded(s) => sheets.push(s),
            css::StylesheetRef::External { href, integrity } => {
                if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                    continue;
                }
                ext_count += 1;
                if let Ok(abs) = base_url.join(&href) {
                    if let Ok(r) = client.get(&abs.to_string()) {
                        if (200..300).contains(&r.status)
                            && stylesheet_integrity_ok(&href, integrity.as_deref(), &r.body)
                        {
                            sheets.push(css::parse(&String::from_utf8_lossy(&r.body)));
                        }
                    }
                }
            }
        }
    }
    let style_tree = css::style_dom(&dom, &sheets);

    prefetch_link_resources(&dom, &client, &base_url);

    let mut images = layout::ImageCache::new();
    prefetch_images(&dom, &client, &base_url, &mut images);
    prefetch_background_images(&dom, &style_tree, &client, &base_url, &mut images);

    let viewport = layout::Rect {
        x: 0.0,
        y: 0.0,
        width: 1024.0,
        height: 768.0,
    };
    let box_tree = layout::layout(&dom, &style_tree, &images, viewport);

    let mut max_bottom = viewport.height as u32;
    for opt in &box_tree.boxes {
        if let Some(b) = opt {
            let bottom = (b.rect.y + b.rect.height).ceil() as u32;
            if bottom > max_bottom {
                max_bottom = bottom;
            }
        }
    }
    let paint_height = max_bottom.min(PAINT_HEIGHT_CEILING);

    let iframes = render_iframes(&dom, &box_tree, &client, &base_url);

    if let Some(mut pixmap) = paint::paint(
        &dom,
        &style_tree,
        &box_tree,
        &images,
        viewport.width as u32,
        paint_height,
    ) {
        composite_iframes(&mut pixmap, &box_tree, &iframes);
        if let Ok(png) = pixmap.encode_png() {
            let path = "/tmp/daboss-out.png";
            let _ = std::fs::write(path, png);
            eprintln!(
                "[png] wrote {path} ({}x{paint_height}, {} iframe(s))",
                viewport.width as u32,
                iframes.len()
            );
        }
    }
    Ok(())
}

// ---------------- Browser shell ----------------

fn run_browser(initial_url: Option<String>) {
    let event_loop = EventLoop::new().expect("event loop");
    let mut browser = Browser::new(initial_url);
    event_loop.run_app(&mut browser).expect("event loop");
}

/// One in-flight CSS transition. Only `opacity` is honoured today —
/// adding more interpolated properties is each its own follow-up.
struct RunningAnim {
    node: dom::NodeId,
    property: String,
    from: f32,
    to: f32,
    start: std::time::Instant,
    duration: std::time::Duration,
    timing: css::TimingFunction,
}

struct Page {
    url: url::Url,
    dom: dom::Dom,
    styles: css::StyleTree,
    box_tree: layout::BoxTree,
    images: layout::ImageCache,
    /// All stylesheets that contributed to the cascade — UA stylesheet is
    /// rebuilt by `css::style_dom`, so we just keep the page-supplied set.
    sheets: Vec<css::Stylesheet>,
    /// Full-page rendered pixmap.
    pixmap: Pixmap,
    /// Currently hovered node (if any).
    hover: Option<dom::NodeId>,
    /// Currently focused node (if any).
    focus: Option<dom::NodeId>,
    /// Per-`<input>` typed value. Keyed by the input's NodeId.
    inputs: std::collections::HashMap<dom::NodeId, String>,
    /// In-progress IME composition strings, per focused input. The
    /// paint pipeline reads this to show the pre-edit text alongside
    /// the committed input value; the OS hasn't accepted the
    /// keystrokes yet (CJK candidate window etc.).
    input_preedit: std::collections::HashMap<dom::NodeId, String>,
    /// Compositor cache: per-layer painted pixmaps keyed by NodeId.
    /// Survives across paint passes so animations on transform /
    /// opacity skip the subtree repaint cost when the subtree
    /// content hash is unchanged.
    layer_cache: std::rc::Rc<std::cell::RefCell<paint::LayerCache>>,
    /// `position: fixed` overlays — painted out of the document
    /// pixmap so they stay pinned during scroll. Refilled each
    /// paint pass; consumed by the redraw blit.
    fixed_overlays: std::rc::Rc<std::cell::RefCell<Vec<paint::FixedOverlay>>>,
    /// Rendered iframe contents, keyed by the iframe's NodeId in this page.
    iframes: std::collections::HashMap<dom::NodeId, IframeContent>,
    /// Audio elements keyed by their `<audio>` element id, prefetched
    /// during navigation. Held in a shared `Rc<RefCell>` so JS shims
    /// can grab the same map via a thread-local.
    audio: js::AudioElements,
    /// `<video>` element registry; same shape as `audio` but each
    /// entry owns an ffmpeg subprocess + decode thread.
    video: js::VideoElements,
    /// Page-scoped JS context. Owns the long-lived `boa::Context` plus the
    /// addEventListener registry, so click handlers registered by inline
    /// scripts can fire on subsequent user input.
    js: js::JsEngine,
    /// Currently animating properties. Browser advances these on each
    /// rAF tick and writes interpolated values back into `styles`.
    anims: Vec<RunningAnim>,
    /// Last cascaded opacity per element. After each cascade we compare
    /// to detect properties that should start transitioning.
    prev_opacity: std::collections::HashMap<dom::NodeId, f32>,
}

/// A nested document loaded inside an `<iframe>`. We render it like a real
/// page (own DOM, styles, layout, paint) into a small pixmap sized to the
/// iframe's content box, then composite into the parent. The DOM and box
/// tree stay on this struct so clicks landing inside the iframe can be
/// hit-tested against its own layout (Phase 6e click-to-navigate).
struct IframeContent {
    url: url::Url,
    dom: dom::Dom,
    box_tree: layout::BoxTree,
    pixmap: Pixmap,
    /// True if the iframe element carried a `sandbox` attribute. Toy
    /// behaviour: block click-to-navigate when set.
    sandbox: bool,
}

struct Chrome {
    /// Text currently in the URL bar (editable when focused).
    text: String,
    focused: bool,
}

/// Find-in-page state: query string + cached matches + cursor.
struct FindState {
    query: String,
    /// IDs of element nodes whose text content contains the query.
    matches: Vec<dom::NodeId>,
    /// Index into `matches` of the currently-highlighted hit.
    current: usize,
}

impl FindState {
    fn new() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
        }
    }
}

/// Snapshot of a tab while it isn't focused. When the user switches
/// to this tab, the Browser swaps its current active state into the
/// vacated `InactiveTab` slot and pulls these fields back onto the
/// live `Browser` ones.
struct InactiveTab {
    page: Option<Page>,
    history: Vec<url::Url>,
    history_cursor: Option<usize>,
    scroll_y: f32,
    url_bar: String,
    /// `Some(url)` when this tab hasn't navigated yet (new tab; load
    /// pending) — drives the first paint after the tab is focused.
    pending_url: Option<String>,
}

impl Default for InactiveTab {
    fn default() -> Self {
        Self {
            page: None,
            history: Vec::new(),
            history_cursor: None,
            scroll_y: 0.0,
            url_bar: String::new(),
            pending_url: None,
        }
    }
}

struct Browser {
    /// URL to load on first frame.
    pending_url: Option<String>,

    client: Rc<net::Client>,

    /// Per-origin `localStorage` map. Each navigated [`js::JsEngine`]
    /// gets the `StorageArea` keyed by its page's origin (scheme + host
    /// + port). No on-disk persistence yet.
    local_storage: Rc<std::cell::RefCell<std::collections::HashMap<String, js::StorageArea>>>,

    window: Option<std::sync::Arc<Window>>,
    /// GPU surface + presenter. None until winit's `resumed` event
    /// gives us a window we can build a wgpu surface against.
    surface: Option<gpu::GpuPresenter>,
    /// CPU-side framebuffer the rest of the rendering pipeline writes
    /// into. We swap this for whatever the wgpu presenter wants on
    /// each frame.
    framebuf: Vec<u32>,
    /// Persistent BGRA byte buffer fed to the GPU each frame. Lives
    /// on the Browser so we don't reallocate per redraw.
    byte_buf: Vec<u8>,
    viewport_size: (u32, u32),
    scroll_y: f32,
    cursor: (f32, f32),

    /// Tracks Shift/Control/Alt/Super for chord shortcuts (Cmd+R, Cmd+L, etc.).
    modifiers: ModifiersState,

    chrome: Chrome,

    /// Long-lived font system used for chrome text only (so we don't
    /// re-scan system fonts every frame).
    chrome_font_system: FontSystem,
    chrome_swash: SwashCache,

    page: Option<Page>,

    /// Visited URLs *before* the current page. The current page is not in
    /// `history`; pushing a new URL moves the old current_url into history.
    history: Vec<url::Url>,
    /// When the user uses Back/Forward, `history_cursor` points into
    /// `history` indicating where in the back-stack we currently are.
    /// `None` means we're at the live tip (no back-navigation active).
    history_cursor: Option<usize>,

    /// Inactive tabs. The active tab's state lives directly on the
    /// fields above (`page` / `history` / `history_cursor` /
    /// `scroll_y` / `chrome.text`); switching tabs swaps state into /
    /// out of this list.
    tabs: Vec<InactiveTab>,
    /// Index of the active tab. The active tab itself has no entry in
    /// `tabs` — its state IS the live Browser state.
    active_tab: usize,

    /// Find-in-page state. `Some` while the find bar is visible /
    /// active.
    find: Option<FindState>,

    /// Back-forward cache. Each entry preserves a fully-rendered Page
    /// keyed by URL so Back / Forward can restore without refetching
    /// (no network round-trip, no JS re-execution, scroll position
    /// preserved). Capped at `BFCACHE_CAP` entries — evicted oldest
    /// first.
    bfcache: Vec<(url::Url, Page, f32)>,

    /// Devtools overlay state. Toggled with F12 / Cmd+Opt+I; when
    /// `open`, keyboard input goes to the console prompt and the
    /// panel renders along the bottom of the window.
    devtools: devtools::DevTools,
}

impl Browser {
    fn new(initial_url: Option<String>) -> Self {
        let allow_loopback = std::env::var("DABOSS_ALLOW_LOOPBACK").is_ok();
        Self {
            pending_url: initial_url.clone(),
            client: Rc::new(net::Client::new().with_allow_loopback(allow_loopback)),
            local_storage: Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::<String, js::StorageArea>::new(),
            )),
            window: None,
            surface: None,
            framebuf: Vec::new(),
            byte_buf: Vec::new(),
            viewport_size: (1024, 768),
            scroll_y: 0.0,
            cursor: (0.0, 0.0),
            modifiers: ModifiersState::empty(),
            chrome: Chrome {
                text: initial_url.unwrap_or_default(),
                focused: false,
            },
            chrome_font_system: FontSystem::new(),
            chrome_swash: SwashCache::new(),
            page: None,
            history: Vec::new(),
            history_cursor: None,
            tabs: Vec::new(),
            active_tab: 0,
            find: None,
            bfcache: Vec::new(),
            devtools: devtools::DevTools::new(),
        }
    }

    /// Install the shared console + network capture buffers so the
    /// JS console shims and the network client push into the
    /// devtools' scrollback / network log respectively.
    fn install_devtools_console_capture(&self) {
        devtools::JS_CONSOLE_BUFFER.with(|slot| {
            *slot.borrow_mut() = Some(self.devtools.buffer.clone());
        });
        devtools::NETWORK_LOG.with(|slot| {
            *slot.borrow_mut() = Some(self.devtools.network.clone());
        });
    }

    /// Height (px) available to the page after subtracting the chrome.
    fn page_viewport_height(&self) -> u32 {
        self.viewport_size.1.saturating_sub(CHROME_HEIGHT).max(1)
    }

    fn navigate(&mut self, url_str: &str, record_history: bool) {
        let parsed = match url::Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[nav] invalid url {url_str:?}: {e}");
                return;
            }
        };
        eprintln!("[nav] → {parsed}");

        // BFCache: if a previously-rendered Page for this URL is
        // sitting in the cache, restore it instead of refetching.
        // `record_history` is false when we got here via Back/Forward
        // so the typical hit path is exactly that case.
        if !record_history {
            if let Some((parsed_match, page, scroll_y)) = self.bfcache_take(&parsed) {
                eprintln!("[bfcache] restored {parsed_match}");
                if let Some(prev) = self.page.take() {
                    self.bfcache_push(prev, self.scroll_y);
                }
                self.page = Some(page);
                self.scroll_y = scroll_y;
                self.chrome.text = parsed.to_string();
                self.chrome.focused = false;
                self.refresh_js_bounding_rects();
                self.refresh_js_computed_styles();
                if let Some(p) = self.page.as_mut() {
                    if let Some(root) = first_element_child(&p.dom, p.dom.document()) {
                        let _ = p.js.dispatch_event_with(
                            &mut p.dom,
                            "pageshow",
                            root,
                            js::engine::EventInit::non_bubbling(),
                        );
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return;
            }
        }

        let response = match self.client.get_with(
            url_str,
            net::RequestContext::new().with_top_level_navigation(true),
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[nav] fetch failed: {e}");
                return;
            }
        };
        if !(200..400).contains(&response.status) {
            eprintln!("[nav] HTTP {}", response.status);
        }
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let mut dom = html::parse(&body);

        // Parse Content-Security-Policy from the page response. Without
        // an unsafe-inline allowance, the JS engine skips running
        // inline `<script>` content. `require-trusted-types-for
        // 'script'` flips the Trusted Types enforcement flag so DOM
        // sinks like `innerHTML` reject raw strings.
        let csp = response
            .header("Content-Security-Policy")
            .map(net::Csp::parse)
            .unwrap_or_default();
        let inline_scripts_allowed = csp.allows_inline_scripts();
        if !inline_scripts_allowed {
            eprintln!("[csp] inline scripts blocked by Content-Security-Policy");
        }
        js::trusted_types::set_required(csp.require_trusted_types_for_script);

        // Parse Permissions-Policy and pipe it to the JS layer so
        // `document.featurePolicy.allowsFeature(...)` answers
        // truthfully. The page URL is the policy's "self" origin —
        // `(self)` allowlists match the document.
        let perm_policy = response
            .header("Permissions-Policy")
            .map(net::PermissionsPolicy::parse)
            .unwrap_or_default()
            .with_self_origin(parsed.origin());
        js::permissions::set_policy(perm_policy);

        // Phase 7a/b/c/e: spin up a JS engine for the page and run its
        // inline `<script>` content. The engine persists on `Page` so
        // event handlers registered via `addEventListener` can fire on
        // later clicks. `fetch` is wired to the same SSRF-guarded
        // client that powers page navigation; `localStorage` is scoped
        // per origin (scheme + host + port) — different origins get
        // different stores within the same browser run.
        let origin_key = parsed.origin().ascii_serialization();
        let local_storage = self
            .local_storage
            .borrow_mut()
            .entry(origin_key)
            .or_insert_with(|| {
                Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()))
            })
            .clone();
        let js_engine = js::JsEngine::with_security(
            &mut dom,
            Some(self.client.clone()),
            Some(parsed.clone()),
            Some(local_storage),
            inline_scripts_allowed,
        );

        // External stylesheets (with size cap)
        let mut sheets: Vec<css::Stylesheet> = Vec::new();
        let mut ext_count = 0;
        for r in css::discover_stylesheets(&dom) {
            match r {
                css::StylesheetRef::Embedded(s) => sheets.push(s),
                css::StylesheetRef::External { href, integrity } => {
                    if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                        continue;
                    }
                    ext_count += 1;
                    if let Ok(abs) = parsed.join(&href) {
                        if let Ok(resp) = self.client.get(&abs.to_string()) {
                            if (200..300).contains(&resp.status)
                                && stylesheet_integrity_ok(
                                    &href,
                                    integrity.as_deref(),
                                    &resp.body,
                                )
                            {
                                sheets.push(css::parse(&String::from_utf8_lossy(&resp.body)));
                            }
                        }
                    }
                }
            }
        }

        let css_viewport = css::Viewport::from_size(
            self.viewport_size.0 as f32,
            self.page_viewport_height() as f32,
        );
        let style_tree = css::style_dom_with_viewport(
            &dom,
            &sheets,
            &css::InteractionState::EMPTY,
            &css_viewport,
        );

        prefetch_link_resources(&dom, &self.client, &parsed);

        let mut images = layout::ImageCache::new();
        prefetch_images(&dom, &self.client, &parsed, &mut images);
        prefetch_background_images(&dom, &style_tree, &self.client, &parsed, &mut images);

        let viewport = layout::Rect {
            x: 0.0,
            y: 0.0,
            width: self.viewport_size.0 as f32,
            height: self.page_viewport_height() as f32,
        };
        let box_tree = layout::layout(&dom, &style_tree, &images, viewport);

        let mut max_bottom = self.page_viewport_height();
        for opt in &box_tree.boxes {
            if let Some(b) = opt {
                let bottom = (b.rect.y + b.rect.height).ceil() as u32;
                if bottom > max_bottom {
                    max_bottom = bottom;
                }
            }
        }
        for (_, p) in &box_tree.pseudo_boxes {
            let bottom = (p.rect.y + p.rect.height).ceil() as u32;
            if bottom > max_bottom {
                max_bottom = bottom;
            }
        }
        let paint_h = max_bottom.min(PAINT_HEIGHT_CEILING);

        // Iframes are fetched + rendered after parent layout (so we know
        // each iframe's box size) but before parent paint (so we composite
        // them into the parent pixmap as the last step).
        let iframes = render_iframes(&dom, &box_tree, &self.client, &parsed);

        // Install the JS engine's canvas surfaces for the duration of
        // paint so any `<canvas>` element composites correctly.
        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            *slot.borrow_mut() = Some(js_engine.canvas_surfaces());
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            *slot.borrow_mut() = Some(js_engine.video_elements());
        });
        paint::PAINT_CAPTURES.with(|slot| {
            *slot.borrow_mut() = Some(js_engine.captures());
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            *slot.borrow_mut() = Some(js_engine.capture_bindings());
        });
        let painted = paint::paint(
            &dom,
            &style_tree,
            &box_tree,
            &images,
            self.viewport_size.0,
            paint_h,
        );
        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            slot.borrow_mut().take();
        });
        let mut pixmap = match painted {
            Some(p) => p,
            None => {
                eprintln!("[paint] could not allocate pixmap");
                return;
            }
        };
        composite_iframes(&mut pixmap, &box_tree, &iframes);

        if record_history {
            if let Some(prev) = self.page.take() {
                self.history.push(prev.url.clone());
                self.history_cursor = None;
                // Store the outgoing page into BFCache so a Back hit
                // restores it instantly. Skips evicting matching URL
                // (so reload-then-back works).
                self.bfcache_push(prev, self.scroll_y);
            }
        }

        // Sync the URL bar text so users see where they actually landed
        // (after redirects, etc.).
        self.chrome.text = parsed.to_string();
        self.chrome.focused = false;

        // Seed input values from each <input>'s `value` attribute.
        let mut inputs: std::collections::HashMap<dom::NodeId, String> =
            std::collections::HashMap::new();
        seed_input_values(&dom, dom.document(), &mut inputs);

        let audio_map = js_engine.audio_elements();
        let video_map = js_engine.video_elements();
        self.page = Some(Page {
            url: parsed,
            dom,
            styles: style_tree,
            box_tree,
            images,
            sheets,
            pixmap,
            hover: None,
            focus: None,
            inputs,
            input_preedit: std::collections::HashMap::new(),
            layer_cache: std::rc::Rc::new(std::cell::RefCell::new(
                paint::LayerCache::new(),
            )),
            fixed_overlays: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            iframes,
            audio: audio_map,
            video: video_map,
            js: js_engine,
            anims: Vec::new(),
            prev_opacity: std::collections::HashMap::new(),
        });
        self.scroll_y = 0.0;

        self.refresh_js_bounding_rects();
        self.refresh_js_computed_styles();
        self.start_css_transitions();
        self.prefetch_audio_elements();
        self.prefetch_video_elements();

        // Lifecycle: DOMContentLoaded fires now (parse complete + script
        // execution done), then `load` after layout/paint also done.
        self.fire_lifecycle_events();

        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Fire `DOMContentLoaded` and `load` on the document root. We
    /// don't separate the two timings since our pipeline doesn't have
    /// late-arriving resources (images are blocking-fetched during nav).
    fn fire_lifecycle_events(&mut self) {
        // Pull the root id without keeping a mutable borrow across the
        // dispatch + recascade cycle.
        let root = match self.page.as_ref() {
            Some(p) => first_element_child(&p.dom, p.dom.document()),
            None => None,
        };
        let Some(root) = root else { return };

        // DOMContentLoaded
        let r1 = match self.page.as_mut() {
            Some(p) => p.js.dispatch_event_with(
                &mut p.dom,
                "DOMContentLoaded",
                root,
                js::engine::EventInit::bubbling(),
            ),
            None => return,
        };
        if r1.mutated {
            self.recascade_and_paint();
        }

        // load — non-bubbling on the document/window in spec; we fire
        // on the root which serves the common addEventListener-on-window
        // pattern thanks to the `window === globalThis` alias.
        let r2 = match self.page.as_mut() {
            Some(p) => p.js.dispatch_event_with(
                &mut p.dom,
                "load",
                root,
                js::engine::EventInit::non_bubbling(),
            ),
            None => return,
        };
        if r2.mutated {
            self.recascade_and_paint();
        }
    }

    /// Total number of tabs (active + inactive).
    fn tab_count(&self) -> usize {
        self.tabs.len() + 1
    }

    /// Pull the active tab's snapshot fields out into a new
    /// `InactiveTab` and zero out the live ones. The caller is
    /// responsible for placing the snapshot somewhere.
    fn snapshot_active_tab(&mut self) -> InactiveTab {
        InactiveTab {
            page: self.page.take(),
            history: std::mem::take(&mut self.history),
            history_cursor: self.history_cursor.take(),
            scroll_y: self.scroll_y,
            url_bar: std::mem::take(&mut self.chrome.text),
            pending_url: None,
        }
    }

    /// Replace the live state with the contents of an `InactiveTab`.
    /// Doesn't touch `tabs` / `active_tab`.
    fn restore_into_active(&mut self, tab: InactiveTab) {
        self.page = tab.page;
        self.history = tab.history;
        self.history_cursor = tab.history_cursor;
        self.scroll_y = tab.scroll_y;
        self.chrome.text = tab.url_bar;
        if let Some(url) = tab.pending_url {
            self.navigate(&url, true);
        }
    }

    /// Switch to the tab at `index` (relative to all tabs, with the
    /// active tab counted at its `active_tab` position). No-op if
    /// already focused there or out of range.
    fn switch_to_tab(&mut self, index: usize) {
        if index >= self.tab_count() {
            return;
        }
        if index == self.active_tab {
            return;
        }
        // Materialise the inactive list with the current active tab
        // inserted at `active_tab`. Cheaper to operate on directly:
        // pull the target out, push the current active in its place.
        // Because `tabs` is "all non-active tabs in display order",
        // its indexing relative to `active_tab` is:
        //   display index < active_tab: tabs[display index]
        //   display index = active_tab: live (Browser fields)
        //   display index > active_tab: tabs[display index - 1]
        let active_pos = self.active_tab;
        let snapshot = self.snapshot_active_tab();
        if index < active_pos {
            // Pull tabs[index]; insert snapshot so the active slot
            // now lives where the live state used to be.
            let next = std::mem::take(&mut self.tabs[index]);
            self.tabs[index] = snapshot;
            self.restore_into_active(next);
            // active_tab now reflects the new display index.
            self.active_tab = index;
        } else {
            // index > active_pos: tabs[index - 1] is the target.
            let tabs_idx = index - 1;
            let next = std::mem::take(&mut self.tabs[tabs_idx]);
            self.tabs[tabs_idx] = snapshot;
            self.restore_into_active(next);
            self.active_tab = index;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Open a new tab and switch to it. The new tab is empty until
    /// the user navigates.
    fn open_new_tab(&mut self) {
        let active_pos = self.active_tab;
        let snapshot = self.snapshot_active_tab();
        // Insert the snapshot at the current active position.
        self.tabs.insert(active_pos, snapshot);
        // Start the new tab with a blank slate. `active_tab` is now
        // one past the old position (display order: ..., snapshot,
        // [new live]).
        self.active_tab = active_pos + 1;
        self.chrome.text = String::new();
        self.scroll_y = 0.0;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Close the tab at the given display index. If it's the active
    /// tab, switches to the previous one. If it's the last tab,
    /// closes the window (sets a pending exit flag the event loop
    /// honours).
    fn close_tab(&mut self, index: usize) {
        if self.tab_count() <= 1 {
            // Last tab — closing it should bring up an empty live
            // state rather than killing the window unsolicited.
            self.page = None;
            self.history.clear();
            self.history_cursor = None;
            self.scroll_y = 0.0;
            self.chrome.text.clear();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        if index == self.active_tab {
            // Close the active tab: pop the previous one (or next if
            // none earlier) into the live slot.
            let take_idx = if self.active_tab > 0 {
                self.active_tab - 1
            } else {
                // active_tab was 0; tabs[0] is now the display tab
                // at position 1.
                0
            };
            let restored = std::mem::take(&mut self.tabs[take_idx]);
            self.tabs.remove(take_idx);
            self.restore_into_active(restored);
            if take_idx < self.active_tab {
                self.active_tab -= 1;
            }
        } else if index < self.active_tab {
            self.tabs.remove(index);
            self.active_tab -= 1;
        } else {
            self.tabs.remove(index - 1);
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Push a Page into BFCache, evicting an existing entry for the
    /// same URL (so duplicate URLs in history don't bloat the cache)
    /// and the oldest entry if we'd exceed `BFCACHE_CAP`.
    fn bfcache_push(&mut self, page: Page, scroll_y: f32) {
        let url = page.url.clone();
        // Drop any existing entry for this URL so the newer state wins.
        self.bfcache.retain(|(u, _, _)| u != &url);
        self.bfcache.push((url, page, scroll_y));
        while self.bfcache.len() > BFCACHE_CAP {
            self.bfcache.remove(0);
        }
    }

    /// Take a cached Page for `url`, if any. Returns `(url, page,
    /// scroll_y)`.
    fn bfcache_take(&mut self, url: &url::Url) -> Option<(url::Url, Page, f32)> {
        let idx = self.bfcache.iter().position(|(u, _, _)| u == url)?;
        Some(self.bfcache.remove(idx))
    }

    fn reload(&mut self) {
        if let Some(url_str) = self.page.as_ref().map(|p| p.url.to_string()) {
            // Reload bypasses BFCache for that URL — spec behaviour.
            if let Ok(u) = url::Url::parse(&url_str) {
                self.bfcache.retain(|(cached, _, _)| cached != &u);
            }
            self.navigate(&url_str, false);
        }
    }

    /// Re-run layout + paint on the existing page without re-fetching. Used
    /// when the window is resized so we adapt to the new viewport width.
    fn re_layout(&mut self) {
        let page_h = self.page_viewport_height();
        let page_w = self.viewport_size.0;
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let viewport = layout::Rect {
            x: 0.0,
            y: 0.0,
            width: page_w as f32,
            height: page_h as f32,
        };
        let box_tree = layout::layout(&page.dom, &page.styles, &page.images, viewport);
        let mut max_bottom = page_h;
        for opt in &box_tree.boxes {
            if let Some(b) = opt {
                let bottom = (b.rect.y + b.rect.height).ceil() as u32;
                if bottom > max_bottom {
                    max_bottom = bottom;
                }
            }
        }
        let paint_h = max_bottom.min(PAINT_HEIGHT_CEILING);

        // Re-render iframes at the new viewport — their inner sizes follow
        // the new parent box.
        let new_iframes =
            render_iframes(&page.dom, &box_tree, &self.client, &page.url);

        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            *slot.borrow_mut() = Some(page.js.canvas_surfaces());
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            *slot.borrow_mut() = Some(page.js.video_elements());
        });
        paint::PAINT_CAPTURES.with(|slot| {
            *slot.borrow_mut() = Some(page.js.captures());
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            *slot.borrow_mut() = Some(page.js.capture_bindings());
        });
        paint::PAINT_LAYER_CACHE.with(|slot| {
            *slot.borrow_mut() = Some(page.layer_cache.clone());
        });
        // Drain any previous-pass fixed overlays and install the
        // shared Vec so the painter can refill it.
        page.fixed_overlays.borrow_mut().clear();
        paint::PAINT_FIXED_OVERLAYS.with(|slot| {
            *slot.borrow_mut() = Some(page.fixed_overlays.clone());
        });
        let painted = paint::paint(
            &page.dom,
            &page.styles,
            &box_tree,
            &page.images,
            page_w,
            paint_h,
        );
        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_LAYER_CACHE.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_FIXED_OVERLAYS.with(|slot| {
            slot.borrow_mut().take();
        });
        if let Some(mut pixmap) = painted {
            composite_iframes(&mut pixmap, &box_tree, &new_iframes);
            // Drive Intersection / Resize observers using the layout
            // we just computed. Done after paint so callback work
            // doesn't delay the visible update.
            page.js.tick_layout_observers(&mut page.dom, &box_tree);
            page.box_tree = box_tree;
            page.pixmap = pixmap;
            page.iframes = new_iframes;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn history_back(&mut self) {
        // The current page sits "above" history; we walk down into history.
        if self.history.is_empty() {
            return;
        }
        let target_idx = match self.history_cursor {
            None => self.history.len() - 1,
            Some(0) => return,
            Some(i) => i - 1,
        };
        let url = self.history[target_idx].to_string();
        self.history_cursor = Some(target_idx);
        self.navigate(&url, false);
    }

    fn history_forward(&mut self) {
        let cur = match self.history_cursor {
            Some(i) => i,
            None => return,
        };
        if cur + 1 >= self.history.len() {
            // Already at the tip of recorded history.
            self.history_cursor = None;
            return;
        }
        let url = self.history[cur + 1].to_string();
        self.history_cursor = Some(cur + 1);
        self.navigate(&url, false);
    }

    fn click_at(&mut self, x: f32, y: f32) {
        // Tab strip clicks: select-or-close on existing tabs, "+"
        // button opens a new tab.
        if y < TAB_STRIP_HEIGHT as f32 {
            let total = self.tab_count();
            let click_tab_idx = (x as u32 / TAB_WIDTH) as usize;
            if click_tab_idx < total {
                // Check if click landed on the close-button circle.
                let tab_origin_x = (click_tab_idx as u32) * TAB_WIDTH;
                let close_cx = (tab_origin_x + TAB_WIDTH - 14) as f32;
                let close_cy = (TAB_STRIP_HEIGHT / 2) as f32;
                let dx = x - close_cx;
                let dy = y - close_cy;
                if dx * dx + dy * dy <= TAB_CLOSE_RADIUS * TAB_CLOSE_RADIUS {
                    self.close_tab(click_tab_idx);
                } else {
                    self.switch_to_tab(click_tab_idx);
                }
            } else {
                let plus_x = (total as u32) * TAB_WIDTH;
                if x >= plus_x as f32 && x < (plus_x + NEW_TAB_BUTTON_WIDTH) as f32 {
                    self.open_new_tab();
                    self.chrome.focused = true;
                }
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        // URL bar clicks: focus it (Cmd+L style).
        if y < CHROME_HEIGHT as f32 {
            self.chrome.focused = true;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        let page_y = y - CHROME_HEIGHT as f32;
        if self.chrome.focused {
            self.chrome.focused = false;
            if let Some(p) = &self.page {
                self.chrome.text = p.url.to_string();
            }
        }

        // Hit-test first so we know which element to fire JS at.
        let hit_node: dom::NodeId = {
            let Some(page) = &self.page else {
                return;
            };
            let abs_y = page_y + self.scroll_y;
            match layout::hit_test(&page.dom, &page.box_tree, x, abs_y) {
                Some(n) => n,
                None => return,
            }
        };

        // If the click landed inside an `<iframe>` element, give the
        // iframe's own DOM a chance to handle it (typically a link
        // navigation that should re-load just that iframe). Returns true
        // when the iframe consumed the click — in that case we skip the
        // parent's built-in handlers + JS dispatch entirely.
        if self.try_handle_iframe_click(hit_node, x, page_y) {
            return;
        }

        // Dispatch JS click before any built-in handling so scripts get a
        // chance to `preventDefault()` (e.g., custom anchor handling).
        let js_result = {
            let Some(page) = self.page.as_mut() else {
                return;
            };
            // Fire pointerdown / pointerup before click so listeners
            // installed via PointerEvent see the input first. Each
            // event carries the same coords but stamps the pointer
            // surface (pointerId / pressure / pointerType=mouse).
            let mut pdown = js::engine::EventInit::bubbling();
            pdown.client_x = Some(x);
            pdown.client_y = Some(page_y);
            pdown.button = Some(0);
            pdown.pointer_id = Some(1);
            pdown.pointer_type = Some("mouse".to_string());
            pdown.is_primary = Some(true);
            pdown.pressure = Some(0.5);
            let _ = page.js.dispatch_event_with(
                &mut page.dom,
                "pointerdown",
                hit_node,
                pdown.clone(),
            );
            let _ = page.js.dispatch_event_with(
                &mut page.dom,
                "pointerup",
                hit_node,
                pdown,
            );
            // Touch parity: emit touchstart/touchend with a single
            // synthesized touch point so mobile-style libraries get a
            // signal even when the real source was a mouse click.
            let mut touch_init = js::engine::EventInit::bubbling();
            touch_init.client_x = Some(x);
            touch_init.client_y = Some(page_y);
            touch_init.touch_points = Some(vec![js::engine::TouchPoint {
                identifier: 0,
                client_x: x,
                client_y: page_y,
                radius_x: 1.0,
                radius_y: 1.0,
                force: 0.5,
            }]);
            let _ = page.js.dispatch_event_with(
                &mut page.dom,
                "touchstart",
                hit_node,
                touch_init.clone(),
            );
            let _ = page.js.dispatch_event_with(
                &mut page.dom,
                "touchend",
                hit_node,
                touch_init,
            );
            let mut init = js::engine::EventInit::bubbling();
            init.client_x = Some(x);
            init.client_y = Some(page_y);
            init.button = Some(0);
            page.js
                .dispatch_event_with(&mut page.dom, "click", hit_node, init)
        };
        if js_result.mutated {
            self.recascade_and_paint();
        }

        // Walk up the hit-tested node looking for: <a href> (navigate),
        // submit-type input/button (submit form), or focusable element
        // (input/textarea/select/button → focus).
        enum Action {
            None,
            Navigate(url::Url),
            Submit(dom::NodeId),
            Focus(dom::NodeId),
        }
        let action = if js_result.default_prevented {
            // Script handled it — skip built-in behaviour entirely.
            Action::None
        } else {
            let Some(page) = &self.page else {
                return;
            };
            let hit = hit_node;

            let mut cur = Some(hit);
            let mut chosen = Action::None;
            while let Some(n) = cur {
                if let dom::NodeKind::Element { tag, attrs } = &page.dom.node(n).kind {
                    match tag.as_str() {
                        "a" => {
                            if let Some((_, h)) = attrs.iter().find(|(k, _)| k == "href") {
                                if let Ok(abs) = page.url.join(h) {
                                    chosen = Action::Navigate(abs);
                                    break;
                                }
                            }
                        }
                        "button" => {
                            let ty = attr_value(attrs, "type").unwrap_or("submit");
                            if ty == "submit" {
                                chosen = Action::Submit(n);
                            } else {
                                chosen = Action::Focus(n);
                            }
                            break;
                        }
                        "input" => {
                            let ty = attr_value(attrs, "type").unwrap_or("text");
                            match ty {
                                "submit" | "image" => chosen = Action::Submit(n),
                                _ => chosen = Action::Focus(n),
                            }
                            break;
                        }
                        "textarea" | "select" => {
                            chosen = Action::Focus(n);
                            break;
                        }
                        _ => {}
                    }
                }
                cur = page.dom.node(n).parent;
            }
            chosen
        };

        match action {
            Action::None => {
                // Click on plain page content — unfocus any input.
                if let Some(p) = self.page.as_mut() {
                    if p.focus.is_some() {
                        p.focus = None;
                        self.recascade_and_paint();
                    }
                }
            }
            Action::Navigate(target) => self.navigate(&target.to_string(), true),
            Action::Submit(submitter) => self.submit_form_from(submitter),
            Action::Focus(node) => self.set_focus(Some(node)),
        }
    }

    fn set_focus(&mut self, node: Option<dom::NodeId>) {
        let changed = self
            .page
            .as_ref()
            .map(|p| p.focus != node)
            .unwrap_or(false);
        if !changed {
            return;
        }
        // Toggle the OS IME based on whether the new focus is an
        // editable input. Without this, winit never delivers
        // `WindowEvent::Ime(...)` and CJK / dead-key composition is
        // dropped on the floor.
        let editable = node
            .and_then(|n| self.page.as_ref().map(|p| (p, n)))
            .map(|(p, n)| is_editable_input(&p.dom, n))
            .unwrap_or(false);
        if let Some(w) = &self.window {
            w.set_ime_allowed(editable);
        }
        if let Some(p) = self.page.as_mut() {
            p.focus = node;
            // Stale preedit from a previous focus shouldn't visually
            // linger.
            p.input_preedit.clear();
        }
        self.recascade_and_paint();
    }

    /// Hit-test the page under the cursor; if hover changed, re-cascade
    /// with the new `:hover` chain and re-paint.
    fn update_hover(&mut self) {
        let (x, y) = self.cursor;
        let new_hover = if y < CHROME_HEIGHT as f32 {
            None
        } else if let Some(page) = &self.page {
            let abs_y = y - CHROME_HEIGHT as f32 + self.scroll_y;
            layout::hit_test(&page.dom, &page.box_tree, x, abs_y)
        } else {
            None
        };

        let changed = self
            .page
            .as_ref()
            .map(|p| p.hover != new_hover)
            .unwrap_or(false);
        if !changed {
            return;
        }

        if let Some(page) = self.page.as_mut() {
            page.hover = new_hover;
        }
        self.recascade_and_paint();
    }

    fn recascade_and_paint(&mut self) {
        let page_w = self.viewport_size.0;
        let css_viewport =
            css::Viewport::from_size(page_w as f32, self.page_viewport_height() as f32);
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let hover_chain = chain_of(&page.dom, page.hover);
        let focus_chain = chain_of(&page.dom, page.focus);
        let interaction = css::InteractionState {
            hover_chain: &hover_chain,
            focus_chain: &focus_chain,
        };
        page.styles = css::style_dom_with_viewport(
            &page.dom,
            &page.sheets,
            &interaction,
            &css_viewport,
        );
        // The cascade may not change layout, but conservatively refresh
        // rects so getBoundingClientRect doesn't return stale values
        // when interaction state flips and a re-layout runs upstack.
        let entries: Vec<(dom::NodeId, [f32; 4])> = page
            .box_tree
            .boxes
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let b = b.as_ref()?;
                let id = dom::NodeId::from_raw(i as u32);
                Some((id, [b.rect.x, b.rect.y, b.rect.width, b.rect.height]))
            })
            .collect();
        page.js.refresh_bounding_rects(entries);
        let style_snaps = computed_style_snapshots(&page.dom, &page.styles);
        page.js.refresh_computed_styles(style_snaps);
        self.start_css_transitions();
        // Re-apply currently running animation values so transitions
        // mid-flight survive a recascade caused by interaction state
        // changes.
        self.tick_css_animations();
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let max_bottom = page.pixmap.height();
        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            *slot.borrow_mut() = Some(page.js.canvas_surfaces());
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            *slot.borrow_mut() = Some(page.js.video_elements());
        });
        paint::PAINT_CAPTURES.with(|slot| {
            *slot.borrow_mut() = Some(page.js.captures());
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            *slot.borrow_mut() = Some(page.js.capture_bindings());
        });
        let painted = paint::paint(
            &page.dom,
            &page.styles,
            &page.box_tree,
            &page.images,
            page_w,
            max_bottom,
        );
        paint::PAINT_CANVAS_SURFACES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_VIDEO_ELEMENTS.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURES.with(|slot| {
            slot.borrow_mut().take();
        });
        paint::PAINT_CAPTURE_BINDINGS.with(|slot| {
            slot.borrow_mut().take();
        });
        if let Some(mut pixmap) = painted {
            composite_iframes(&mut pixmap, &page.box_tree, &page.iframes);
            page.pixmap = pixmap;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn handle_key(&mut self, logical_key: Key, text: Option<winit::keyboard::SmolStr>) {
        // Super / Cmd chords always take priority — they're browser commands.
        let cmd = self.modifiers.super_key() || self.modifiers.control_key();
        if cmd {
            match logical_key.as_ref() {
                Key::Character("r") | Key::Character("R") => {
                    self.reload();
                    return;
                }
                Key::Character("l") | Key::Character("L") => {
                    self.chrome.focused = true;
                    self.chrome.text.clear(); // select-all + clear, the common Cmd+L UX
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                Key::Character("t") | Key::Character("T") => {
                    self.open_new_tab();
                    self.chrome.focused = true;
                    return;
                }
                Key::Character("f") | Key::Character("F") => {
                    if self.find.is_none() {
                        self.find = Some(FindState::new());
                    } else {
                        self.find = None;
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                Key::Character("w") | Key::Character("W") => {
                    let idx = self.active_tab;
                    self.close_tab(idx);
                    return;
                }
                Key::Named(NamedKey::Tab) => {
                    // Cmd+Tab style cycle through tabs (Cmd+Tab is OS-
                    // reserved; using it inside the app is unusual but
                    // gives us a keyboard tab switcher.)
                    let count = self.tab_count();
                    if count > 1 {
                        let next = (self.active_tab + 1) % count;
                        self.switch_to_tab(next);
                    }
                    return;
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    self.history_back();
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.history_forward();
                    return;
                }
                _ => {}
            }
        }

        // F12 toggles the devtools overlay regardless of other
        // input states. Cmd+Opt+I matches Chrome / Safari muscle
        // memory.
        if matches!(logical_key.as_ref(), Key::Named(NamedKey::F12)) {
            self.devtools.toggle();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        if self.modifiers.super_key() && self.modifiers.alt_key() {
            if matches!(logical_key.as_ref(), Key::Character("i") | Key::Character("I")) {
                self.devtools.toggle();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return;
            }
        }

        // Devtools, when open, consumes keyboard input.
        if self.devtools.open {
            self.handle_devtools_key(logical_key, text);
            return;
        }

        if self.find.is_some() {
            self.handle_find_key(logical_key, text);
            return;
        }
        if self.chrome.focused {
            self.handle_chrome_key(logical_key, text);
            return;
        }

        // Dispatch a `keydown` JS event before the built-in handler so a
        // page script can `preventDefault()` (e.g., to block default
        // text-input behaviour for `<input>` and run its own logic).
        let key_str = logical_key_to_string(&logical_key, text.as_deref());
        let js_result = self.dispatch_key_event("keydown", &key_str);
        if js_result.default_prevented {
            return;
        }

        if self
            .page
            .as_ref()
            .map(|p| p.focus.is_some())
            .unwrap_or(false)
        {
            self.handle_input_key(logical_key, text);
        } else {
            self.handle_page_key(logical_key);
        }
    }

    fn handle_find_key(&mut self, key: Key, text: Option<winit::keyboard::SmolStr>) {
        let close = matches!(key.as_ref(), Key::Named(NamedKey::Escape));
        if close {
            self.find = None;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        let Some(find) = self.find.as_mut() else {
            return;
        };
        match key.as_ref() {
            Key::Named(NamedKey::Backspace) => {
                find.query.pop();
            }
            Key::Named(NamedKey::Enter) => {
                if !find.matches.is_empty() {
                    find.current = (find.current + 1) % find.matches.len();
                }
            }
            _ => {
                if let Some(s) = text {
                    let s: &str = s.as_ref();
                    if !s.is_empty() && !s.chars().any(|c| c.is_control()) {
                        find.query.push_str(s);
                    }
                }
            }
        }
        // Recompute matches on every keystroke.
        self.recompute_find_matches();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn recompute_find_matches(&mut self) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.matches.clear();
        find.current = 0;
        if find.query.is_empty() {
            return;
        }
        let needle = find.query.to_lowercase();
        let Some(page) = self.page.as_ref() else {
            return;
        };
        let mut stack: Vec<dom::NodeId> = vec![page.dom.document()];
        while let Some(n) = stack.pop() {
            if matches!(page.dom.node(n).kind, dom::NodeKind::Element { .. }) {
                let mut hay = String::new();
                collect_immediate_text(&page.dom, n, &mut hay);
                if hay.to_lowercase().contains(&needle) {
                    find.matches.push(n);
                }
            }
            let kids: Vec<dom::NodeId> = page.dom.children(n).collect();
            stack.extend(kids);
        }
    }

    fn dispatch_key_event(&mut self, event_type: &str, key: &str) -> js::engine::DispatchResult {
        let target = match self
            .page
            .as_ref()
            .and_then(|p| p.focus.or_else(|| first_element_child(&p.dom, p.dom.document())))
        {
            Some(n) => n,
            None => return js::engine::DispatchResult::default(),
        };
        let Some(page) = self.page.as_mut() else {
            return js::engine::DispatchResult::default();
        };
        let mut init = js::engine::EventInit::bubbling();
        init.key = Some(key.to_string());
        init.code = Some(key.to_string());
        init.ctrl = self.modifiers.control_key();
        init.shift = self.modifiers.shift_key();
        init.alt = self.modifiers.alt_key();
        init.meta = self.modifiers.super_key();
        let r = page
            .js
            .dispatch_event_with(&mut page.dom, event_type, target, init);
        if r.mutated {
            self.recascade_and_paint();
        }
        r
    }

    /// Translate a winit IME event into composition-event dispatch +
    /// per-input preedit / committed text. Active only when an
    /// editable input is focused; otherwise ignored.
    fn handle_ime(&mut self, ime: Ime) {
        let Some(focus_node) = self.page.as_ref().and_then(|p| p.focus) else {
            return;
        };
        match ime {
            Ime::Enabled | Ime::Disabled => {
                // No-op visually; just make sure stale preedit is
                // cleared when the IME pops off.
                if let Some(page) = self.page.as_mut() {
                    page.input_preedit.remove(&focus_node);
                }
            }
            Ime::Preedit(text, _cursor_range) => {
                let had_preedit = self
                    .page
                    .as_ref()
                    .map(|p| p.input_preedit.contains_key(&focus_node))
                    .unwrap_or(false);
                let new_empty = text.is_empty();
                // Fire compositionstart on the first non-empty preedit
                // of a sequence; compositionend when it goes empty.
                if !had_preedit && !new_empty {
                    self.dispatch_composition_event("compositionstart", "");
                }
                if let Some(page) = self.page.as_mut() {
                    if new_empty {
                        page.input_preedit.remove(&focus_node);
                    } else {
                        page.input_preedit.insert(focus_node, text.clone());
                    }
                }
                if !new_empty {
                    self.dispatch_composition_event("compositionupdate", &text);
                } else if had_preedit {
                    self.dispatch_composition_event("compositionend", "");
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Ime::Commit(text) => {
                let had_preedit = self
                    .page
                    .as_ref()
                    .map(|p| p.input_preedit.contains_key(&focus_node))
                    .unwrap_or(false);
                if !had_preedit {
                    // Some IMEs commit without a preceding preedit
                    // (e.g. dictation). Spec still wants the start /
                    // end pair around the commit.
                    self.dispatch_composition_event("compositionstart", "");
                }
                if let Some(page) = self.page.as_mut() {
                    page.input_preedit.remove(&focus_node);
                    if !text.is_empty() {
                        page.inputs
                            .entry(focus_node)
                            .or_default()
                            .push_str(&text);
                    }
                }
                self.dispatch_composition_event("compositionend", &text);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }

    fn dispatch_composition_event(&mut self, event_type: &str, data: &str) {
        let target = match self.page.as_ref().and_then(|p| p.focus) {
            Some(n) => n,
            None => return,
        };
        let Some(page) = self.page.as_mut() else { return };
        let mut init = js::engine::EventInit::bubbling();
        init.input_data = Some(data.to_string());
        let r = page
            .js
            .dispatch_event_with(&mut page.dom, event_type, target, init);
        if r.mutated {
            self.recascade_and_paint();
        }
    }

    fn handle_input_key(&mut self, logical_key: Key, text: Option<winit::keyboard::SmolStr>) {
        let focus_node = match self.page.as_ref().and_then(|p| p.focus) {
            Some(n) => n,
            None => return,
        };
        match logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => {
                self.set_focus(None);
                return;
            }
            Key::Named(NamedKey::Enter) => {
                // Enter inside a text input submits the enclosing form.
                self.submit_form_from(focus_node);
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(page) = self.page.as_mut() {
                    if let Some(val) = page.inputs.get_mut(&focus_node) {
                        val.pop();
                    }
                }
            }
            _ => {
                if let Some(s) = text {
                    let s: &str = s.as_ref();
                    if !s.is_empty() && !s.chars().any(|c| c.is_control()) {
                        if let Some(page) = self.page.as_mut() {
                            page.inputs
                                .entry(focus_node)
                                .or_default()
                                .push_str(s);
                        }
                    }
                }
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn handle_devtools_key(&mut self, logical_key: Key, text: Option<winit::keyboard::SmolStr>) {
        match logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => {
                self.devtools.open = false;
            }
            Key::Named(NamedKey::Tab) => {
                self.devtools.cycle_panel();
            }
            Key::Named(NamedKey::Enter) => {
                // Enter only submits in the Console panel; the other
                // panels are read-only.
                if matches!(self.devtools.panel, devtools::Panel::Console) {
                    if let Some(src) = self.devtools.submit() {
                        self.eval_in_devtools(&src);
                    }
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if matches!(self.devtools.panel, devtools::Panel::Console) {
                    self.devtools.input.pop();
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                if matches!(self.devtools.panel, devtools::Panel::Console) {
                    self.devtools.history_prev();
                }
            }
            Key::Named(NamedKey::ArrowDown) => {
                if matches!(self.devtools.panel, devtools::Panel::Console) {
                    self.devtools.history_next();
                }
            }
            _ => {
                if matches!(self.devtools.panel, devtools::Panel::Console) {
                    if let Some(s) = text {
                        let s: &str = s.as_ref();
                        if !s.is_empty() && !s.chars().any(|c| c.is_control()) {
                            self.devtools.input.push_str(s);
                        }
                    }
                }
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Evaluate a snippet against the current page's JS engine and
    /// stash the result into the devtools scrollback.
    fn eval_in_devtools(&mut self, src: &str) {
        let result = match self.page.as_mut() {
            Some(page) => page.js.eval_for_devtools(src),
            None => Err("no page loaded".to_string()),
        };
        match result {
            Ok(s) => self.devtools.push_result(s),
            Err(e) => {
                self.devtools.buffer.borrow_mut().push_back(devtools::ConsoleLine {
                    level: devtools::ConsoleLevel::Error,
                    text: e,
                });
            }
        }
    }

    fn handle_chrome_key(&mut self, logical_key: Key, text: Option<winit::keyboard::SmolStr>) {
        match logical_key.as_ref() {
            Key::Named(NamedKey::Enter) => {
                let url = self.chrome.text.clone();
                self.chrome.focused = false;
                // If the user typed a URL without scheme, default to https://
                let target = if url.contains("://") {
                    url
                } else if !url.is_empty() {
                    format!("https://{}", url)
                } else {
                    return;
                };
                self.navigate(&target, true);
            }
            Key::Named(NamedKey::Escape) => {
                self.chrome.focused = false;
                if let Some(p) = &self.page {
                    self.chrome.text = p.url.to_string();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Named(NamedKey::Backspace) => {
                self.chrome.text.pop();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {
                // Typeable character?
                if let Some(s) = text {
                    let s: &str = s.as_ref();
                    // Filter out control chars (newlines, tabs) that some keys emit.
                    if !s.is_empty() && !s.chars().any(|c| c.is_control()) {
                        self.chrome.text.push_str(s);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
        }
    }

    fn handle_page_key(&mut self, logical_key: Key) {
        match logical_key.as_ref() {
            Key::Character("r") | Key::Character("R") => self.reload(),
            Key::Named(NamedKey::ArrowLeft) => self.history_back(),
            Key::Named(NamedKey::ArrowRight) => self.history_forward(),
            Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                self.scroll_y =
                    (self.scroll_y + self.page_viewport_height() as f32 * 0.9).max(0.0);
                if let Some(page) = &self.page {
                    let max_scroll = (page.pixmap.height() as f32
                        - self.page_viewport_height() as f32)
                        .max(0.0);
                    self.scroll_y = self.scroll_y.min(max_scroll);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Named(NamedKey::PageUp) => {
                self.scroll_y =
                    (self.scroll_y - self.page_viewport_height() as f32 * 0.9).max(0.0);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Named(NamedKey::Home) => {
                self.scroll_y = 0.0;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Named(NamedKey::End) => {
                if let Some(page) = &self.page {
                    self.scroll_y = (page.pixmap.height() as f32
                        - self.page_viewport_height() as f32)
                        .max(0.0);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };

        let vw = w.get() as usize;
        let vh = h.get() as usize;
        let needed = vw.saturating_mul(vh);
        self.framebuf.clear();
        self.framebuf.resize(needed, 0x00FF_FFFF);
        let buffer = &mut self.framebuf[..];

        // (white page fill already applied during the `resize` above)

        // Blit the page pixmap *below* the chrome strip.
        if let Some(page) = &self.page {
            let pmap_w = page.pixmap.width() as usize;
            let pmap_h = page.pixmap.height() as usize;
            let scroll = self.scroll_y as usize;
            let page_top = CHROME_HEIGHT as usize;
            let page_rows = vh.saturating_sub(page_top);
            let visible_rows = page_rows.min(pmap_h.saturating_sub(scroll));
            let copy_cols = vw.min(pmap_w);
            let pmap_data = page.pixmap.data();
            for row in 0..visible_rows {
                let src_row = scroll + row;
                if src_row >= pmap_h {
                    break;
                }
                let src_off = src_row * pmap_w * 4;
                let dst_off = (page_top + row) * vw;
                for col in 0..copy_cols {
                    let s = src_off + col * 4;
                    let r = pmap_data[s];
                    let g = pmap_data[s + 1];
                    let b = pmap_data[s + 2];
                    buffer[dst_off + col] =
                        ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                }
            }
        }

        // Sticky positioning — re-blit pixmap regions of sticky elements
        // when scrolled past their threshold.
        if let Some(page) = &self.page {
            paint_sticky_overlays(buffer, vw as u32, vh as u32, page, self.scroll_y);
        }

        // `position: fixed` overlays are NOT blitted into the
        // main framebuffer here; they go to the GPU as separate
        // textures so the present path can composite them on top
        // of the scrolled view via textured quads. See the
        // present_with_overlays call below.

        // Devtools panel: draws on top of everything else, anchored
        // along the bottom of the window. Painted directly into the
        // softbuffer u32 surface like the chrome strips.
        if self.devtools.open {
            paint_devtools_panel(
                &mut self.chrome_font_system,
                &mut self.chrome_swash,
                buffer,
                vw as u32,
                vh as u32,
                &self.devtools,
                self.page.as_ref(),
            );
        }

        // Input overlays — typed values painted on top of the page pixmap.
        if let Some(page) = &self.page {
            paint_input_overlays(
                &mut self.chrome_font_system,
                &mut self.chrome_swash,
                buffer,
                vw as u32,
                vh as u32,
                page,
                self.scroll_y,
                CHROME_HEIGHT,
            );
        }

        // Find-in-page match highlights — paint yellow rects under
        // the matched elements before the chrome strips overlay them.
        if let (Some(find), Some(page)) = (self.find.as_ref(), self.page.as_ref()) {
            paint_find_highlights(
                buffer,
                vw as u32,
                vh as u32,
                page,
                self.scroll_y,
                CHROME_HEIGHT,
                find,
            );
        }

        // Tab strip + URL bar.
        paint_tab_strip(
            &mut self.chrome_font_system,
            &mut self.chrome_swash,
            buffer,
            vw as u32,
            self.active_tab,
            &self.tabs,
            &self.chrome.text,
            self.page.as_ref().map(|p| &p.url),
        );
        paint_chrome(
            &mut self.chrome_font_system,
            &mut self.chrome_swash,
            buffer,
            vw as u32,
            vh as u32,
            &self.chrome,
        );

        // Find bar overlay (above the URL bar's bottom border, below
        // any tab strip border).
        if let Some(find) = self.find.as_ref() {
            paint_find_bar(
                &mut self.chrome_font_system,
                &mut self.chrome_swash,
                buffer,
                vw as u32,
                find,
            );
        }

        // Repack u32 ARGB → BGRA bytes for the GPU upload. We use a
        // persistent buffer on Browser so we don't reallocate
        // every frame (was: per-frame Vec<u8> via flat_map +
        // collect, ~viewport*4 byte alloc per redraw).
        let needed_bytes = buffer.len() * 4;
        if self.byte_buf.len() != needed_bytes {
            self.byte_buf.resize(needed_bytes, 0xFF);
        }
        for (i, p) in buffer.iter().enumerate() {
            let off = i * 4;
            self.byte_buf[off] = (*p & 0xFF) as u8;
            self.byte_buf[off + 1] = ((*p >> 8) & 0xFF) as u8;
            self.byte_buf[off + 2] = ((*p >> 16) & 0xFF) as u8;
            self.byte_buf[off + 3] = 0xFF;
        }
        let pixels_u8: &[u8] = &self.byte_buf;

        // Build the GPU-side overlay list from the painter's
        // fixed-position pixmaps. The GpuPresenter samples each
        // overlay texture at its viewport-relative pixel rect,
        // skipping the per-pixel CPU blit we'd otherwise pay.
        let mut overlay_bgra_storage: Vec<Vec<u8>> = Vec::new();
        let mut overlay_meta: Vec<(u32, u32, f32, f32)> = Vec::new();
        if let Some(page) = &self.page {
            let overlays = page.fixed_overlays.borrow();
            for overlay in overlays.iter() {
                let w = overlay.pixmap.width();
                let h = overlay.pixmap.height();
                // tiny_skia pixmaps are premultiplied RGBA; the GPU
                // sampler wants BGRA8UnormSrgb. Swizzle on upload.
                let src = overlay.pixmap.data();
                let mut dst = Vec::with_capacity(src.len());
                for chunk in src.chunks_exact(4) {
                    dst.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
                }
                overlay_bgra_storage.push(dst);
                overlay_meta.push((
                    w,
                    h,
                    overlay.dest_x,
                    CHROME_HEIGHT as f32 + overlay.dest_y,
                ));
            }
        }
        let overlay_layers: Vec<gpu::OverlayLayer<'_>> = overlay_bgra_storage
            .iter()
            .zip(overlay_meta.iter())
            .map(|(bytes, meta)| gpu::OverlayLayer {
                bgra: bytes,
                width: meta.0,
                height: meta.1,
                dest_x: meta.2,
                dest_y: meta.3,
            })
            .collect();
        if overlay_layers.is_empty() {
            surface.present(&pixels_u8, vw as u32, vh as u32);
        } else {
            surface.present_with_overlays(
                &pixels_u8,
                vw as u32,
                vh as u32,
                &overlay_layers,
            );
        }
    }
}

// ---------------- DOM helpers ----------------

fn chain_of(dom: &dom::Dom, node: Option<dom::NodeId>) -> Vec<dom::NodeId> {
    let mut out = Vec::new();
    let mut cur = node;
    while let Some(n) = cur {
        out.push(n);
        cur = dom.node(n).parent;
    }
    out
}

/// Apply a CSS timing function to linear progress in [0, 1].
fn ease(t: f32, kind: css::TimingFunction) -> f32 {
    match kind {
        css::TimingFunction::Linear => t,
        css::TimingFunction::EaseIn => t * t,
        css::TimingFunction::EaseOut => 1.0 - (1.0 - t).powi(2),
        css::TimingFunction::EaseInOut => {
            if t < 0.5 {
                2.0 * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
            }
        }
        css::TimingFunction::Ease => {
            // Approximation of the `ease` cubic-bezier.
            1.0 - (1.0 - t).powi(3)
        }
    }
}

/// Map a winit `Key` (plus any `text` payload for character keys) to a
/// short string suitable for `KeyboardEvent.key` / `.code`. We don't
/// distinguish the two — close enough for the toy.
fn logical_key_to_string(key: &Key, text: Option<&str>) -> String {
    if let Some(s) = text {
        if !s.is_empty() && !s.chars().any(|c| c.is_control()) {
            return s.to_string();
        }
    }
    match key.as_ref() {
        Key::Named(NamedKey::Enter) => "Enter".into(),
        Key::Named(NamedKey::Backspace) => "Backspace".into(),
        Key::Named(NamedKey::Tab) => "Tab".into(),
        Key::Named(NamedKey::Escape) => "Escape".into(),
        Key::Named(NamedKey::ArrowLeft) => "ArrowLeft".into(),
        Key::Named(NamedKey::ArrowRight) => "ArrowRight".into(),
        Key::Named(NamedKey::ArrowUp) => "ArrowUp".into(),
        Key::Named(NamedKey::ArrowDown) => "ArrowDown".into(),
        Key::Named(NamedKey::Space) => " ".into(),
        Key::Named(NamedKey::Shift) => "Shift".into(),
        Key::Named(NamedKey::Control) => "Control".into(),
        Key::Named(NamedKey::Alt) => "Alt".into(),
        Key::Named(NamedKey::Super) => "Meta".into(),
        Key::Character(s) => s.to_string(),
        _ => String::new(),
    }
}

fn collect_video_sources(
    dom: &dom::Dom,
    node: dom::NodeId,
    out: &mut Vec<(dom::NodeId, String, bool, bool)>,
) {
    if let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "video" {
            let src = attrs
                .iter()
                .find(|(k, _)| k == "src")
                .map(|(_, v)| v.clone())
                .or_else(|| {
                    dom.children(node).find_map(|c| {
                        if let dom::NodeKind::Element { tag: t, attrs: a } = &dom.node(c).kind {
                            if t == "source" {
                                return a
                                    .iter()
                                    .find(|(k, _)| k == "src")
                                    .map(|(_, v)| v.clone());
                            }
                        }
                        None
                    })
                });
            if let Some(src) = src.filter(|s| !s.is_empty()) {
                let autoplay = attrs.iter().any(|(k, _)| k == "autoplay");
                let loop_ = attrs.iter().any(|(k, _)| k == "loop");
                out.push((node, src, autoplay, loop_));
            }
        }
    }
    for c in dom.children(node).collect::<Vec<_>>() {
        collect_video_sources(dom, c, out);
    }
}

fn collect_audio_sources(
    dom: &dom::Dom,
    node: dom::NodeId,
    out: &mut Vec<(dom::NodeId, String, bool, bool, f32)>,
) {
    if let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "audio" {
            let src = attrs
                .iter()
                .find(|(k, _)| k == "src")
                .map(|(_, v)| v.clone());
            let src = src.or_else(|| {
                // <audio><source src="..."></audio> — pick the first child source.
                dom.children(node).find_map(|c| {
                    if let dom::NodeKind::Element { tag: t, attrs: a } = &dom.node(c).kind {
                        if t == "source" {
                            return a.iter().find(|(k, _)| k == "src").map(|(_, v)| v.clone());
                        }
                    }
                    None
                })
            });
            if let Some(src) = src.filter(|s| !s.is_empty()) {
                let autoplay = attrs.iter().any(|(k, _)| k == "autoplay");
                let loop_ = attrs.iter().any(|(k, _)| k == "loop");
                let volume = attrs
                    .iter()
                    .find(|(k, _)| k == "volume")
                    .and_then(|(_, v)| v.parse::<f32>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                out.push((node, src, autoplay, loop_, volume));
            }
        }
    }
    for c in dom.children(node).collect::<Vec<_>>() {
        collect_audio_sources(dom, c, out);
    }
}

/// Build the per-element computed-style snapshots backing
/// `getComputedStyle()`. We export the handful of properties scripts
/// actually probe — adding more is cheap if a script needs it.
fn computed_style_snapshots(
    dom: &dom::Dom,
    styles: &css::StyleTree,
) -> Vec<(dom::NodeId, Vec<(String, String)>)> {
    let mut out = Vec::new();
    fn walk(
        dom: &dom::Dom,
        styles: &css::StyleTree,
        node: dom::NodeId,
        out: &mut Vec<(dom::NodeId, Vec<(String, String)>)>,
    ) {
        if matches!(dom.node(node).kind, dom::NodeKind::Element { .. }) {
            let s = styles.get(node);
            let pairs = vec![
                ("color".to_string(), color_to_css(s.color)),
                (
                    "background-color".to_string(),
                    color_to_css(s.background_color),
                ),
                ("display".to_string(), display_to_css(&s.display)),
                ("font-size".to_string(), format!("{}px", s.font_size)),
                ("font-weight".to_string(), s.font_weight.to_string()),
                ("font-style".to_string(), font_style_to_css(s.font_style)),
                ("line-height".to_string(), format!("{}", s.line_height)),
                ("opacity".to_string(), format!("{}", s.opacity)),
                ("text-align".to_string(), text_align_to_css(&s.text_align)),
                ("position".to_string(), position_to_css(&s.position)),
                ("z-index".to_string(), s.z_index.map(|z| z.to_string()).unwrap_or_else(|| "auto".into())),
                ("visibility".to_string(), "visible".to_string()),
            ];
            out.push((node, pairs));
        }
        for c in dom.children(node).collect::<Vec<_>>() {
            walk(dom, styles, c, out);
        }
    }
    walk(dom, styles, dom.document(), &mut out);
    out
}

fn color_to_css(c: css::Color) -> String {
    if c.a == 0 {
        "rgba(0, 0, 0, 0)".into()
    } else if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else {
        format!(
            "rgba({}, {}, {}, {})",
            c.r,
            c.g,
            c.b,
            c.a as f32 / 255.0
        )
    }
}

fn display_to_css(d: &css::Display) -> String {
    match d {
        css::Display::Block => "block",
        css::Display::Inline => "inline",
        css::Display::InlineBlock => "inline-block",
        css::Display::ListItem => "list-item",
        css::Display::Flex => "flex",
        css::Display::InlineFlex => "inline-flex",
        css::Display::Grid => "grid",
        css::Display::InlineGrid => "inline-grid",
        css::Display::None => "none",
    }
    .to_string()
}

fn font_style_to_css(s: css::FontStyle) -> String {
    match s {
        css::FontStyle::Normal => "normal",
        css::FontStyle::Italic => "italic",
    }
    .to_string()
}

fn text_align_to_css(t: &css::TextAlign) -> String {
    match t {
        css::TextAlign::Left => "left",
        css::TextAlign::Right => "right",
        css::TextAlign::Center => "center",
        css::TextAlign::Justify => "justify",
        css::TextAlign::Start => "start",
        css::TextAlign::End => "end",
    }
    .to_string()
}

fn position_to_css(p: &css::Position) -> String {
    match p {
        css::Position::Static => "static",
        css::Position::Relative => "relative",
        css::Position::Absolute => "absolute",
        css::Position::Fixed => "fixed",
        css::Position::Sticky => "sticky",
    }
    .to_string()
}

/// Concatenate the immediate text-node children of `node` (no
/// descent into element children). Used by find-in-page to test the
/// shallowest text content for a match.
fn collect_immediate_text(dom: &dom::Dom, node: dom::NodeId, out: &mut String) {
    for c in dom.children(node).collect::<Vec<_>>() {
        if let dom::NodeKind::Text(t) = &dom.node(c).kind {
            out.push_str(t);
        }
    }
}

/// First element child of `parent` (skipping text/comments). Used by
/// lifecycle events to find the document's <html> root.
/// Does this node accept text input? Used to decide whether to
/// enable the OS IME — we don't want every focused link / button to
/// pop up the candidate window.
fn is_editable_input(dom: &dom::Dom, node: dom::NodeId) -> bool {
    let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind else {
        return false;
    };
    match tag.as_str() {
        "textarea" => true,
        "input" => {
            let ty = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("type"))
                .map(|(_, v)| v.to_ascii_lowercase())
                .unwrap_or_else(|| "text".to_string());
            matches!(
                ty.as_str(),
                "text" | "search" | "url" | "email" | "tel" | "password" | "number"
            )
        }
        _ => attrs
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("contenteditable") && v != "false"),
    }
}

fn first_element_child(dom: &dom::Dom, parent: dom::NodeId) -> Option<dom::NodeId> {
    dom.children(parent)
        .find(|c| matches!(dom.node(*c).kind, dom::NodeKind::Element { .. }))
}

/// Walk from `node` up through its ancestors looking for an element
/// whose tag matches `tag_name`. Returns the matching ancestor (or
/// `node` itself if it qualifies). Used by click handling to decide
/// whether the click landed inside an `<iframe>` element.
fn ancestor_with_tag(dom: &dom::Dom, node: dom::NodeId, tag_name: &str) -> Option<dom::NodeId> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if let dom::NodeKind::Element { tag, .. } = &dom.node(n).kind {
            if tag == tag_name {
                return Some(n);
            }
        }
        cur = dom.node(n).parent;
    }
    None
}

fn attr_value<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Seed `<input value="...">` defaults into the per-input value map so the
/// user sees the page's preset text before typing anything.
fn seed_input_values(
    dom: &dom::Dom,
    node: dom::NodeId,
    out: &mut std::collections::HashMap<dom::NodeId, String>,
) {
    if let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "input" || tag == "textarea" {
            let v = attr_value(attrs, "value").unwrap_or("").to_string();
            out.insert(node, v);
        }
    }
    let kids: Vec<dom::NodeId> = dom.children(node).collect();
    for c in kids {
        seed_input_values(dom, c, out);
    }
}

// ---------------- Form submission ----------------

impl Browser {
    /// Submit the form ancestor of `submitter` (the clicked button or input).
    fn submit_form_from(&mut self, submitter: dom::NodeId) {
        let Some(page) = &self.page else {
            return;
        };
        // Find the enclosing <form>.
        let mut form_node = None;
        let mut cur = Some(submitter);
        while let Some(n) = cur {
            if let dom::NodeKind::Element { tag, .. } = &page.dom.node(n).kind {
                if tag == "form" {
                    form_node = Some(n);
                    break;
                }
            }
            cur = page.dom.node(n).parent;
        }
        let Some(form) = form_node else {
            return;
        };

        // Read form attributes.
        let (action, method) = if let dom::NodeKind::Element { attrs, .. } = &page.dom.node(form).kind {
            (
                attr_value(attrs, "action").unwrap_or("").to_string(),
                attr_value(attrs, "method")
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "get".to_string()),
            )
        } else {
            return;
        };

        // Collect fields. Walk all <input>/<textarea>/<select> descendants
        // of the form. Use the typed value if present, otherwise the seeded
        // attribute value.
        let mut fields: Vec<(String, String)> = Vec::new();
        collect_form_fields(&page.dom, &page.inputs, form, &mut fields);

        // urlencode.
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &fields {
            serializer.append_pair(k, v);
        }
        let encoded = serializer.finish();

        // Resolve action URL.
        let action_url = if action.is_empty() {
            page.url.clone()
        } else {
            match page.url.join(&action) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("[form] bad action {action:?}: {e}");
                    return;
                }
            }
        };
        let allow_loopback = std::env::var("DABOSS_ALLOW_LOOPBACK").is_ok();
        let _ = allow_loopback;

        eprintln!("[form] {method} {action_url} body={} bytes", encoded.len());
        if method == "post" {
            // POST → body is the encoded form, content-type is the urlencoded mime.
            match self
                .client
                .post(&action_url.to_string(), encoded.into_bytes(), "application/x-www-form-urlencoded")
            {
                Ok(_resp) => {
                    // For simplicity, re-route through navigate() to render
                    // the response by re-fetching with GET. Real browsers
                    // render the POST response directly; the toy keeps the
                    // pipeline simpler at the cost of one extra fetch.
                    self.navigate(&action_url.to_string(), true);
                }
                Err(e) => eprintln!("[form] POST failed: {e}"),
            }
        } else {
            // GET → append ?query to action and navigate.
            let mut target = action_url.clone();
            target.set_query(if encoded.is_empty() { None } else { Some(&encoded) });
            self.navigate(&target.to_string(), true);
        }
    }
}

fn collect_form_fields(
    dom: &dom::Dom,
    inputs: &std::collections::HashMap<dom::NodeId, String>,
    node: dom::NodeId,
    out: &mut Vec<(String, String)>,
) {
    if let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        match tag.as_str() {
            "input" | "textarea" => {
                let name = attr_value(attrs, "name");
                if let Some(name) = name {
                    // Skip non-submitting input types.
                    let ty = attr_value(attrs, "type").unwrap_or("text");
                    let submitting = !matches!(
                        ty,
                        "submit" | "button" | "image" | "reset" | "file" | "checkbox" | "radio"
                    );
                    if submitting {
                        let val = inputs.get(&node).cloned().unwrap_or_default();
                        out.push((name.to_string(), val));
                    }
                }
            }
            _ => {}
        }
    }
    let kids: Vec<dom::NodeId> = dom.children(node).collect();
    for c in kids {
        collect_form_fields(dom, inputs, c, out);
    }
}

// ---------------- Iframe rendering ----------------

/// Walk the parent DOM for `<iframe>` elements. For each one with a
/// resolvable `src`, fetch + parse + style + layout + paint the nested
/// document into a pixmap sized to the iframe's own layout box.
///
/// Caps at `MAX_IFRAMES` per page and never recurses into iframes-inside-
/// iframes (the nested mini-pipeline runs against a fresh empty cache).
fn render_iframes(
    parent_dom: &dom::Dom,
    parent_box_tree: &layout::BoxTree,
    client: &net::Client,
    base_url: &url::Url,
) -> std::collections::HashMap<dom::NodeId, IframeContent> {
    let mut out = std::collections::HashMap::new();
    let mut count = 0usize;
    walk_iframes(
        parent_dom,
        parent_box_tree,
        client,
        base_url,
        parent_dom.document(),
        &mut out,
        &mut count,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_iframes(
    parent_dom: &dom::Dom,
    parent_box_tree: &layout::BoxTree,
    client: &net::Client,
    base_url: &url::Url,
    node: dom::NodeId,
    out: &mut std::collections::HashMap<dom::NodeId, IframeContent>,
    count: &mut usize,
) {
    if *count >= MAX_IFRAMES {
        return;
    }
    if let dom::NodeKind::Element { tag, attrs } = &parent_dom.node(node).kind {
        if tag == "iframe" {
            *count += 1;
            let src = attrs
                .iter()
                .find(|(k, _)| k == "src")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            let sandbox = attrs.iter().any(|(k, _)| k == "sandbox");
            if !src.is_empty() {
                if let Some(b) = parent_box_tree.get(node) {
                    let w = b.rect.width.max(1.0) as u32;
                    let h = b.rect.height.max(1.0) as u32;
                    if let Some(content) =
                        load_iframe_document(client, base_url, src, w, h, sandbox)
                    {
                        out.insert(node, content);
                    }
                }
            }
        }
    }
    let kids: Vec<dom::NodeId> = parent_dom.children(node).collect();
    for c in kids {
        walk_iframes(parent_dom, parent_box_tree, client, base_url, c, out, count);
    }
}

fn load_iframe_document(
    client: &net::Client,
    base_url: &url::Url,
    src: &str,
    width: u32,
    height: u32,
    sandbox: bool,
) -> Option<IframeContent> {
    let abs = base_url.join(src).ok()?;
    let url_str = abs.to_string();
    let ctx = net::RequestContext::new().with_initiator(base_url.clone());
    let response = match client.get_with(&url_str, ctx) {
        Ok(r) if (200..400).contains(&r.status) => r,
        Ok(r) => {
            eprintln!("[iframe] {url_str}: HTTP {}", r.status);
            return None;
        }
        Err(e) => {
            eprintln!("[iframe] {url_str}: {e}");
            return None;
        }
    };

    // X-Frame-Options / CSP frame-ancestors enforcement. `DENY` refuses
    // any embedding; `SAMEORIGIN` allows only same-origin parents.
    if let Some(xfo) = response.header("X-Frame-Options") {
        let val = xfo.trim().to_ascii_lowercase();
        let blocked = match val.as_str() {
            "deny" => true,
            "sameorigin" => abs.origin() != base_url.origin(),
            _ => false,
        };
        if blocked {
            eprintln!("[iframe] {url_str}: refused via X-Frame-Options: {val}");
            return None;
        }
    }
    if let Some(csp_raw) = response.header("Content-Security-Policy") {
        // The `frame-ancestors` directive may contain `'none'` or
        // explicit origins. We honour `'none'` and `'self'` only.
        for directive in csp_raw.split(';') {
            let parts: Vec<&str> = directive.split_ascii_whitespace().collect();
            if parts.first().map(|s| s.to_ascii_lowercase()) == Some("frame-ancestors".into()) {
                let sources: Vec<String> = parts[1..]
                    .iter()
                    .map(|s| s.to_ascii_lowercase())
                    .collect();
                let allowed = if sources.iter().any(|s| s == "'none'") {
                    false
                } else if sources.iter().any(|s| s == "*") {
                    true
                } else if sources.iter().any(|s| s == "'self'") {
                    abs.origin() == base_url.origin()
                } else {
                    // Specific hostnames — match against parent origin.
                    let parent_origin = base_url.origin().ascii_serialization();
                    sources.iter().any(|s| s == &parent_origin)
                };
                if !allowed {
                    eprintln!(
                        "[iframe] {url_str}: refused by Content-Security-Policy frame-ancestors"
                    );
                    return None;
                }
            }
        }
    }
    let body = String::from_utf8_lossy(&response.body).into_owned();
    let dom = html::parse(&body);

    // External stylesheets (fetched through the same client; same caps).
    let mut sheets: Vec<css::Stylesheet> = Vec::new();
    let mut ext_count = 0usize;
    for r in css::discover_stylesheets(&dom) {
        match r {
            css::StylesheetRef::Embedded(s) => sheets.push(s),
            css::StylesheetRef::External { href, integrity } => {
                if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                    continue;
                }
                ext_count += 1;
                if let Ok(child_abs) = abs.join(&href) {
                    if let Ok(resp) = client.get(&child_abs.to_string()) {
                        if (200..300).contains(&resp.status)
                            && stylesheet_integrity_ok(
                                &href,
                                integrity.as_deref(),
                                &resp.body,
                            )
                        {
                            sheets.push(css::parse(&String::from_utf8_lossy(&resp.body)));
                        }
                    }
                }
            }
        }
    }
    let style_tree = css::style_dom(&dom, &sheets);

    prefetch_link_resources(&dom, client, &abs);

    let mut images = layout::ImageCache::new();
    prefetch_images(&dom, client, &abs, &mut images);
    prefetch_background_images(&dom, &style_tree, client, &abs, &mut images);

    let viewport = layout::Rect {
        x: 0.0,
        y: 0.0,
        width: width as f32,
        height: height as f32,
    };
    let box_tree = layout::layout(&dom, &style_tree, &images, viewport);

    // For iframes, paint into a pixmap of the iframe's exact box size —
    // any overflow is clipped (CSS spec: iframe default is `overflow: hidden`).
    let pixmap = paint::paint(&dom, &style_tree, &box_tree, &images, width, height)?;
    eprintln!("[iframe] rendered {url_str} → {width}x{height}");
    Some(IframeContent {
        url: abs,
        dom,
        box_tree,
        pixmap,
        sandbox,
    })
}

/// Composite each iframe's pixmap into the parent pixmap at its layout
/// box position. Done after the parent paint so iframe content overdraws
/// the parent's default white fill for that region.
fn composite_iframes(
    parent_pixmap: &mut Pixmap,
    parent_box_tree: &layout::BoxTree,
    iframes: &std::collections::HashMap<dom::NodeId, IframeContent>,
) {
    for (node, iframe) in iframes {
        let Some(b) = parent_box_tree.get(*node) else {
            continue;
        };
        let transform = tiny_skia::Transform::from_translate(b.rect.x, b.rect.y);
        parent_pixmap.draw_pixmap(
            0,
            0,
            iframe.pixmap.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            transform,
            None,
        );
    }
}

// ---------------- Sticky overlays ----------------

/// Walk the DOM for `position: sticky` elements. For each, if the natural
/// position has scrolled above the threshold (`top` offset measured from
/// the bottom of the chrome strip), copy the element's pixmap region to
/// the threshold position. This re-paints the sticky element so it appears
/// pinned to the top of the viewport even while the page scrolls beneath.
fn paint_sticky_overlays(
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    page: &Page,
    scroll_y: f32,
) {
    walk_sticky(buffer, vw, vh, page, scroll_y, page.dom.document());
}

fn walk_sticky(
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    page: &Page,
    scroll_y: f32,
    node: dom::NodeId,
) {
    if let dom::NodeKind::Element { .. } = &page.dom.node(node).kind {
        let style = page.styles.get(node);
        if style.position == css::Position::Sticky {
            if let Some(b) = page.box_tree.get(node) {
                let top_offset = style.top.unwrap_or(0.0);
                // Element's natural y in viewport coords (after subtracting
                // scroll + chrome offset).
                let natural_top_in_page = b.rect.y;
                let threshold_in_page = scroll_y + top_offset;
                if natural_top_in_page < threshold_in_page {
                    // Scrolled past — pin to threshold.
                    blit_pixmap_region(
                        buffer,
                        vw,
                        vh,
                        &page.pixmap,
                        b.rect.x as i32,
                        b.rect.y as i32,
                        b.rect.width.ceil() as i32,
                        b.rect.height.ceil() as i32,
                        b.rect.x as i32,
                        (CHROME_HEIGHT as f32 + top_offset) as i32,
                    );
                }
            }
        }
    }
    let kids: Vec<dom::NodeId> = page.dom.children(node).collect();
    for c in kids {
        walk_sticky(buffer, vw, vh, page, scroll_y, c);
    }
}

/// Blit a `(src_w × src_h)` region of `src` starting at `(src_x, src_y)`
/// into `buffer` (laid out as `vw` × `vh`) at destination `(dst_x, dst_y)`.
/// Clips to both pixmap and surface bounds.
#[allow(clippy::too_many_arguments)]
fn blit_pixmap_region(
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    src: &Pixmap,
    src_x: i32,
    src_y: i32,
    src_w: i32,
    src_h: i32,
    dst_x: i32,
    dst_y: i32,
) {
    let pmap_w = src.width() as i32;
    let pmap_h = src.height() as i32;
    let data = src.data();
    for row in 0..src_h {
        let sy = src_y + row;
        let dy = dst_y + row;
        if sy < 0 || sy >= pmap_h || dy < 0 || dy >= vh as i32 {
            continue;
        }
        for col in 0..src_w {
            let sx = src_x + col;
            let dx = dst_x + col;
            if sx < 0 || sx >= pmap_w || dx < 0 || dx >= vw as i32 {
                continue;
            }
            let src_idx = (sy as usize * pmap_w as usize + sx as usize) * 4;
            let r = data[src_idx];
            let g = data[src_idx + 1];
            let b = data[src_idx + 2];
            let dst_idx = dy as usize * vw as usize + dx as usize;
            buffer[dst_idx] = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        }
    }
}

// ---------------- Input overlays ----------------

/// Walk the DOM for `<input>` / `<textarea>` elements, render their typed
/// value (if any) into their layout box, and draw a blinking-ish caret
/// when the input is focused. Painted directly onto the softbuffer u32
/// surface so we don't have to invalidate the page pixmap on every keystroke.
fn paint_input_overlays(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    page: &Page,
    scroll_y: f32,
    top_offset: u32,
) {
    paint_input_overlays_walk(
        font_system,
        swash_cache,
        buffer,
        vw,
        vh,
        page,
        scroll_y,
        top_offset,
        page.dom.document(),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_input_overlays_walk(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    page: &Page,
    scroll_y: f32,
    top_offset: u32,
    node: dom::NodeId,
) {
    if let dom::NodeKind::Element { tag, .. } = &page.dom.node(node).kind {
        if tag == "input" || tag == "textarea" {
            let value = page.inputs.get(&node).map(String::as_str).unwrap_or("");
            let preedit = page.input_preedit.get(&node).map(String::as_str).unwrap_or("");
            let is_focused = page.focus == Some(node);
            // While the IME is composing, render the preedit text
            // concatenated to the committed value. Visually this is
            // less elegant than the underlined-pre-edit treatment
            // browsers use, but it's good enough to see CJK
            // candidates landing.
            let display: String = if preedit.is_empty() {
                value.to_string()
            } else {
                format!("{value}{preedit}")
            };
            if let Some(b) = page.box_tree.get(node) {
                draw_input_text(
                    font_system,
                    swash_cache,
                    buffer,
                    vw,
                    vh,
                    b.rect,
                    &display,
                    is_focused,
                    scroll_y,
                    top_offset,
                );
            }
        }
    }
    let kids: Vec<dom::NodeId> = page.dom.children(node).collect();
    for c in kids {
        paint_input_overlays_walk(
            font_system,
            swash_cache,
            buffer,
            vw,
            vh,
            page,
            scroll_y,
            top_offset,
            c,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_input_text(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    rect: layout::Rect,
    value: &str,
    focused: bool,
    scroll_y: f32,
    top_offset: u32,
) {
    // Translate page coordinates to surface coordinates.
    let surface_x = rect.x;
    let surface_y = rect.y - scroll_y + top_offset as f32;
    // Skip entirely off-screen inputs.
    if surface_y + rect.height < top_offset as f32 || surface_y >= vh as f32 {
        return;
    }

    let pad_x: f32 = 4.0;
    let pad_y: f32 = 2.0;
    if !value.is_empty() {
        let metrics = Metrics::new(14.0, 18.0);
        let mut tb = Buffer::new(font_system, metrics);
        tb.set_size(font_system, Some((rect.width - pad_x * 2.0).max(1.0)), None);
        tb.set_wrap(font_system, Wrap::None);
        let attrs = Attrs::new().family(Family::SansSerif);
        tb.set_text(font_system, value, attrs, Shaping::Advanced);
        tb.shape_until_scroll(font_system, false);

        let text_color = CtColor::rgb(20, 20, 25);
        let pmap_w = vw as i32;
        let pmap_h = vh as i32;
        let bar_top = top_offset as i32;
        let mut last_right = (surface_x + pad_x) as i32;

        for run in tb.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((surface_x + pad_x, surface_y + pad_y + run.line_y), 1.0);
                let cache_key = physical.cache_key;
                let glyph_x = physical.x;
                let glyph_y = physical.y;
                last_right = glyph_x + glyph.w as i32;
                swash_cache.with_pixels(font_system, cache_key, text_color, |x_off, y_off, color| {
                    let px = glyph_x + x_off;
                    let py = glyph_y + y_off;
                    if px < 0 || py < bar_top || px >= pmap_w || py >= pmap_h {
                        return;
                    }
                    let idx = py as usize * pmap_w as usize + px as usize;
                    let src_a = color.a();
                    if src_a == 0 {
                        return;
                    }
                    let inv_a = 255u32 - src_a as u32;
                    let dst = buffer[idx];
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let sr = color.r() as u32 * src_a as u32 / 255;
                    let sg = color.g() as u32 * src_a as u32 / 255;
                    let sb = color.b() as u32 * src_a as u32 / 255;
                    let nr = sr + dr * inv_a / 255;
                    let ng = sg + dg * inv_a / 255;
                    let nb = sb + db * inv_a / 255;
                    buffer[idx] = (nr << 16) | (ng << 8) | nb;
                });
            }
        }

        if focused {
            // Caret just past the last glyph.
            let caret_x = (last_right + 1).max((surface_x + pad_x) as i32);
            let caret_y0 = (surface_y + 4.0) as i32;
            let caret_y1 = (surface_y + rect.height - 4.0) as i32;
            for y in caret_y0..caret_y1 {
                if y >= top_offset as i32 && y < vh as i32 && caret_x >= 0 && caret_x < vw as i32 {
                    let idx = (y as usize * vw as usize + caret_x as usize) as usize;
                    if let Some(p) = buffer.get_mut(idx) {
                        *p = 0x00202028;
                    }
                }
            }
        }
    } else if focused {
        // Empty focused input — show a caret at the start.
        let caret_x = (surface_x + pad_x) as i32;
        let caret_y0 = (surface_y + 4.0) as i32;
        let caret_y1 = (surface_y + rect.height - 4.0) as i32;
        for y in caret_y0..caret_y1 {
            if y >= top_offset as i32 && y < vh as i32 && caret_x >= 0 && caret_x < vw as i32 {
                let idx = (y as usize * vw as usize + caret_x as usize) as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    *p = 0x00202028;
                }
            }
        }
    }
}

// ---------------- Chrome painting ----------------

/// Paints the URL bar directly into the softbuffer's u32 surface buffer.
/// Background is a light gray strip with a darker bottom border; text uses
/// cosmic-text shaping + swash glyph rendering, alpha-blended onto the
/// surface pixel by pixel.
fn paint_devtools_panel(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    dt: &devtools::DevTools,
    page: Option<&Page>,
) {
    // Anchor the panel along the bottom third of the window.
    let panel_h = (height / 3).max(140).min(height);
    let panel_top = height.saturating_sub(panel_h);
    let bg = 0x00_1E_1E_1E;
    let prompt_bg = 0x00_2A_2A_2A;
    let border = 0x00_3A_3A_3A;
    // Background fill.
    for y in panel_top..height {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = bg;
        }
    }
    // 1px top border.
    if panel_top > 0 {
        let row = (panel_top * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = border;
        }
    }

    // Header strip with hint text.
    let header_h: u32 = 22;
    let header_color = 0x00_28_28_28;
    let header_text_color = CtColor::rgb(180, 180, 200);
    for y in panel_top..(panel_top + header_h).min(height) {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = header_color;
        }
    }
    let header_metrics = Metrics::new(12.0, 14.0);
    let mut hb = Buffer::new(font_system, header_metrics);
    hb.set_size(font_system, Some(width as f32 - 16.0), None);
    hb.set_wrap(font_system, Wrap::None);
    let tabs = [
        devtools::Panel::Console,
        devtools::Panel::Dom,
        devtools::Panel::Network,
        devtools::Panel::Picker,
    ];
    let header_text = format!(
        "{} | F12 close, Tab cycle panel",
        tabs.iter()
            .map(|p| {
                if *p == dt.panel {
                    format!("[{}]", p.label())
                } else {
                    format!(" {} ", p.label())
                }
            })
            .collect::<Vec<_>>()
            .join("")
    );
    hb.set_text(
        font_system,
        &header_text,
        Attrs::new().family(Family::SansSerif),
        Shaping::Advanced,
    );
    hb.shape_until_scroll(font_system, false);
    for run in hb.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((8.0, panel_top as f32 + run.line_y + 14.0), 1.0);
            let gx = physical.x;
            let gy = physical.y;
            let pmap_w = width as i32;
            swash_cache.with_pixels(
                font_system,
                physical.cache_key,
                header_text_color,
                |x_off, y_off, color| {
                    let px = gx + x_off;
                    let py = gy + y_off;
                    if px < 0 || py < 0 || px >= pmap_w || py as u32 >= height {
                        return;
                    }
                    let idx = py as usize * pmap_w as usize + px as usize;
                    let src_a = color.a();
                    if src_a == 0 {
                        return;
                    }
                    let inv_a = 255u32 - src_a as u32;
                    let dst = buffer[idx];
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let sr = color.r() as u32 * src_a as u32 / 255;
                    let sg = color.g() as u32 * src_a as u32 / 255;
                    let sb = color.b() as u32 * src_a as u32 / 255;
                    let nr = sr + dr * inv_a / 255;
                    let ng = sg + dg * inv_a / 255;
                    let nb = sb + db * inv_a / 255;
                    buffer[idx] = (nr << 16) | (ng << 8) | nb;
                },
            );
        }
    }

    // Body region (between header and the bottom prompt strip).
    let prompt_h: u32 = if matches!(dt.panel, devtools::Panel::Console) {
        24
    } else {
        0
    };
    let scroll_top = panel_top + header_h;
    let scroll_bottom = height.saturating_sub(prompt_h);
    if scroll_bottom <= scroll_top + 12 {
        return;
    }
    let line_h: i32 = 16;
    let metrics_body = Metrics::new(13.0, 16.0);

    // Non-console panels render their own body and then we return —
    // the prompt strip is console-only.
    match dt.panel {
        devtools::Panel::Dom => {
            paint_dom_panel(
                font_system,
                swash_cache,
                buffer,
                width,
                height,
                scroll_top,
                scroll_bottom,
                line_h,
                metrics_body,
                page,
            );
            return;
        }
        devtools::Panel::Network => {
            paint_network_panel(
                font_system,
                swash_cache,
                buffer,
                width,
                height,
                scroll_top,
                scroll_bottom,
                line_h,
                metrics_body,
                dt,
            );
            return;
        }
        devtools::Panel::Picker => {
            paint_picker_panel(
                font_system,
                swash_cache,
                buffer,
                width,
                height,
                scroll_top,
                scroll_bottom,
                line_h,
                metrics_body,
                page,
            );
            return;
        }
        devtools::Panel::Storage => {
            paint_storage_panel(
                font_system,
                swash_cache,
                buffer,
                width,
                height,
                scroll_top,
                scroll_bottom,
                line_h,
                metrics_body,
            );
            return;
        }
        devtools::Panel::Console => {}
    }

    // Console scrollback: most recent lines fill the area between
    // the header and the prompt. Each line gets a coloured tag
    // prefix.
    let lines = dt.buffer.borrow();
    let visible: Vec<&devtools::ConsoleLine> = lines.iter().rev().collect();
    let max_lines = ((scroll_bottom - scroll_top) as i32 / line_h).max(1) as usize;
    let to_show: Vec<&devtools::ConsoleLine> = visible.into_iter().take(max_lines).collect();
    // Render bottom-up so the newest line lands closest to the
    // prompt — same ordering as Chrome's console.
    let metrics = Metrics::new(13.0, 16.0);
    let mut y = scroll_bottom as i32 - line_h;
    for line in to_show {
        let (tag_color, body_color) = match line.level {
            devtools::ConsoleLevel::Error => (CtColor::rgb(255, 100, 100), CtColor::rgb(255, 200, 200)),
            devtools::ConsoleLevel::Warn => (CtColor::rgb(255, 200, 80), CtColor::rgb(240, 220, 180)),
            devtools::ConsoleLevel::Info => (CtColor::rgb(140, 200, 255), CtColor::rgb(220, 230, 240)),
            devtools::ConsoleLevel::Debug => (CtColor::rgb(180, 180, 200), CtColor::rgb(200, 200, 210)),
            devtools::ConsoleLevel::Prompt => (CtColor::rgb(140, 230, 140), CtColor::rgb(220, 240, 220)),
            devtools::ConsoleLevel::Result => (CtColor::rgb(140, 200, 255), CtColor::rgb(220, 220, 240)),
            devtools::ConsoleLevel::Log => (CtColor::rgb(180, 180, 200), CtColor::rgb(220, 220, 220)),
        };
        let prefix = match line.level {
            devtools::ConsoleLevel::Prompt => "> ",
            devtools::ConsoleLevel::Result => "<- ",
            _ => "",
        };
        let composed = format!("{prefix}{}", line.text);
        let mut lb = Buffer::new(font_system, metrics);
        lb.set_size(font_system, Some(width as f32 - 16.0), None);
        lb.set_wrap(font_system, Wrap::None);
        lb.set_text(
            font_system,
            &composed,
            Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        lb.shape_until_scroll(font_system, false);
        for run in lb.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((10.0, (y + 12) as f32), 1.0);
                let gx = physical.x;
                let gy = physical.y;
                let pmap_w = width as i32;
                let color = if prefix.is_empty() {
                    body_color
                } else if (glyph.start as usize) < prefix.len() {
                    tag_color
                } else {
                    body_color
                };
                swash_cache.with_pixels(
                    font_system,
                    physical.cache_key,
                    color,
                    |x_off, y_off, color| {
                        let px = gx + x_off;
                        let py = gy + y_off;
                        if px < 0 || py < 0 || px >= pmap_w || py as u32 >= height {
                            return;
                        }
                        if (py as u32) < scroll_top {
                            return;
                        }
                        let idx = py as usize * pmap_w as usize + px as usize;
                        let src_a = color.a();
                        if src_a == 0 {
                            return;
                        }
                        let inv_a = 255u32 - src_a as u32;
                        let dst = buffer[idx];
                        let dr = (dst >> 16) & 0xFF;
                        let dg = (dst >> 8) & 0xFF;
                        let db = dst & 0xFF;
                        let sr = color.r() as u32 * src_a as u32 / 255;
                        let sg = color.g() as u32 * src_a as u32 / 255;
                        let sb = color.b() as u32 * src_a as u32 / 255;
                        let nr = sr + dr * inv_a / 255;
                        let ng = sg + dg * inv_a / 255;
                        let nb = sb + db * inv_a / 255;
                        buffer[idx] = (nr << 16) | (ng << 8) | nb;
                    },
                );
            }
        }
        y -= line_h;
        if y < scroll_top as i32 {
            break;
        }
    }

    // Prompt strip.
    for y in scroll_bottom..height {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = prompt_bg;
        }
    }
    let prompt_text = format!("> {}_", dt.input);
    let mut pb = Buffer::new(font_system, metrics);
    pb.set_size(font_system, Some(width as f32 - 16.0), None);
    pb.set_wrap(font_system, Wrap::None);
    pb.set_text(
        font_system,
        &prompt_text,
        Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
    );
    pb.shape_until_scroll(font_system, false);
    let prompt_color = CtColor::rgb(220, 240, 220);
    for run in pb.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((10.0, scroll_bottom as f32 + 16.0), 1.0);
            let gx = physical.x;
            let gy = physical.y;
            let pmap_w = width as i32;
            swash_cache.with_pixels(
                font_system,
                physical.cache_key,
                prompt_color,
                |x_off, y_off, color| {
                    let px = gx + x_off;
                    let py = gy + y_off;
                    if px < 0 || py < 0 || px >= pmap_w || py as u32 >= height {
                        return;
                    }
                    let idx = py as usize * pmap_w as usize + px as usize;
                    let src_a = color.a();
                    if src_a == 0 {
                        return;
                    }
                    let inv_a = 255u32 - src_a as u32;
                    let dst = buffer[idx];
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let sr = color.r() as u32 * src_a as u32 / 255;
                    let sg = color.g() as u32 * src_a as u32 / 255;
                    let sb = color.b() as u32 * src_a as u32 / 255;
                    let nr = sr + dr * inv_a / 255;
                    let ng = sg + dg * inv_a / 255;
                    let nb = sb + db * inv_a / 255;
                    buffer[idx] = (nr << 16) | (ng << 8) | nb;
                },
            );
        }
    }
}

/// Render a list of `(color, text)` lines into the scrollback
/// region. Top-aligned so the structure is readable; we don't try
/// to scroll yet.
#[allow(clippy::too_many_arguments)]
fn paint_panel_lines(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    scroll_top: u32,
    scroll_bottom: u32,
    line_h: i32,
    metrics: Metrics,
    lines: &[(CtColor, String)],
) {
    let max_lines = ((scroll_bottom - scroll_top) as i32 / line_h).max(1) as usize;
    let mut y = scroll_top as i32 + 4;
    for (color, text) in lines.iter().take(max_lines) {
        let mut lb = Buffer::new(font_system, metrics);
        lb.set_size(font_system, Some(width as f32 - 16.0), None);
        lb.set_wrap(font_system, Wrap::None);
        lb.set_text(
            font_system,
            text,
            Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        lb.shape_until_scroll(font_system, false);
        for run in lb.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((10.0, (y + 12) as f32), 1.0);
                let gx = physical.x;
                let gy = physical.y;
                let pmap_w = width as i32;
                swash_cache.with_pixels(
                    font_system,
                    physical.cache_key,
                    *color,
                    |x_off, y_off, color| {
                        let px = gx + x_off;
                        let py = gy + y_off;
                        if px < 0 || py < 0 || px >= pmap_w || py as u32 >= height {
                            return;
                        }
                        if (py as u32) < scroll_top {
                            return;
                        }
                        let idx = py as usize * pmap_w as usize + px as usize;
                        let src_a = color.a();
                        if src_a == 0 {
                            return;
                        }
                        let inv_a = 255u32 - src_a as u32;
                        let dst = buffer[idx];
                        let dr = (dst >> 16) & 0xFF;
                        let dg = (dst >> 8) & 0xFF;
                        let db = dst & 0xFF;
                        let sr = color.r() as u32 * src_a as u32 / 255;
                        let sg = color.g() as u32 * src_a as u32 / 255;
                        let sb = color.b() as u32 * src_a as u32 / 255;
                        let nr = sr + dr * inv_a / 255;
                        let ng = sg + dg * inv_a / 255;
                        let nb = sb + db * inv_a / 255;
                        buffer[idx] = (nr << 16) | (ng << 8) | nb;
                    },
                );
            }
        }
        y += line_h;
        if y as u32 >= scroll_bottom {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_dom_panel(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    scroll_top: u32,
    scroll_bottom: u32,
    line_h: i32,
    metrics: Metrics,
    page: Option<&Page>,
) {
    let lines: Vec<(CtColor, String)> = match page {
        None => vec![(CtColor::rgb(180, 180, 200), "(no page loaded)".to_string())],
        Some(p) => {
            let mut out: Vec<(CtColor, String)> = Vec::new();
            dump_dom_lines(&p.dom, p.dom.document(), 0, &mut out, 200);
            out
        }
    };
    paint_panel_lines(
        font_system,
        swash_cache,
        buffer,
        width,
        height,
        scroll_top,
        scroll_bottom,
        line_h,
        metrics,
        &lines,
    );
}

/// Walk the DOM emitting one indented line per element / text /
/// comment. Caps at `max_lines` so a huge page can't blow up the
/// renderer.
fn dump_dom_lines(
    dom: &dom::Dom,
    node: dom::NodeId,
    depth: usize,
    out: &mut Vec<(CtColor, String)>,
    max_lines: usize,
) {
    if out.len() >= max_lines {
        return;
    }
    let indent = "  ".repeat(depth);
    match &dom.node(node).kind {
        dom::NodeKind::Element { tag, attrs } => {
            let mut tag_text = format!("{indent}<{tag}");
            for (k, v) in attrs.iter().take(3) {
                tag_text.push_str(&format!(" {k}=\"{}\"", truncate_chars(v, 24)));
            }
            if attrs.len() > 3 {
                tag_text.push_str(" …");
            }
            tag_text.push('>');
            out.push((CtColor::rgb(140, 200, 255), tag_text));
        }
        dom::NodeKind::Text(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.push((
                    CtColor::rgb(220, 220, 220),
                    format!("{indent}\u{201C}{}\u{201D}", truncate_chars(trimmed, 80)),
                ));
            }
        }
        dom::NodeKind::Comment(c) => {
            out.push((
                CtColor::rgb(120, 120, 130),
                format!("{indent}<!-- {} -->", truncate_chars(c, 60)),
            ));
        }
        dom::NodeKind::Doctype(d) => {
            out.push((
                CtColor::rgb(180, 180, 200),
                format!("{indent}<!DOCTYPE {d}>"),
            ));
        }
        _ => {}
    }
    for child in dom.children(node).collect::<Vec<_>>() {
        if out.len() >= max_lines {
            return;
        }
        dump_dom_lines(dom, child, depth + 1, out, max_lines);
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max_chars).collect();
    t.push('…');
    t
}

#[allow(clippy::too_many_arguments)]
fn paint_network_panel(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    scroll_top: u32,
    scroll_bottom: u32,
    line_h: i32,
    metrics: Metrics,
    dt: &devtools::DevTools,
) {
    let log = dt.network.borrow();
    let mut lines: Vec<(CtColor, String)> = Vec::new();
    if log.is_empty() {
        lines.push((
            CtColor::rgb(180, 180, 200),
            "(no requests captured)".to_string(),
        ));
    } else {
        // Most recent first.
        for entry in log.iter().rev().take(200) {
            let color = match entry.status {
                0 => CtColor::rgb(255, 100, 100),
                200..=299 => CtColor::rgb(140, 230, 140),
                300..=399 => CtColor::rgb(140, 200, 255),
                400..=499 => CtColor::rgb(255, 200, 100),
                _ => CtColor::rgb(255, 100, 100),
            };
            let size = if entry.body_size >= 1024 * 1024 {
                format!("{:.1}M", entry.body_size as f64 / 1_048_576.0)
            } else if entry.body_size >= 1024 {
                format!("{:.1}K", entry.body_size as f64 / 1024.0)
            } else {
                format!("{}B", entry.body_size)
            };
            lines.push((
                color,
                format!(
                    "{m:>4} {s:>3} {url}  ({size}, {dur}ms)",
                    m = entry.method,
                    s = entry.status,
                    url = truncate_chars(&entry.url, 80),
                    size = size,
                    dur = entry.duration_ms,
                ),
            ));
        }
    }
    paint_panel_lines(
        font_system,
        swash_cache,
        buffer,
        width,
        height,
        scroll_top,
        scroll_bottom,
        line_h,
        metrics,
        &lines,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_storage_panel(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    scroll_top: u32,
    scroll_bottom: u32,
    line_h: i32,
    metrics: Metrics,
) {
    let mut lines: Vec<(CtColor, String)> = Vec::new();
    let header = CtColor::rgb(140, 200, 255);
    let key = CtColor::rgb(220, 220, 220);
    let muted = CtColor::rgb(160, 160, 180);

    let local = js::storage::enumerate_local_storage();
    lines.push((
        header,
        format!("localStorage  ({} entries)", local.len()),
    ));
    if local.is_empty() {
        lines.push((muted, "  (empty)".to_string()));
    }
    for (k, v) in &local {
        lines.push((
            key,
            format!(
                "  {} = {}",
                truncate_chars(k, 40),
                truncate_chars(v, 80)
            ),
        ));
    }
    lines.push((header, String::new()));

    let session = js::storage::enumerate_session_storage();
    lines.push((
        header,
        format!("sessionStorage  ({} entries)", session.len()),
    ));
    if session.is_empty() {
        lines.push((muted, "  (empty)".to_string()));
    }
    for (k, v) in &session {
        lines.push((
            key,
            format!(
                "  {} = {}",
                truncate_chars(k, 40),
                truncate_chars(v, 80)
            ),
        ));
    }

    paint_panel_lines(
        font_system,
        swash_cache,
        buffer,
        width,
        height,
        scroll_top,
        scroll_bottom,
        line_h,
        metrics,
        &lines,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_picker_panel(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    scroll_top: u32,
    scroll_bottom: u32,
    line_h: i32,
    metrics: Metrics,
    page: Option<&Page>,
) {
    let mut lines: Vec<(CtColor, String)> = Vec::new();
    lines.push((
        CtColor::rgb(180, 180, 200),
        "Hover over the page to inspect the element under the cursor.".to_string(),
    ));
    if let Some(p) = page {
        if let Some(node) = p.hover {
            if let dom::NodeKind::Element { tag, attrs } = &p.dom.node(node).kind {
                lines.push((
                    CtColor::rgb(140, 230, 140),
                    format!("<{tag}>"),
                ));
                for (k, v) in attrs {
                    lines.push((
                        CtColor::rgb(220, 220, 220),
                        format!("  {k} = \"{}\"", truncate_chars(v, 80)),
                    ));
                }
                if let Some(b) = p.box_tree.get(node) {
                    lines.push((
                        CtColor::rgb(140, 200, 255),
                        format!(
                            "  box: x={:.0} y={:.0} w={:.0} h={:.0}",
                            b.rect.x, b.rect.y, b.rect.width, b.rect.height
                        ),
                    ));
                }
            }
        }
    }
    paint_panel_lines(
        font_system,
        swash_cache,
        buffer,
        width,
        height,
        scroll_top,
        scroll_bottom,
        line_h,
        metrics,
        &lines,
    );
}

fn paint_chrome(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    _height: u32,
    chrome: &Chrome,
) {
    let y_off = TAB_STRIP_HEIGHT.min(_height);
    let bar_h = (y_off + URL_BAR_HEIGHT).min(_height);
    let bg_color = if chrome.focused {
        0x00FFFFFF // white when editing
    } else {
        0x00ECEEF1 // soft gray otherwise
    };
    let border_color = 0x00C0C4CC;
    // Background — URL bar strip below the tab strip.
    for y in y_off..bar_h.saturating_sub(1) {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = bg_color;
        }
    }
    // 1px bottom border
    if bar_h > 0 {
        let row = ((bar_h - 1) * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = border_color;
        }
    }

    // Text rendering inside the bar.
    let pad_x: f32 = 10.0;
    let baseline_y: f32 = (y_off as f32) + 8.0;
    let display_text = if chrome.text.is_empty() {
        "about:blank"
    } else {
        chrome.text.as_str()
    };
    let metrics = Metrics::new(14.0, 18.0);
    let mut tb = Buffer::new(font_system, metrics);
    tb.set_size(font_system, Some(width as f32 - pad_x * 2.0), None);
    tb.set_wrap(font_system, Wrap::None);
    let attrs = Attrs::new().family(Family::SansSerif);
    tb.set_text(font_system, display_text, attrs, Shaping::Advanced);
    tb.shape_until_scroll(font_system, false);

    let text_color = CtColor::rgb(30, 30, 35);
    let mut last_glyph_right: i32 = pad_x as i32;
    for run in tb.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((pad_x, baseline_y + run.line_y), 1.0);
            let cache_key = physical.cache_key;
            let glyph_x = physical.x;
            let glyph_y = physical.y;
            last_glyph_right = glyph_x + glyph.w as i32;
            let pmap_w = width as i32;
            let pmap_h = bar_h as i32;
            swash_cache.with_pixels(font_system, cache_key, text_color, |x_off, y_off, color| {
                let px = glyph_x + x_off;
                let py = glyph_y + y_off;
                if px < 0 || py < 0 || px >= pmap_w || py >= pmap_h {
                    return;
                }
                let idx = py as usize * pmap_w as usize + px as usize;
                let src_a = color.a();
                if src_a == 0 {
                    return;
                }
                let inv_a = 255u32 - src_a as u32;
                let dst = buffer[idx];
                let dr = (dst >> 16) & 0xFF;
                let dg = (dst >> 8) & 0xFF;
                let db = dst & 0xFF;
                let sr = color.r() as u32 * src_a as u32 / 255;
                let sg = color.g() as u32 * src_a as u32 / 255;
                let sb = color.b() as u32 * src_a as u32 / 255;
                let nr = sr + dr * inv_a / 255;
                let ng = sg + dg * inv_a / 255;
                let nb = sb + db * inv_a / 255;
                buffer[idx] = (nr << 16) | (ng << 8) | nb;
            });
        }
    }

    // Caret if focused: a thin black bar just past the last glyph.
    if chrome.focused {
        let caret_x = (last_glyph_right + 2).max(pad_x as i32);
        let caret_y0 = (y_off as i32) + 6;
        let caret_y1 = (bar_h as i32).saturating_sub(7);
        if caret_x >= 0 && caret_x < width as i32 {
            for y in caret_y0..caret_y1 {
                let idx = (y * width as i32 + caret_x) as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    *p = 0x00000000;
                }
            }
        }
    }
}

/// Paint yellow translucent highlights under each find-in-page
/// match's element box. The "current" match gets a darker outline.
fn paint_find_highlights(
    buffer: &mut [u32],
    vw: u32,
    vh: u32,
    page: &Page,
    scroll_y: f32,
    top_offset: u32,
    find: &FindState,
) {
    for (i, &node) in find.matches.iter().enumerate() {
        let Some(b) = page.box_tree.get(node) else { continue };
        let highlight_color = if i == find.current { 0x00FFC107 } else { 0x00FFEB80 };
        let x0 = b.rect.x.max(0.0) as i32;
        let y0 = (b.rect.y - scroll_y + top_offset as f32).max(top_offset as f32) as i32;
        let x1 = (b.rect.x + b.rect.width).min(vw as f32) as i32;
        let y1 = (b.rect.y + b.rect.height - scroll_y + top_offset as f32)
            .min(vh as f32) as i32;
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for y in y0..y1 {
            let row = (y as usize) * vw as usize;
            for x in x0..x1 {
                let idx = row + x as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    // 50% alpha blend with the highlight color.
                    let dst = *p;
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let sr = (highlight_color >> 16) & 0xFF;
                    let sg = (highlight_color >> 8) & 0xFF;
                    let sb = highlight_color & 0xFF;
                    let nr = (sr + dr) / 2;
                    let ng = (sg + dg) / 2;
                    let nb = (sb + db) / 2;
                    *p = (nr << 16) | (ng << 8) | nb;
                }
            }
        }
    }
}

fn paint_find_bar(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    find: &FindState,
) {
    let bar_h = 30u32;
    let bar_y0 = CHROME_HEIGHT;
    let bar_color = 0x00FFFCCC;
    let border = 0x00C0C4CC;
    for y in bar_y0..(bar_y0 + bar_h) {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = bar_color;
        }
    }
    let last_row = ((bar_y0 + bar_h - 1) * width) as usize;
    for x in 0..width {
        buffer[last_row + x as usize] = border;
    }
    let label = if find.query.is_empty() {
        "Find: ".to_string()
    } else {
        format!(
            "Find: {}   {} match{}",
            find.query,
            find.matches.len(),
            if find.matches.len() == 1 { "" } else { "es" }
        )
    };
    let metrics = Metrics::new(13.0, 18.0);
    let mut tb = Buffer::new(font_system, metrics);
    tb.set_size(font_system, Some(width as f32 - 20.0), None);
    tb.set_wrap(font_system, Wrap::None);
    let attrs = Attrs::new().family(Family::SansSerif);
    tb.set_text(font_system, &label, attrs, Shaping::Advanced);
    tb.shape_until_scroll(font_system, false);
    let text_color = CtColor::rgb(35, 35, 40);
    for run in tb.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((10.0, bar_y0 as f32 + 6.0 + run.line_y), 1.0);
            let cache_key = physical.cache_key;
            let glyph_x = physical.x;
            let glyph_y = physical.y;
            let pmap_w = width as i32;
            let pmap_h_end = (bar_y0 + bar_h) as i32;
            swash_cache.with_pixels(font_system, cache_key, text_color, |x_off, y_off, color| {
                let px = glyph_x + x_off;
                let py = glyph_y + y_off;
                if px < 0 || py < bar_y0 as i32 || px >= pmap_w || py >= pmap_h_end {
                    return;
                }
                let idx = py as usize * pmap_w as usize + px as usize;
                let src_a = color.a();
                if src_a == 0 {
                    return;
                }
                let inv_a = 255u32 - src_a as u32;
                let dst = buffer[idx];
                let dr = (dst >> 16) & 0xFF;
                let dg = (dst >> 8) & 0xFF;
                let db = dst & 0xFF;
                let sr = color.r() as u32 * src_a as u32 / 255;
                let sg = color.g() as u32 * src_a as u32 / 255;
                let sb = color.b() as u32 * src_a as u32 / 255;
                let nr = sr + dr * inv_a / 255;
                let ng = sg + dg * inv_a / 255;
                let nb = sb + db * inv_a / 255;
                buffer[idx] = (nr << 16) | (ng << 8) | nb;
            });
        }
    }
}

/// Paint the tab strip at the top of the chrome. Inactive tabs from
/// `tabs`, the active tab at `active_tab`. Returns the box rects
/// (tab title rect, close-button center) for hit testing.
#[allow(clippy::too_many_arguments)]
fn paint_tab_strip(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    active_tab: usize,
    tabs: &[InactiveTab],
    active_url_bar: &str,
    active_page_url: Option<&url::Url>,
) {
    let strip_h = TAB_STRIP_HEIGHT;
    let bg_color = 0x00DDDDDD;
    let active_color = 0x00ECEEF1;
    let border_color = 0x00C0C4CC;

    // Strip background.
    for y in 0..strip_h.saturating_sub(1) {
        let row = (y * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = bg_color;
        }
    }
    // Single-pixel bottom separator between strip and URL bar.
    if strip_h > 0 {
        let row = ((strip_h - 1) * width) as usize;
        for x in 0..width {
            buffer[row + x as usize] = border_color;
        }
    }

    let total = tabs.len() + 1;
    for display_idx in 0..total {
        let tab_x = (display_idx as u32) * TAB_WIDTH;
        if tab_x + TAB_WIDTH > width {
            break;
        }
        // Title source: live state for active tab, snapshot for the rest.
        let title = if display_idx == active_tab {
            if !active_url_bar.is_empty() {
                active_url_bar.to_string()
            } else {
                active_page_url
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "New Tab".to_string())
            }
        } else {
            let pos = if display_idx < active_tab {
                display_idx
            } else {
                display_idx - 1
            };
            let tab = &tabs[pos];
            if !tab.url_bar.is_empty() {
                tab.url_bar.clone()
            } else if let Some(page) = &tab.page {
                page.url.host_str().unwrap_or("New Tab").to_string()
            } else {
                "New Tab".to_string()
            }
        };

        let is_active = display_idx == active_tab;
        let fill = if is_active { active_color } else { bg_color };
        // Fill the tab rect (one pixel inset on the right for separator).
        for y in 1..strip_h.saturating_sub(1) {
            let row = (y * width) as usize;
            for x in tab_x..(tab_x + TAB_WIDTH - 1) {
                buffer[row + x as usize] = fill;
            }
        }
        // Right-edge separator.
        for y in 0..strip_h {
            let idx = (y * width + tab_x + TAB_WIDTH - 1) as usize;
            if idx < buffer.len() {
                buffer[idx] = border_color;
            }
        }
        // Top accent line for the active tab.
        if is_active {
            for x in tab_x..(tab_x + TAB_WIDTH - 1) {
                let idx = x as usize;
                if idx < buffer.len() {
                    buffer[idx] = 0x004A90E2;
                }
            }
        }

        // Render the title.
        let metrics = Metrics::new(12.0, 16.0);
        let mut tb = Buffer::new(font_system, metrics);
        let available = (TAB_WIDTH as f32 - 24.0 - TAB_CLOSE_RADIUS * 2.0).max(20.0);
        tb.set_size(font_system, Some(available), None);
        tb.set_wrap(font_system, Wrap::None);
        let attrs = Attrs::new().family(Family::SansSerif);
        tb.set_text(font_system, &title, attrs, Shaping::Advanced);
        tb.shape_until_scroll(font_system, false);

        let text_x = tab_x as f32 + 10.0;
        let text_y = 6.0;
        let text_color = CtColor::rgb(40, 40, 50);
        for run in tb.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((text_x, text_y + run.line_y), 1.0);
                let cache_key = physical.cache_key;
                let glyph_x = physical.x;
                let glyph_y = physical.y;
                let pmap_w = width as i32;
                let pmap_h = strip_h as i32;
                let title_right = (tab_x + TAB_WIDTH - 24) as i32;
                swash_cache.with_pixels(font_system, cache_key, text_color, |x_off, y_off, color| {
                    let px = glyph_x + x_off;
                    let py = glyph_y + y_off;
                    if px < tab_x as i32 || py < 0 || px >= title_right || py >= pmap_h {
                        return;
                    }
                    let idx = py as usize * pmap_w as usize + px as usize;
                    let src_a = color.a();
                    if src_a == 0 {
                        return;
                    }
                    let inv_a = 255u32 - src_a as u32;
                    let dst = buffer[idx];
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let sr = color.r() as u32 * src_a as u32 / 255;
                    let sg = color.g() as u32 * src_a as u32 / 255;
                    let sb = color.b() as u32 * src_a as u32 / 255;
                    let nr = sr + dr * inv_a / 255;
                    let ng = sg + dg * inv_a / 255;
                    let nb = sb + db * inv_a / 255;
                    buffer[idx] = (nr << 16) | (ng << 8) | nb;
                });
            }
        }

        // Close button (×) — a 14×14 region near the right edge.
        let close_cx = (tab_x + TAB_WIDTH - 14) as i32;
        let close_cy = (strip_h / 2) as i32;
        let r = TAB_CLOSE_RADIUS as i32;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy > r * r {
                    continue;
                }
                let px = close_cx + dx;
                let py = close_cy + dy;
                if px < 0 || py < 0 || px >= width as i32 || py >= strip_h as i32 {
                    continue;
                }
                let idx = py as usize * width as usize + px as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    *p = 0x00B0B4BC;
                }
            }
        }
        // Render an `×` as two thin diagonal lines in the close circle.
        for off in -3..=3i32 {
            for &(dx, dy) in &[(off, off), (off, -off)] {
                let px = close_cx + dx;
                let py = close_cy + dy;
                if px < 0 || py < 0 || px >= width as i32 || py >= strip_h as i32 {
                    continue;
                }
                let idx = py as usize * width as usize + px as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    *p = 0x00404040;
                }
            }
        }
    }

    // `+` New tab button to the right of the last tab.
    let plus_x = (total as u32) * TAB_WIDTH;
    if plus_x + NEW_TAB_BUTTON_WIDTH <= width {
        for y in 1..strip_h.saturating_sub(1) {
            let row = (y * width) as usize;
            for x in plus_x..(plus_x + NEW_TAB_BUTTON_WIDTH) {
                buffer[row + x as usize] = bg_color;
            }
        }
        // Render a `+` glyph
        let cx = (plus_x + NEW_TAB_BUTTON_WIDTH / 2) as i32;
        let cy = (strip_h / 2) as i32;
        for off in -5..=5i32 {
            for &(dx, dy) in &[(off, 0_i32), (0, off)] {
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || py < 0 || px >= width as i32 || py >= strip_h as i32 {
                    continue;
                }
                let idx = py as usize * width as usize + px as usize;
                if let Some(p) = buffer.get_mut(idx) {
                    *p = 0x00404040;
                }
            }
        }
    }
}

impl ApplicationHandler for Browser {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("DaBoss")
            .with_inner_size(LogicalSize::new(1024.0, 768.0));
        let window = std::sync::Arc::new(event_loop.create_window(attrs).expect("create window"));
        let surface = gpu::GpuPresenter::new(window.clone())
            .expect("could not init GPU presenter");
        let size = window.inner_size();
        self.viewport_size = (size.width.max(1), size.height.max(1));
        self.window = Some(window);
        self.surface = Some(surface);
        // Pipe console.* output from page scripts into the
        // devtools scrollback for the lifetime of the process.
        self.install_devtools_console_capture();

        if let Some(url) = self.pending_url.take() {
            self.navigate(&url, true);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.viewport_size = (size.width.max(1), size.height.max(1));
                // Re-run layout + paint on the existing page (no refetch).
                self.re_layout();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                self.update_hover();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = self.cursor;
                self.click_at(x, y);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 32.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                self.scroll_y = (self.scroll_y - dy).max(0.0);
                if let Some(page) = &self.page {
                    let max_scroll = (page.pixmap.height() as f32
                        - self.page_viewport_height() as f32)
                        .max(0.0);
                    self.scroll_y = self.scroll_y.min(max_scroll);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state: ElementState::Pressed,
                        logical_key,
                        text,
                        ..
                    },
                ..
            } => self.handle_key(logical_key, text),
            WindowEvent::Ime(ime) => self.handle_ime(ime),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    /// Called whenever the event loop is about to wait. We use it to fire
    /// any JS timers that have come due, and to set winit's control flow
    /// so we wake up at the next pending timer's fire time (or wait
    /// indefinitely if none are queued).
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.pump_js_timers();
        self.pump_js_animation_frames();
        // Drain any pending `element.scrollIntoView()` requests
        // from page scripts and clamp into the scrollable range.
        let scroll_target =
            js::engine::JS_SCROLL_TO_DOC_Y.with(|s| s.borrow_mut().take());
        if let Some(y) = scroll_target {
            if let Some(page) = &self.page {
                let max_scroll = (page.pixmap.height() as f32
                    - self.page_viewport_height() as f32)
                    .max(0.0);
                self.scroll_y = y.clamp(0.0, max_scroll);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
        let next = self
            .page
            .as_ref()
            .and_then(|p| p.js.next_timer_at());
        let has_raf = self
            .page
            .as_ref()
            .map(|p| p.js.has_pending_animation_frames())
            .unwrap_or(false);
        if has_raf {
            // rAF callbacks should run on the next paint tick. Schedule
            // a poll instead of indefinitely waiting.
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
        } else {
            match next {
                Some(at) => {
                    event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(at))
                }
                None => event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait),
            }
        }
    }
}

impl Browser {
    /// If `hit_node` is (or is inside) an `<iframe>`, hit-test the click
    /// against the iframe's own box tree and re-render it in place when
    /// the click resolves to a link. Returns true if the iframe consumed
    /// the click.
    fn try_handle_iframe_click(&mut self, hit_node: dom::NodeId, x: f32, page_y: f32) -> bool {
        let iframe_node = {
            let Some(page) = &self.page else {
                return false;
            };
            ancestor_with_tag(&page.dom, hit_node, "iframe")
        };
        let Some(iframe_node) = iframe_node else {
            return false;
        };

        // Compute (local_x, local_y) relative to the iframe's content
        // box, and look up the iframe's own DOM/box_tree.
        let (local_x, local_y, dest_url, sandbox) = {
            let Some(page) = &self.page else {
                return false;
            };
            let Some(b) = page.box_tree.get(iframe_node) else {
                return false;
            };
            // page_y already includes the chrome offset removed; scroll_y
            // is the parent's scroll. The iframe's pixmap is composited
            // at (b.rect.x, b.rect.y) in parent coords, so the click's
            // iframe-local coords are:
            let abs_y = page_y + self.scroll_y;
            let local_x = x - b.rect.x;
            let local_y = abs_y - b.rect.y;
            if local_x < 0.0 || local_y < 0.0 {
                return false;
            }

            let Some(iframe) = page.iframes.get(&iframe_node) else {
                return false;
            };
            if local_x > b.rect.width || local_y > b.rect.height {
                return false;
            }

            // Hit-test inside the iframe's own layout.
            let Some(inner_hit) =
                layout::hit_test(&iframe.dom, &iframe.box_tree, local_x, local_y)
            else {
                return false;
            };

            // Walk up looking for <a href>.
            let mut cur = Some(inner_hit);
            let mut found: Option<url::Url> = None;
            while let Some(n) = cur {
                if let dom::NodeKind::Element { tag, attrs } = &iframe.dom.node(n).kind {
                    if tag == "a" {
                        if let Some((_, h)) = attrs.iter().find(|(k, _)| k == "href") {
                            if let Ok(abs) = iframe.url.join(h) {
                                found = Some(abs);
                                break;
                            }
                        }
                    }
                }
                cur = iframe.dom.node(n).parent;
            }
            (local_x, local_y, found, iframe.sandbox)
        };
        let _ = (local_x, local_y); // computed for clarity; not used further

        let Some(dest) = dest_url else {
            return false;
        };
        if sandbox {
            eprintln!("[iframe] click in sandboxed iframe blocked: {dest}");
            return true;
        }

        // Re-render this iframe at its current box size with the new URL.
        let (width, height, base) = {
            let Some(page) = &self.page else {
                return true;
            };
            let Some(b) = page.box_tree.get(iframe_node) else {
                return true;
            };
            (
                b.rect.width.max(1.0) as u32,
                b.rect.height.max(1.0) as u32,
                page.url.clone(),
            )
        };
        let dest_str = dest.to_string();
        eprintln!("[iframe] navigating iframe to {dest_str}");
        if let Some(new_content) =
            load_iframe_document(&self.client, &base, &dest_str, width, height, false)
        {
            if let Some(page) = self.page.as_mut() {
                page.iframes.insert(iframe_node, new_content);
            }
            // Recomposite the parent pixmap so the iframe area updates.
            self.recomposite_iframes();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        true
    }

    /// Recomposite all iframes onto the parent pixmap. Called after an
    /// iframe is re-rendered so we don't need to redo the parent paint.
    fn recomposite_iframes(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        composite_iframes(&mut page.pixmap, &page.box_tree, &page.iframes);
    }

    fn pump_js_timers(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let r = page.js.pump_timers(&mut page.dom);
        if r.mutated {
            self.recascade_and_paint();
        }
    }

    /// Walk the box tree and push each element's rect into the JS
    /// engine so `getBoundingClientRect` can return current values
    /// without going through the box tree directly.
    fn refresh_js_bounding_rects(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let entries = page.box_tree.boxes.iter().enumerate().filter_map(|(i, b)| {
            let b = b.as_ref()?;
            let id = dom::NodeId::from_raw(i as u32);
            Some((id, [b.rect.x, b.rect.y, b.rect.width, b.rect.height]))
        });
        page.js.refresh_bounding_rects(entries);
    }

    /// Walk the page DOM for `<audio>` elements, fetch + decode each
    /// `src` (WAV only for now), and stash an `AudioElement` keyed by
    /// the audio element's NodeId. Honours `autoplay`.
    fn prefetch_audio_elements(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let mut to_prefetch: Vec<(dom::NodeId, String, bool, bool, f32)> = Vec::new();
        collect_audio_sources(&page.dom, page.dom.document(), &mut to_prefetch);
        let base = page.url.clone();
        let elements = page.audio.clone();
        for (id, src, autoplay, loop_, volume) in to_prefetch {
            let Ok(abs) = base.join(&src) else { continue };
            let url_str = abs.to_string();
            let ctx = net::RequestContext::new().with_initiator(base.clone());
            let Ok(resp) = self.client.get_with(&url_str, ctx) else { continue };
            if !(200..300).contains(&resp.status) {
                continue;
            }
            let Some(wav) = audio::decode_any(&resp.body) else {
                eprintln!("[audio] unrecognised audio format at {url_str}");
                continue;
            };
            let Some(element) = audio::AudioElement::from_wav(wav) else {
                eprintln!("[audio] could not open output stream for {url_str}");
                continue;
            };
            element.set_volume(volume);
            element.set_loop(loop_);
            if autoplay {
                element.play();
            }
            elements.borrow_mut().insert(id, element);
        }
    }

    /// Walk for `<video>` elements, fetch their `src`, hand the bytes
    /// to ffmpeg via [`video::VideoElement::from_bytes`]. Stored on
    /// the JsEngine's video_elements map so paint can pull frames.
    fn prefetch_video_elements(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let mut to_prefetch: Vec<(dom::NodeId, String, bool, bool)> = Vec::new();
        collect_video_sources(&page.dom, page.dom.document(), &mut to_prefetch);
        let base = page.url.clone();
        let elements = page.video.clone();
        for (id, src, autoplay, loop_) in to_prefetch {
            let Ok(abs) = base.join(&src) else { continue };
            let url_str = abs.to_string();
            let ctx = net::RequestContext::new().with_initiator(base.clone());
            let Ok(resp) = self.client.get_with(&url_str, ctx) else { continue };
            if !(200..300).contains(&resp.status) {
                continue;
            }
            // Big videos likely spilled to disk — hand the path
            // directly to ffmpeg instead of re-copying through RAM.
            let element_opt = if let Some(path) = resp.body_path.clone() {
                video::VideoElement::from_path(path, autoplay, loop_)
            } else {
                video::VideoElement::from_bytes(resp.body.clone(), autoplay, loop_)
            };
            let Some(element) = element_opt
            else {
                eprintln!("[video] could not start decode for {url_str}");
                continue;
            };
            elements.borrow_mut().insert(id, element);
        }
    }

    /// Build the `getComputedStyle` snapshot for the currently styled
    /// page and push it into the JS engine.
    fn refresh_js_computed_styles(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let snapshots = computed_style_snapshots(&page.dom, &page.styles);
        page.js.refresh_computed_styles(snapshots);
    }

    fn pump_js_animation_frames(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let r = page.js.pump_animation_frames(&mut page.dom);
        if r.mutated {
            self.recascade_and_paint();
        }
        // Advance CSS-side animations. If anything is still running
        // we want the next frame to keep ticking.
        let has_active = self.tick_css_animations();
        if has_active {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        self.process_js_nav_requests();
    }

    /// Advance running CSS transitions / animations by reading the
    /// current wall-clock time, writing the interpolated value back
    /// into the live computed style for each animating element. Drops
    /// completed animations. Returns `true` if any animations are
    /// still in flight.
    fn tick_css_animations(&mut self) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        let now = std::time::Instant::now();
        let mut still_active = false;
        page.anims.retain(|anim| {
            let elapsed = now.duration_since(anim.start);
            let raw = if anim.duration.as_secs_f32() <= 0.0 {
                1.0
            } else {
                (elapsed.as_secs_f32() / anim.duration.as_secs_f32()).clamp(0.0, 1.0)
            };
            let progress = ease(raw, anim.timing);
            let value = anim.from + (anim.to - anim.from) * progress;
            // Write back to the live style.
            let style = page.styles.get_mut(anim.node);
            if anim.property == "opacity" {
                style.opacity = value.clamp(0.0, 1.0);
            }
            if raw < 1.0 {
                still_active = true;
                true
            } else {
                false
            }
        });
        still_active
    }

    /// After each cascade, compare opacities and start transitions
    /// for elements that have `transition` on opacity (or `all`).
    fn start_css_transitions(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let now = std::time::Instant::now();
        // Walk every element in the DOM with a computed style.
        let mut new_opacity_map: std::collections::HashMap<dom::NodeId, f32> =
            std::collections::HashMap::new();
        let doc = page.dom.document();
        let mut stack: Vec<dom::NodeId> = vec![doc];
        while let Some(n) = stack.pop() {
            if matches!(page.dom.node(n).kind, dom::NodeKind::Element { .. }) {
                let s = page.styles.get(n);
                let new_opacity = s.opacity;
                let was = page.prev_opacity.get(&n).copied();
                let transitions_opacity = s
                    .transitions
                    .iter()
                    .any(|t| t.property == "opacity" || t.property == "all");
                if let Some(prev) = was {
                    if (prev - new_opacity).abs() > f32::EPSILON && transitions_opacity {
                        if let Some(rule) = s
                            .transitions
                            .iter()
                            .find(|t| t.property == "opacity" || t.property == "all")
                        {
                            let dur = std::time::Duration::from_secs_f32(rule.duration_s);
                            // Push a running animation FROM prev TO new.
                            page.anims.push(RunningAnim {
                                node: n,
                                property: "opacity".into(),
                                from: prev,
                                to: new_opacity,
                                start: now
                                    + std::time::Duration::from_secs_f32(rule.delay_s),
                                duration: dur,
                                timing: rule.timing,
                            });
                            // The first frame of the animation should
                            // show `prev`, not `new` — write back.
                            page.styles.get_mut(n).opacity = prev;
                        }
                    }
                }
                new_opacity_map.insert(n, new_opacity);
            }
            for c in page.dom.children(n).collect::<Vec<_>>() {
                stack.push(c);
            }
        }
        page.prev_opacity = new_opacity_map;
    }

    /// Drain navigation requests scripts have queued (location.assign,
    /// history.back, etc.) and act on them.
    fn process_js_nav_requests(&mut self) {
        let requests = match self.page.as_ref() {
            Some(p) => p.js.drain_nav_requests(),
            None => return,
        };
        for req in requests {
            match req {
                js::engine::NavRequest::Assign(url) | js::engine::NavRequest::Replace(url) => {
                    self.navigate(&url, true);
                }
                js::engine::NavRequest::Reload => self.reload(),
                js::engine::NavRequest::Go(_) => {
                    // Browser back/forward is best modelled against our
                    // own `history` Vec. For the toy, defer to the
                    // built-in helpers and let JS-side bookkeeping
                    // catch up on the next navigate.
                    // n < 0 = back, n > 0 = forward
                    self.history_back();
                }
            }
        }
    }
}

// ---------------- Subresource Integrity helper ----------------

/// Centralised SRI gate for external resources. Returns `true` if the
/// resource is allowed to load: either no integrity metadata was
/// supplied, or the body matched one of the listed hashes.
fn stylesheet_integrity_ok(href: &str, integrity: Option<&str>, body: &[u8]) -> bool {
    let Some(spec) = integrity else {
        return true;
    };
    let verdict = net::verify_integrity(spec, body);
    if !verdict.allows_load() {
        tracing::warn!(
            href = %href,
            integrity = %spec,
            "SRI check failed; blocking stylesheet",
        );
    }
    verdict.allows_load()
}

// ---------------- <link rel=preload|prefetch> warming ----------------

/// Cap on background URL warming per page. Lighthouse warns at >5
/// preloads; 24 leaves headroom for chatty sites without letting a
/// malicious page burn through arbitrary network/disk budget.
const MAX_LINK_PRELOADS: usize = 24;

/// Walk the DOM for `<link rel="preload">` and `<link rel="prefetch">`
/// elements and best-effort fetch each `href`. The HTTP cache layer
/// stores the responses, so a later request — JS `fetch()`, a future
/// navigation, an XHR — hits the cache instead of the network.
///
/// We don't try to use the result here; the win is purely warming
/// `HttpCache`. Failures are silently ignored.
fn prefetch_link_resources(dom: &dom::Dom, client: &net::Client, base_url: &url::Url) -> usize {
    let mut hrefs: Vec<(String, Option<String>)> = Vec::new();
    collect_link_preload_hrefs(dom, dom.document(), &mut hrefs);
    let mut warmed = 0usize;
    for (href, as_attr) in hrefs.into_iter().take(MAX_LINK_PRELOADS) {
        if href.is_empty() {
            continue;
        }
        let Ok(abs) = base_url.join(&href) else {
            continue;
        };
        // Skip non-http(s) schemes — data:, blob:, about: have no
        // network round-trip to warm.
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        match client.get(abs.as_str()) {
            Ok(_) => {
                warmed += 1;
                tracing::debug!(
                    href = %abs,
                    as_attr = as_attr.as_deref().unwrap_or(""),
                    "preload: warmed cache",
                );
            }
            Err(e) => {
                tracing::debug!(href = %abs, error = %e, "preload: fetch failed");
            }
        }
    }
    warmed
}

fn collect_link_preload_hrefs(
    dom: &dom::Dom,
    node: dom::NodeId,
    out: &mut Vec<(String, Option<String>)>,
) {
    if out.len() >= MAX_LINK_PRELOADS {
        return;
    }
    if let dom::NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "link" {
            let rel = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("rel"))
                .map(|(_, v)| v.to_ascii_lowercase());
            let is_warming_hint = rel
                .as_deref()
                .map(|r| {
                    r.split_ascii_whitespace()
                        .any(|tok| tok == "preload" || tok == "prefetch")
                })
                .unwrap_or(false);
            if is_warming_hint {
                let href = attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("href"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let as_attr = attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("as"))
                    .map(|(_, v)| v.clone());
                if !href.is_empty() {
                    out.push((href, as_attr));
                }
            }
        }
    }
    let kids: Vec<dom::NodeId> = dom.children(node).collect();
    for c in kids {
        collect_link_preload_hrefs(dom, c, out);
    }
}

// ---------------- Image prefetching (shared between png + browser) ----------------

fn prefetch_images(
    dom: &dom::Dom,
    client: &net::Client,
    base_url: &url::Url,
    cache: &mut layout::ImageCache,
) -> usize {
    let mut count = 0usize;
    walk_images(dom, dom.document(), client, base_url, cache, &mut count);
    cache.len()
}

fn walk_images(
    dom_ref: &dom::Dom,
    node: dom::NodeId,
    client: &net::Client,
    base_url: &url::Url,
    cache: &mut layout::ImageCache,
    count: &mut usize,
) {
    if *count >= MAX_IMAGES {
        return;
    }
    if let dom::NodeKind::Element { tag, attrs } = &dom_ref.node(node).kind {
        if tag == "img" {
            let chosen = pick_img_url(dom_ref, node, attrs);
            if let Some(src) = chosen {
                if !src.is_empty() {
                    *count += 1;
                    if let Some(info) = fetch_and_decode(client, base_url, &src) {
                        cache.insert((node, layout::ImageSlot::Img), info);
                    }
                }
            }
        }
    }
    let kids: Vec<dom::NodeId> = dom_ref.children(node).collect();
    for c in kids {
        walk_images(dom_ref, c, client, base_url, cache, count);
    }
}

/// Pick the best source for an `<img>`. Honours `srcset` (1x/2x/Nx
/// descriptors and `w`-descriptors), `<picture>` parent `<source>`
/// elements, and falls back to `src=`. The toy assumes a 1x device
/// pixel ratio; future tablets / Retina support would feed a real DPR
/// in here.
fn pick_img_url(
    dom: &dom::Dom,
    node: dom::NodeId,
    attrs: &[(String, String)],
) -> Option<String> {
    // `<picture>` semantics: if the parent is a <picture>, walk its
    // <source> children before this img and use the first one with a
    // matching srcset. We don't parse `media`/`type` filters yet.
    if let Some(parent) = dom.node(node).parent {
        if let dom::NodeKind::Element { tag, .. } = &dom.node(parent).kind {
            if tag == "picture" {
                for sib in dom.children(parent) {
                    if sib == node {
                        break;
                    }
                    if let dom::NodeKind::Element { tag: t, attrs: a } = &dom.node(sib).kind {
                        if t == "source" {
                            if let Some(set) =
                                a.iter().find(|(k, _)| k == "srcset").map(|(_, v)| v.as_str())
                            {
                                if let Some(pick) = pick_srcset_url(set) {
                                    return Some(pick);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Then the img's own srcset, then bare src.
    if let Some((_, set)) = attrs.iter().find(|(k, _)| k == "srcset") {
        if let Some(pick) = pick_srcset_url(set) {
            return Some(pick);
        }
    }
    attrs.iter().find(|(k, _)| k == "src").map(|(_, v)| v.clone())
}

/// Parse a `srcset` and pick the entry closest to 1× DPR. Each
/// candidate is `URL <descriptor>` where the descriptor is either
/// `<n>x` (density) or `<n>w` (width hint). For width hints we don't
/// know the rendered width, so we pick the smallest entry — keeps
/// bandwidth low for the toy.
fn pick_srcset_url(srcset: &str) -> Option<String> {
    let mut candidates: Vec<(String, f32)> = Vec::new();
    for entry in srcset.split(',') {
        let parts: Vec<&str> = entry.trim().split_ascii_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        let url = parts[0].to_string();
        let density = match parts.get(1) {
            None => 1.0,
            Some(d) => {
                let d = d.trim();
                if let Some(num) = d.strip_suffix('x').and_then(|s| s.parse::<f32>().ok()) {
                    num
                } else if let Some(num) = d.strip_suffix('w').and_then(|s| s.parse::<f32>().ok()) {
                    // Approximate width descriptors as density via
                    // width / 1024 (our default viewport). Smaller =
                    // closer to 1×.
                    (num / 1024.0).max(0.1)
                } else {
                    1.0
                }
            }
        };
        if !url.is_empty() {
            candidates.push((url, density));
        }
    }
    // Pick the entry with density closest to 1.0 (so 1x preferred over
    // 2x or 3x on a non-Retina display).
    candidates
        .into_iter()
        .min_by(|a, b| {
            (a.1 - 1.0)
                .abs()
                .partial_cmp(&(b.1 - 1.0).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(u, _)| u)
}

fn fetch_and_decode(
    client: &net::Client,
    base_url: &url::Url,
    src: &str,
) -> Option<layout::ImageInfo> {
    let abs = base_url.join(src).ok()?;
    let url = abs.to_string();
    match client.get(&url) {
        Ok(resp) if (200..300).contains(&resp.status) => layout::decode_image(&resp.body),
        _ => None,
    }
}

fn prefetch_background_images(
    dom_ref: &dom::Dom,
    styles: &css::StyleTree,
    client: &net::Client,
    base_url: &url::Url,
    cache: &mut layout::ImageCache,
) -> usize {
    let mut count = cache.len();
    walk_bg_images(
        dom_ref,
        styles,
        dom_ref.document(),
        client,
        base_url,
        cache,
        &mut count,
    );
    cache
        .keys()
        .filter(|(_, s)| *s == layout::ImageSlot::Background)
        .count()
}

fn walk_bg_images(
    dom_ref: &dom::Dom,
    styles: &css::StyleTree,
    node: dom::NodeId,
    client: &net::Client,
    base_url: &url::Url,
    cache: &mut layout::ImageCache,
    count: &mut usize,
) {
    if *count >= MAX_IMAGES {
        return;
    }
    if let dom::NodeKind::Element { .. } = &dom_ref.node(node).kind {
        let style = styles.get(node);
        if let Some(css::BackgroundImage::Url(src)) = &style.background_image {
            if !src.is_empty() {
                *count += 1;
                if let Some(info) = fetch_and_decode(client, base_url, src) {
                    cache.insert((node, layout::ImageSlot::Background), info);
                }
            }
        }
        if let Some(css::BackgroundImage::Url(src)) = &style.mask_image {
            if !src.is_empty() {
                *count += 1;
                if let Some(info) = fetch_and_decode(client, base_url, src) {
                    cache.insert((node, layout::ImageSlot::Mask), info);
                }
            }
        }
    }
    let kids: Vec<dom::NodeId> = dom_ref.children(node).collect();
    for c in kids {
        walk_bg_images(dom_ref, styles, c, client, base_url, cache, count);
    }
}
