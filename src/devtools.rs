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
    Sources,
    Picker,
}

impl Panel {
    pub fn label(self) -> &'static str {
        match self {
            Panel::Console => "Console",
            Panel::Dom => "Elements",
            Panel::Network => "Network",
            Panel::Storage => "Storage",
            Panel::Sources => "Sources",
            Panel::Picker => "Picker",
        }
    }

    /// Cycle through the panels in order — Tab pages through.
    pub fn next(self) -> Self {
        match self {
            Panel::Console => Panel::Dom,
            Panel::Dom => Panel::Network,
            Panel::Network => Panel::Storage,
            Panel::Storage => Panel::Sources,
            Panel::Sources => Panel::Picker,
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
    pub sources: SourcesPanelState,
}

/// State for the Sources devtools panel: which registered source
/// map is currently selected and where the user's cursor sits in
/// that source's text. The breakpoint set itself lives in
/// `crate::source_map::BREAKPOINTS` so the JS engine can query it
/// without depending on the devtools module.
#[derive(Default, Debug, Clone)]
pub struct SourcesPanelState {
    /// Sorted list of source-map keys captured at panel open time so
    /// keystroke navigation has stable indices. Refreshed in
    /// `refresh_sources`.
    pub map_keys: Vec<String>,
    /// Index into `map_keys` of the active map. `None` when the
    /// registry is empty.
    pub selected_map: Option<usize>,
    /// Index into the active map's `sources` list (the original
    /// pre-bundled files inside one map). `None` for empty maps.
    pub selected_source: Option<usize>,
    /// Cursor line (0-indexed) within the selected source.
    pub cursor_line: u32,
    /// First visible line — the scroll position of the viewport.
    pub scroll_top: u32,
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
            sources: SourcesPanelState::default(),
        }
    }

    /// Re-snapshot the live source-map registry into the sources
    /// panel state. Call this whenever the panel becomes visible or
    /// a navigation key fires — the underlying registry can change
    /// any time a script registers a new map.
    pub fn refresh_sources(&mut self) {
        use crate::source_map;
        let snap = source_map::snapshot();
        let mut keys: Vec<String> = snap.iter().map(|(k, _)| k.clone()).collect();
        keys.sort();
        let prior_selected_key = self
            .sources
            .selected_map
            .and_then(|i| self.sources.map_keys.get(i).cloned());
        self.sources.map_keys = keys;
        self.sources.selected_map = match prior_selected_key {
            Some(k) => self.sources.map_keys.iter().position(|x| x == &k).or_else(
                || if self.sources.map_keys.is_empty() { None } else { Some(0) },
            ),
            None => {
                if self.sources.map_keys.is_empty() {
                    None
                } else {
                    Some(0)
                }
            }
        };
        // Default to the map's first source if we've never picked one.
        if self.sources.selected_source.is_none() && self.sources.selected_map.is_some() {
            self.sources.selected_source = Some(0);
        }
    }

    pub fn sources_next_map(&mut self) {
        if self.sources.map_keys.is_empty() {
            return;
        }
        let n = self.sources.map_keys.len();
        self.sources.selected_map = Some(match self.sources.selected_map {
            Some(i) => (i + 1) % n,
            None => 0,
        });
        // Reset cursor + selected source when changing maps.
        self.sources.selected_source = Some(0);
        self.sources.cursor_line = 0;
        self.sources.scroll_top = 0;
    }

    pub fn sources_next_source(&mut self) {
        let Some(map) = self.active_map() else {
            return;
        };
        let n = map.sources.len();
        if n == 0 {
            return;
        }
        let cur = self.sources.selected_source.unwrap_or(0);
        self.sources.selected_source = Some((cur + 1) % n);
        self.sources.cursor_line = 0;
        self.sources.scroll_top = 0;
    }

    pub fn sources_cursor_down(&mut self, step: u32) {
        let Some(content) = self.active_source_content() else {
            return;
        };
        let max = content.lines().count().saturating_sub(1) as u32;
        self.sources.cursor_line = (self.sources.cursor_line.saturating_add(step)).min(max);
    }

    pub fn sources_cursor_up(&mut self, step: u32) {
        self.sources.cursor_line = self.sources.cursor_line.saturating_sub(step);
    }

    /// Toggle a breakpoint at the cursor's current line in the
    /// selected source. No-op if no source is selected.
    pub fn sources_toggle_breakpoint(&mut self) {
        let (Some(map_idx), Some(src_idx)) =
            (self.sources.selected_map, self.sources.selected_source)
        else {
            return;
        };
        let Some(key) = self.sources.map_keys.get(map_idx).cloned() else {
            return;
        };
        crate::source_map::toggle_breakpoint(&key, src_idx, self.sources.cursor_line);
    }

    /// True when a breakpoint is set at `(map_key, source_idx, line)`.
    pub fn has_breakpoint(&self, map_key: &str, source_idx: usize, line: u32) -> bool {
        crate::source_map::has_breakpoint(map_key, source_idx, line)
    }

    fn active_map(&self) -> Option<crate::source_map::SourceMap> {
        let idx = self.sources.selected_map?;
        let key = self.sources.map_keys.get(idx)?;
        crate::source_map::SOURCE_MAPS.with(|s| s.borrow().get(key).cloned())
    }

    /// Returns the original source text the user is currently
    /// viewing, or None if no source is loaded.
    pub fn active_source_content(&self) -> Option<String> {
        let map = self.active_map()?;
        let idx = self.sources.selected_source?;
        map.sources_content.get(idx).cloned().flatten()
    }

    /// Filename of the currently-viewed original source.
    pub fn active_source_filename(&self) -> Option<String> {
        let map = self.active_map()?;
        let idx = self.sources.selected_source?;
        map.sources.get(idx).cloned()
    }

    /// Snapshot of all (map_key, source_idx, line) breakpoints. The JS
    /// engine reads this before each script eval to know where to
    /// inject hit callbacks.
    pub fn breakpoints_snapshot(&self) -> Vec<(String, usize, u32)> {
        crate::source_map::BREAKPOINTS.with(|s| s.borrow().iter().cloned().collect())
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Cycle to the next panel.
    pub fn cycle_panel(&mut self) {
        self.panel = self.panel.next();
        // Lazily snapshot the source-map registry whenever the
        // Sources panel comes into view — scripts may have just
        // registered new maps via `data:` source-map URLs.
        if matches!(self.panel, Panel::Sources) {
            self.refresh_sources();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_map::{self, SourceMap};

    fn seed_map(key: &str, sources: Vec<&str>, contents: Vec<&str>) {
        let map = SourceMap {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            sources_content: contents
                .iter()
                .map(|s| Some(s.to_string()))
                .collect(),
            ..SourceMap::default()
        };
        source_map::register(key.to_string(), map);
    }

    #[test]
    fn sources_panel_refresh_picks_up_registered_map() {
        source_map::clear();
        seed_map(
            "<inline #0>",
            vec!["a.ts"],
            vec!["const a = 1;\nconst b = 2;"],
        );
        let mut dt = DevTools::new();
        dt.refresh_sources();
        assert_eq!(dt.sources.map_keys, vec!["<inline #0>".to_string()]);
        assert_eq!(dt.sources.selected_map, Some(0));
        assert_eq!(dt.sources.selected_source, Some(0));
        assert_eq!(dt.active_source_filename().as_deref(), Some("a.ts"));
        source_map::clear();
    }

    #[test]
    fn cursor_down_clamps_to_last_line_and_toggle_persists() {
        source_map::clear();
        seed_map(
            "<inline #0>",
            vec!["a.ts"],
            vec!["line1\nline2\nline3"],
        );
        let mut dt = DevTools::new();
        dt.refresh_sources();
        // Cursor at 0, drive down 5: clamps to 2 (3 lines total).
        dt.sources_cursor_down(5);
        assert_eq!(dt.sources.cursor_line, 2);
        // Toggle a breakpoint at the cursor; verify via has_breakpoint.
        dt.sources_toggle_breakpoint();
        assert!(dt.has_breakpoint("<inline #0>", 0, 2));
        // Toggling again clears.
        dt.sources_toggle_breakpoint();
        assert!(!dt.has_breakpoint("<inline #0>", 0, 2));
        source_map::clear();
    }

    #[test]
    fn cycling_to_sources_panel_refreshes_registry() {
        source_map::clear();
        seed_map("<inline #0>", vec!["a.ts"], vec!["const a = 1;"]);
        let mut dt = DevTools::new();
        // Walk panels until we land on Sources.
        for _ in 0..6 {
            dt.cycle_panel();
            if matches!(dt.panel, Panel::Sources) {
                break;
            }
        }
        assert!(matches!(dt.panel, Panel::Sources));
        assert!(!dt.sources.map_keys.is_empty());
        source_map::clear();
    }
}
