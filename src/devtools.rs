//! Minimal devtools overlay.
//!
//! Pressing F12 toggles a console panel docked along the bottom of
//! the window. The panel captures every `console.{log,warn,error,
//! info,debug}` call into a ring buffer, lets the user type a JS
//! expression, and shows its evaluation result alongside the
//! captured logs.
//!
//! Out of scope (deferred):
//!   * DOM tree inspector / element selector picker.
//!   * Network panel + request timing.
//!   * Performance flame chart.
//!   * Breakpoint debugger / source maps.
//!   * Persistent prefs (panel size, font size).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// Lines captured from `console.*` plus user evaluation results.
#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub level: ConsoleLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Warn,
    Error,
    Info,
    Debug,
    /// `>` prompt — the user-entered command.
    Prompt,
    /// `=>` evaluation result.
    Result,
}

impl ConsoleLevel {
    pub fn label(self) -> &'static str {
        match self {
            ConsoleLevel::Log => "log",
            ConsoleLevel::Warn => "warn",
            ConsoleLevel::Error => "error",
            ConsoleLevel::Info => "info",
            ConsoleLevel::Debug => "debug",
            ConsoleLevel::Prompt => ">",
            ConsoleLevel::Result => "=>",
        }
    }
}

/// Soft cap on captured lines. Older entries get evicted from the
/// front of the deque.
pub const CONSOLE_CAPACITY: usize = 500;

pub type ConsoleBuffer = Rc<RefCell<VecDeque<ConsoleLine>>>;

thread_local! {
    /// Installed by the engine for the lifetime of a page so the
    /// console shims can push captured lines from native code.
    pub static JS_CONSOLE_BUFFER: RefCell<Option<ConsoleBuffer>> =
        const { RefCell::new(None) };
}

/// Append a captured console line, evicting the oldest if at cap.
pub fn push_console(level: ConsoleLevel, text: String) {
    JS_CONSOLE_BUFFER.with(|slot| {
        if let Some(buf) = slot.borrow().as_ref() {
            let mut b = buf.borrow_mut();
            while b.len() >= CONSOLE_CAPACITY {
                b.pop_front();
            }
            b.push_back(ConsoleLine { level, text });
        }
    });
}

/// Which panel is currently visible inside the devtools overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Console,
    Dom,
    Network,
    Storage,
    Picker,
}

impl Panel {
    pub fn label(self) -> &'static str {
        match self {
            Panel::Console => "Console",
            Panel::Dom => "Elements",
            Panel::Network => "Network",
            Panel::Storage => "Storage",
            Panel::Picker => "Picker",
        }
    }

    /// Cycle through the panels in order — Tab pages through.
    pub fn next(self) -> Self {
        match self {
            Panel::Console => Panel::Dom,
            Panel::Dom => Panel::Network,
            Panel::Network => Panel::Storage,
            Panel::Storage => Panel::Picker,
            Panel::Picker => Panel::Console,
        }
    }
}

/// One captured network request. The browser shell pushes into the
/// shared buffer from `net::Client::do_request`; the network panel
/// reads it.
#[derive(Debug, Clone)]
pub struct NetworkEntry {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub body_size: u64,
    pub duration_ms: u32,
}

/// Bounded network log shared with the network client.
pub const NETWORK_CAPACITY: usize = 200;
pub type NetworkLog = Rc<RefCell<VecDeque<NetworkEntry>>>;

thread_local! {
    /// Network capture shared with `net::Client`. The browser shell
    /// installs the buffer at startup; the network code pushes
    /// every successful or failed request.
    pub static NETWORK_LOG: RefCell<Option<NetworkLog>> = const { RefCell::new(None) };
}

pub fn push_network(entry: NetworkEntry) {
    NETWORK_LOG.with(|slot| {
        if let Some(log) = slot.borrow().as_ref() {
            let mut q = log.borrow_mut();
            while q.len() >= NETWORK_CAPACITY {
                q.pop_front();
            }
            q.push_back(entry);
        }
    });
}

/// Devtools panel state attached to the Browser.
pub struct DevTools {
    /// `true` when the overlay is visible.
    pub open: bool,
    /// Active panel.
    pub panel: Panel,
    /// User input in the console prompt line.
    pub input: String,
    /// Scrollback view position — index of the topmost visible
    /// console line. Recompute on resize / new lines.
    pub scroll: usize,
    /// Captured console output, shared with the JS engine's shims
    /// via [`JS_CONSOLE_BUFFER`].
    pub buffer: ConsoleBuffer,
    /// Past evaluation inputs. Up-arrow walks back through them.
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    /// Captured network requests, shared with `net::Client` via
    /// [`NETWORK_LOG`].
    pub network: NetworkLog,
    /// When the Picker panel is active, the most recent hovered
    /// element index (live, follows the cursor). Read by the
    /// painter to overlay an outline.
    pub picker_target: Option<u32>,
}

impl Default for DevTools {
    fn default() -> Self {
        Self::new()
    }
}

impl DevTools {
    pub fn new() -> Self {
        Self {
            open: false,
            panel: Panel::Console,
            input: String::new(),
            scroll: 0,
            buffer: Rc::new(RefCell::new(VecDeque::with_capacity(CONSOLE_CAPACITY))),
            history: Vec::new(),
            history_cursor: None,
            network: Rc::new(RefCell::new(VecDeque::with_capacity(NETWORK_CAPACITY))),
            picker_target: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Cycle to the next panel.
    pub fn cycle_panel(&mut self) {
        self.panel = self.panel.next();
    }

    /// Submit the currently-typed line. Returns the source string for
    /// the caller to feed into the JS engine; clears `input` and
    /// echoes the prompt into the scrollback.
    pub fn submit(&mut self) -> Option<String> {
        let src = std::mem::take(&mut self.input);
        if src.trim().is_empty() {
            return None;
        }
        self.history.push(src.clone());
        self.history_cursor = None;
        let mut buf = self.buffer.borrow_mut();
        while buf.len() >= CONSOLE_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(ConsoleLine {
            level: ConsoleLevel::Prompt,
            text: src.clone(),
        });
        Some(src)
    }

    /// Record an evaluation result line.
    pub fn push_result(&mut self, text: String) {
        let mut buf = self.buffer.borrow_mut();
        while buf.len() >= CONSOLE_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(ConsoleLine {
            level: ConsoleLevel::Result,
            text,
        });
    }

    /// Walk back through previous prompts (up arrow).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        self.input = self.history[next].clone();
    }

    /// Walk forward through previous prompts (down arrow). Clears
    /// the input once we step past the end.
    pub fn history_next(&mut self) {
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 >= self.history.len() {
            self.history_cursor = None;
            self.input.clear();
        } else {
            self.history_cursor = Some(i + 1);
            self.input = self.history[i + 1].clone();
        }
    }
}
