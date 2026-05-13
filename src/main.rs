#![forbid(unsafe_code)]

mod net;

use std::io::Write as _;
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

fn run_fetch(url: &str) -> Result<(), net::Error> {
    let allow_loopback = std::env::var("DABOSS_ALLOW_LOOPBACK").is_ok();
    let client = net::Client::new().with_allow_loopback(allow_loopback);
    let response = client.get(url)?;

    println!("HTTP/1.1 {} {}", response.status, response.reason);
    for (name, value) in &response.headers {
        println!("{name}: {value}");
    }
    println!();
    let _ = std::io::stdout().write_all(&response.body);
    println!();
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
