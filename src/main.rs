#![forbid(unsafe_code)]

mod css;
mod dom;
mod html;
mod layout;
mod net;
mod paint;

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::rc::Rc;

use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const MAX_EXTERNAL_STYLESHEETS: usize = 30;
const MAX_IMAGES: usize = 50;
const PAINT_HEIGHT_CEILING: u32 = 65_535;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
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
    let dom = html::parse(&body);

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

    if let Some(pixmap) = paint::paint(
        &dom,
        &style_tree,
        &box_tree,
        &images,
        viewport.width as u32,
        paint_height,
    ) {
        if let Ok(png) = pixmap.encode_png() {
            let path = "/tmp/daboss-out.png";
            let _ = std::fs::write(path, png);
            eprintln!("[png] wrote {path} ({}x{paint_height})", viewport.width as u32);
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
    #[allow(dead_code)] // consumed by :hover / :focus re-cascade in phase 6b
    styles: css::StyleTree,
    box_tree: layout::BoxTree,
    /// Full-page rendered pixmap.
    pixmap: Pixmap,
}

struct Browser {
    /// URL to load on first frame.
    pending_url: Option<String>,

    client: net::Client,

    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    viewport_size: (u32, u32),
    scroll_y: f32,
    cursor: (f32, f32),

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
            pending_url: initial_url,
            client: net::Client::new().with_allow_loopback(allow_loopback),
            window: None,
            surface: None,
            viewport_size: (1024, 768),
            scroll_y: 0.0,
            cursor: (0.0, 0.0),
            page: None,
            history: Vec::new(),
            history_cursor: None,
        }
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
        let dom = html::parse(&body);

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

        let style_tree = css::style_dom(&dom, &sheets);

        let mut images = layout::ImageCache::new();
        prefetch_images(&dom, &self.client, &parsed, &mut images);
        prefetch_background_images(&dom, &style_tree, &self.client, &parsed, &mut images);

        let viewport = layout::Rect {
            x: 0.0,
            y: 0.0,
            width: self.viewport_size.0 as f32,
            height: self.viewport_size.1 as f32,
        };
        let box_tree = layout::layout(&dom, &style_tree, &images, viewport);

        let mut max_bottom = self.viewport_size.1;
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

        let pixmap = match paint::paint(
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

        if record_history {
            if let Some(prev) = &self.page {
                self.history.push(prev.url.clone());
                self.history_cursor = None;
            }
        }

        self.page = Some(Page {
            url: parsed,
            dom,
            styles: style_tree,
            box_tree,
            pixmap,
        });
        self.scroll_y = 0.0;

        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn reload(&mut self) {
        if let Some(url) = self.page.as_ref().map(|p| p.url.to_string()) {
            self.navigate(&url, false);
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
        let target = {
            let Some(page) = &self.page else {
                return;
            };
            let abs_y = y + self.scroll_y;
            let hit = match layout::hit_test(&page.dom, &page.box_tree, x, abs_y) {
                Some(n) => n,
                None => return,
            };
            // Walk up to find the nearest <a href>.
            let mut current = Some(hit);
            let mut href: Option<String> = None;
            while let Some(n) = current {
                if let dom::NodeKind::Element { tag, attrs } = &page.dom.node(n).kind {
                    if tag == "a" {
                        if let Some((_, h)) = attrs.iter().find(|(k, _)| k == "href") {
                            href = Some(h.clone());
                            break;
                        }
                    }
                }
                current = page.dom.node(n).parent;
            }
            href.and_then(|h| page.url.join(&h).ok())
        };

        if let Some(target) = target {
            self.navigate(&target.to_string(), true);
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

        // Default: white background.
        for px in buffer.iter_mut() {
            *px = 0x00FF_FFFF;
        }

        if let Some(page) = &self.page {
            let pmap_w = page.pixmap.width() as usize;
            let pmap_h = page.pixmap.height() as usize;
            let scroll = self.scroll_y as usize;
            let visible_rows = vh.min(pmap_h.saturating_sub(scroll));
            let copy_cols = vw.min(pmap_w);
            let pmap_data = page.pixmap.data();
            for row in 0..visible_rows {
                let src_row = scroll + row;
                if src_row >= pmap_h {
                    break;
                }
                let src_off = src_row * pmap_w * 4;
                let dst_off = row * vw;
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

        buffer.present().expect("present");
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
                // Re-layout + re-paint at the new size.
                if let Some(url) = self.page.as_ref().map(|p| p.url.to_string()) {
                    self.navigate(&url, false);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
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
                    let max_scroll =
                        (page.pixmap.height() as f32 - self.viewport_size.1 as f32).max(0.0);
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
                        ..
                    },
                ..
            } => match logical_key.as_ref() {
                Key::Character("r") | Key::Character("R") => self.reload(),
                Key::Named(NamedKey::ArrowLeft) => self.history_back(),
                Key::Named(NamedKey::ArrowRight) => self.history_forward(),
                Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                    self.scroll_y =
                        (self.scroll_y + self.viewport_size.1 as f32 * 0.9).max(0.0);
                    if let Some(page) = &self.page {
                        let max_scroll = (page.pixmap.height() as f32
                            - self.viewport_size.1 as f32)
                            .max(0.0);
                        self.scroll_y = self.scroll_y.min(max_scroll);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                Key::Named(NamedKey::PageUp) => {
                    self.scroll_y =
                        (self.scroll_y - self.viewport_size.1 as f32 * 0.9).max(0.0);
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
                            - self.viewport_size.1 as f32)
                            .max(0.0);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                _ => {}
            },
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
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
