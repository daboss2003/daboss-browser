#![forbid(unsafe_code)]

mod css;
mod dom;
mod html;
mod js;
mod layout;
mod net;
mod paint;

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::rc::Rc;

use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};
use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const MAX_EXTERNAL_STYLESHEETS: usize = 30;
const MAX_IMAGES: usize = 50;
const MAX_IFRAMES: usize = 5;
const PAINT_HEIGHT_CEILING: u32 = 65_535;
/// Height (px) of the browser chrome strip at the top of the window —
/// holds the URL bar.
const CHROME_HEIGHT: u32 = 36;

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
    let response = client.get(url_str)?;
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
            css::StylesheetRef::External(href) => {
                if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                    continue;
                }
                ext_count += 1;
                if let Ok(abs) = base_url.join(&href) {
                    if let Ok(r) = client.get(&abs.to_string()) {
                        if (200..300).contains(&r.status) {
                            sheets.push(css::parse(&String::from_utf8_lossy(&r.body)));
                        }
                    }
                }
            }
        }
    }
    let style_tree = css::style_dom(&dom, &sheets);

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
    /// Rendered iframe contents, keyed by the iframe's NodeId in this page.
    iframes: std::collections::HashMap<dom::NodeId, IframeContent>,
    /// Page-scoped JS context. Owns the long-lived `boa::Context` plus the
    /// addEventListener registry, so click handlers registered by inline
    /// scripts can fire on subsequent user input.
    js: js::JsEngine,
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

struct Browser {
    /// URL to load on first frame.
    pending_url: Option<String>,

    client: Rc<net::Client>,

    /// Per-origin `localStorage` map. Each navigated [`js::JsEngine`]
    /// gets the `StorageArea` keyed by its page's origin (scheme + host
    /// + port). No on-disk persistence yet.
    local_storage: Rc<std::cell::RefCell<std::collections::HashMap<String, js::StorageArea>>>,

    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
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
        }
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

        let response = match self.client.get(url_str) {
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
        // inline `<script>` content.
        let csp = response
            .header("Content-Security-Policy")
            .map(net::Csp::parse)
            .unwrap_or_default();
        let inline_scripts_allowed = csp.allows_inline_scripts();
        if !inline_scripts_allowed {
            eprintln!("[csp] inline scripts blocked by Content-Security-Policy");
        }

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
                css::StylesheetRef::External(href) => {
                    if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                        continue;
                    }
                    ext_count += 1;
                    if let Ok(abs) = parsed.join(&href) {
                        if let Ok(resp) = self.client.get(&abs.to_string()) {
                            if (200..300).contains(&resp.status) {
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

        let mut pixmap = match paint::paint(
            &dom,
            &style_tree,
            &box_tree,
            &images,
            self.viewport_size.0,
            paint_h,
        ) {
            Some(p) => p,
            None => {
                eprintln!("[paint] could not allocate pixmap");
                return;
            }
        };
        composite_iframes(&mut pixmap, &box_tree, &iframes);

        if record_history {
            if let Some(prev) = &self.page {
                self.history.push(prev.url.clone());
                self.history_cursor = None;
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
            iframes,
            js: js_engine,
        });
        self.scroll_y = 0.0;

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

    fn reload(&mut self) {
        if let Some(url) = self.page.as_ref().map(|p| p.url.to_string()) {
            self.navigate(&url, false);
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

        if let Some(mut pixmap) = paint::paint(
            &page.dom,
            &page.styles,
            &box_tree,
            &page.images,
            page_w,
            paint_h,
        ) {
            composite_iframes(&mut pixmap, &box_tree, &new_iframes);
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
            let mut init = js::engine::EventInit::bubbling();
            init.client_x = Some(x);
            init.client_y = Some(page_y);
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
        if let Some(p) = self.page.as_mut() {
            p.focus = node;
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
        let max_bottom = page.pixmap.height();
        if let Some(mut pixmap) = paint::paint(
            &page.dom,
            &page.styles,
            &page.box_tree,
            &page.images,
            page_w,
            max_bottom,
        ) {
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
        surface.resize(w, h).expect("surface resize");
        let mut buffer = surface.buffer_mut().expect("buffer");

        let vw = w.get() as usize;
        let vh = h.get() as usize;

        // White page fill.
        for px in buffer.iter_mut() {
            *px = 0x00FF_FFFF;
        }

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
            paint_sticky_overlays(&mut buffer, vw as u32, vh as u32, page, self.scroll_y);
        }

        // Input overlays — typed values painted on top of the page pixmap.
        if let Some(page) = &self.page {
            paint_input_overlays(
                &mut self.chrome_font_system,
                &mut self.chrome_swash,
                &mut buffer,
                vw as u32,
                vh as u32,
                page,
                self.scroll_y,
                CHROME_HEIGHT,
            );
        }

        // Chrome strip on top.
        paint_chrome(
            &mut self.chrome_font_system,
            &mut self.chrome_swash,
            &mut buffer,
            vw as u32,
            vh as u32,
            &self.chrome,
        );

        buffer.present().expect("present");
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

/// First element child of `parent` (skipping text/comments). Used by
/// lifecycle events to find the document's <html> root.
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
            css::StylesheetRef::External(href) => {
                if ext_count >= MAX_EXTERNAL_STYLESHEETS {
                    continue;
                }
                ext_count += 1;
                if let Ok(child_abs) = abs.join(&href) {
                    if let Ok(resp) = client.get(&child_abs.to_string()) {
                        if (200..300).contains(&resp.status) {
                            sheets.push(css::parse(&String::from_utf8_lossy(&resp.body)));
                        }
                    }
                }
            }
        }
    }
    let style_tree = css::style_dom(&dom, &sheets);

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
            let is_focused = page.focus == Some(node);
            if let Some(b) = page.box_tree.get(node) {
                draw_input_text(
                    font_system,
                    swash_cache,
                    buffer,
                    vw,
                    vh,
                    b.rect,
                    value,
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
fn paint_chrome(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    buffer: &mut [u32],
    width: u32,
    _height: u32,
    chrome: &Chrome,
) {
    let bar_h = CHROME_HEIGHT.min(_height);
    let bg_color = if chrome.focused {
        0x00FFFFFF // white when editing
    } else {
        0x00ECEEF1 // soft gray otherwise
    };
    let border_color = 0x00C0C4CC;
    // Background
    for y in 0..bar_h.saturating_sub(1) {
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
    let baseline_y: f32 = 8.0;
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
        let caret_y0 = 6;
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

impl ApplicationHandler for Browser {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("DaBoss")
            .with_inner_size(LogicalSize::new(1024.0, 768.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("create window"));
        let context = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("softbuffer surface");
        let size = window.inner_size();
        self.viewport_size = (size.width.max(1), size.height.max(1));
        self.window = Some(window);
        self.surface = Some(surface);

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

    fn pump_js_animation_frames(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let r = page.js.pump_animation_frames(&mut page.dom);
        if r.mutated {
            self.recascade_and_paint();
        }
        self.process_js_nav_requests();
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
            if let Some((_, src)) = attrs.iter().find(|(k, _)| k == "src") {
                if !src.is_empty() {
                    *count += 1;
                    if let Some(info) = fetch_and_decode(client, base_url, src) {
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
    }
    let kids: Vec<dom::NodeId> = dom_ref.children(node).collect();
    for c in kids {
        walk_bg_images(dom_ref, styles, c, client, base_url, cache, count);
    }
}
