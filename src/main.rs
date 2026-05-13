#![forbid(unsafe_code)]

mod css;
mod dom;
mod html;
mod net;

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::rc::Rc;

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

const BACKGROUND: u32 = 0x00_1e_1e_2e;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // rustls 0.23 requires picking a crypto provider explicitly. We use ring.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(url) = args.first() {
        return match run_fetch(url) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    run_browser();
    ExitCode::SUCCESS
}

/// Cap on external `<link rel=stylesheet>` fetches per page. Real browsers
/// have no fixed limit but we don't want a hostile page to make us issue
/// hundreds of subrequests.
const MAX_EXTERNAL_STYLESHEETS: usize = 30;

fn run_fetch(url_str: &str) -> Result<(), net::Error> {
    let allow_loopback = std::env::var("DABOSS_ALLOW_LOOPBACK").is_ok();
    let client = net::Client::new().with_allow_loopback(allow_loopback);

    let base_url =
        url::Url::parse(url_str).map_err(|e| net::Error::InvalidUrl(e.to_string()))?;
    let response = client.get(url_str)?;

    eprintln!("HTTP/1.1 {} {}", response.status, response.reason);
    for (name, value) in &response.headers {
        eprintln!("{name}: {value}");
    }
    eprintln!();

    let body = String::from_utf8_lossy(&response.body);
    let dom = html::parse(&body);

    // Discover stylesheets in source order; fetch externals through the same
    // hardened HTTP client we used for the page itself.
    let refs = css::discover_stylesheets(&dom);
    let mut sheets: Vec<css::Stylesheet> = Vec::new();
    let mut external_count = 0usize;
    let mut embedded_count = 0usize;
    for r in refs {
        match r {
            css::StylesheetRef::Embedded(s) => {
                sheets.push(s);
                embedded_count += 1;
            }
            css::StylesheetRef::External(href) => {
                if external_count >= MAX_EXTERNAL_STYLESHEETS {
                    eprintln!("[phase 3] external stylesheet cap reached; skipping {href}");
                    continue;
                }
                external_count += 1;
                let abs = match base_url.join(&href) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("[phase 3] bad <link href>: {e}");
                        continue;
                    }
                };
                let abs_str = abs.to_string();
                match client.get(&abs_str) {
                    Ok(resp) if (200..300).contains(&resp.status) => {
                        let text = String::from_utf8_lossy(&resp.body);
                        sheets.push(css::parse(&text));
                        eprintln!("[phase 3] fetched {abs_str} ({} bytes)", resp.body.len());
                    }
                    Ok(resp) => {
                        eprintln!("[phase 3] {abs_str}: HTTP {}", resp.status);
                    }
                    Err(e) => {
                        eprintln!("[phase 3] {abs_str}: {e}");
                    }
                }
            }
        }
    }

    let style_tree = css::style_dom(&dom, &sheets);
    eprintln!(
        "[phase 3] computed styles for {} nodes from {} embedded + {} external stylesheet(s)",
        style_tree.styles.len(),
        embedded_count,
        external_count
    );

    dom.print();
    Ok(())
}

#[derive(Default)]
struct DaBoss {
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
}

impl ApplicationHandler for DaBoss {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("DaBoss")
            .with_inner_size(LogicalSize::new(1024.0, 768.0));

        let window = Rc::new(event_loop.create_window(attrs).expect("create window"));
        let context = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("softbuffer surface");

        tracing::info!("window ready");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let (Some(window), Some(surface)) =
                    (self.window.as_ref(), self.surface.as_mut())
                else {
                    return;
                };
                let size = window.inner_size();
                let (Some(width), Some(height)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                else {
                    return;
                };

                surface.resize(width, height).expect("surface resize");
                let mut buffer = surface.buffer_mut().expect("buffer");
                for pixel in buffer.iter_mut() {
                    *pixel = BACKGROUND;
                }
                buffer.present().expect("present");
            }
            _ => {}
        }
    }
}

fn run_browser() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = DaBoss::default();
    event_loop.run_app(&mut app).expect("event loop");
}
