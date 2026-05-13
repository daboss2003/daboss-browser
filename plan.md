# DaBoss Browser — Toy Browser in Rust

A learning project. The goal is to understand how browsers work by building one end-to-end. Not to compete with Chrome.

## Goals

- Render a meaningful subset of real-world HTML/CSS to a window
- Walk every layer of the browser pipeline: network → parse → style → layout → paint → interaction
- Embed a JavaScript engine and wire up enough DOM to run small scripts
- Block ads at the network layer (the originally stated reason this project exists)
- Cross-compile to macOS, Linux, and Windows from one codebase

## Non-goals

- Spec compliance. We pick "the 80% that makes most pages legible."
- Performance competitive with real browsers
- Writing our own JS engine, font renderer, or text shaper
- Chrome-style per-site process isolation (we still sandbox the whole process — see the Security section)
- Mobile platforms

If a page renders recognizably and you can click links, scroll, and run small inline scripts, we're done.

---

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Window / Event Loop  (winit)                   │
└─────────────────────────────────────────────────┘
                       │
┌─────────────────────────────────────────────────┐
│  Browser Chrome  (tabs, URL bar, history)       │
└─────────────────────────────────────────────────┘
                       │
┌─────────────────────────────────────────────────┐
│  Engine                                         │
│                                                 │
│   Net  ──►  HTML ──►  DOM  ──►  Style  ──►     │
│   (tokio,   (parser)  (arena)   (CSS cascade)   │
│   rustls)                                       │
│                                                 │
│                       ▼                         │
│                     Layout  ──► Paint           │
│                     (box      (display list,    │
│                      tree)    tiny-skia)        │
│                                                 │
│                       ▲                         │
│   JS Engine  ◄────────┘                         │
│   (boa)                                         │
└─────────────────────────────────────────────────┘
```

### The arena DOM

Everything mutable and tree-shaped lives in an arena. This is how `rustc`, `html5ever`, and Servo avoid the `Rc<RefCell<>>` swamp.

```rust
pub struct Dom {
    nodes: Vec<Node>,
}

pub struct NodeId(u32);

pub struct Node {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    kind: NodeKind,
}

pub enum NodeKind {
    Document,
    Element { tag: TagName, attrs: Vec<(Atom, Atom)> },
    Text(String),
    Comment(String),
}
```

Same pattern for the style tree, box tree, and display list. Indices, not references.

---

## Tech stack

| Layer | Crate | Why |
|---|---|---|
| Window + input | `winit` | De-facto Rust window library |
| Pixel surface | `softbuffer` | Blit a framebuffer to the window, no GPU |
| 2D drawing | `tiny-skia` | CPU rasterizer, Skia-compatible API |
| Text shaping | `cosmic-text` | Fonts and Unicode are not a hill to die on |
| Async runtime | `tokio` | For the network layer |
| HTTP/TLS | `hyper` + `rustls` | Or write HTTP/1.1 by hand for the first weeks |
| URL parsing | `url` | Spec-compliant, save us the headache |
| JS engine | `boa` | Pure Rust, embeddable, slow but works |
| Logging | `tracing` | Better than `println!` once it gets complex |

Everything else — tokenizer, parser, style resolver, layout, paint pipeline — we write.

---

## Security model

This is a toy, but it executes untrusted code from the internet from phase 1 onwards. Treat it as hostile from day one.

### Threat model

| Threat | Defense |
|---|---|
| Memory-corruption exploit in our parsers | Rust + `#![forbid(unsafe_code)]` in our crate |
| Malicious JS escaping to native code | `boa` is pure Rust, no JIT, no FFI, only the DOM bindings we write |
| Browser used to scan our local network (SSRF) | Reject private IP ranges in our HTTP client |
| Path traversal / `file://` snooping | No `file://` scheme at all in v1. Only `http(s)://`. |
| Bugs in font / image decoders we depend on | OS-level sandbox around the whole process |
| Malicious or compromised Cargo dep | `cargo-deny` + `cargo-audit` in CI, pinned `Cargo.lock`, manual review of every bump |
| Contributor PR sneaks in something nasty | Review, minimize trusted-dep surface, `unsafe` always rejected |
| Browser misbehavior nukes your home directory | **Run dev + testing in a sandboxed user/VM, never your main account** |

Out of scope for a toy: side-channel attacks (Spectre, timing), targeted exploits, supply-chain attacks against `rustc` itself.

### Defense in depth, in order of "bang per buck"

**1. Hybrid sandbox: Docker for headless phases, `sandbox-exec` for windowed phases.** The single biggest defense. Blast radius for any mishap is a disposable environment, never your real `~`.

   - **Phases 0–4 (network, HTML, CSS, layout) and phase 9 (adblock):** all work happens inside a Docker container. Source mounted as a volume, `cargo build` / `cargo test` runs in the container, no GUI needed because everything is testable via stdout + PNG snapshots. On macOS, Docker Desktop is itself a Linux VM, so this is VM-grade kernel isolation packaged with better tooling.
   - **Phases 5–8 (paint, interaction, JS, chrome):** the binary needs a window, so we run it on the macOS host launched via `sandbox-exec -f profiles/macos.sb`. Same Seatbelt that macOS uses to confine its own apps. Use a separate macOS user account with no iCloud, no SSH keys, no Keychain entries, no password manager.
   - **CI uses the same Dockerfile** as local dev so "works on my machine" never happens.
   - **Don't:** test the browser on real internet sites from any account where credentials live.

**2. `#![forbid(unsafe_code)]` at the crate root.** No `unsafe` in code we write, ever. Audited `unsafe` from dependencies is unavoidable but we keep our own code 100% safe. This single attribute makes memory-corruption exploits in our code essentially impossible.

**3. Supply-chain hygiene.** Configure in phase 0, never relax:

   - `Cargo.lock` always committed (it is, for binaries)
   - `cargo-deny` with a `deny.toml` that:
     - Rejects crates with `cargo-audit` advisories
     - Banned-licenses list (no GPL pollution unless we intend it)
     - Banned-crates list (sketchy crates by name)
     - Source allowlist: only crates.io, no random git URLs
   - `cargo-audit` runs on every PR in CI
   - Dependabot opens PRs for bumps. **Read every one.** A dep going from `1.4.2` to `1.4.3` with a 2000-line diff is a red flag.
   - Prefer crates from `rust-lang`, `servo`, `tokio-rs`, `bytecodealliance`. Avoid random one-off authors when an alternative exists.

**4. OS sandbox the browser process itself.** Starting phase 5, before we ever load a real internet page, launch the binary through an OS sandbox:

   - **macOS host (phases 5–8):** `sandbox-exec -f profiles/macos.sb ./target/release/daboss`. Seatbelt profile: deny filesystem writes outside `~/Library/Application Support/DaBoss/`, deny reads outside system font directories + the app data dir, allow network. Seatbelt is technically deprecated but works fine for our purposes.
   - **Linux (inside Docker, or native):** [`bubblewrap`](https://github.com/containers/bubblewrap) wrapper. Bind-mount a minimal root, no `/home` except the app data dir, drop all capabilities. Inside Docker we already have isolation, but layering `bwrap` is cheap defense-in-depth.
   - **Windows:** lower the process integrity level via `SetTokenInformation`, or run as a Low-IL process. AppContainer is the full solution but is significant work; defer.

   Add a `cargo run-sandboxed` alias in `.cargo/config.toml` so we never accidentally run it raw on the test machine.

**5. Network safety at the HTTP layer.** In our own client (phase 1), enforce:

   - Resolve hostnames, reject any of: `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `::1`, `fc00::/7`, IPv6 link-local. Toggle via config for localhost test pages.
   - Allowed schemes: `http`, `https`. Nothing else, ever. No `file`, no `data` URLs above a size cap, no `javascript:`.
   - Response size cap (start at 50 MB) so a hostile server can't OOM us
   - Read timeout (30s) so a slowloris server can't pin us forever
   - TLS via `rustls` only; if HTTPS handshake fails, **do not** fall back to plaintext
   - Cap redirect chains at 10
   - Strip `Cookie`, `Authorization` on cross-origin redirects

**6. JS engine surface area.** When we embed `boa` in phase 7:

   - Expose **only** the DOM bindings we explicitly write. No `Deno.*` analog, no filesystem, no spawn, no native modules.
   - Any `fetch()` we expose goes through the same hardened HTTP client from layer 5.
   - Document the JS-callable surface in `docs/js-api.md` and treat additions as security-sensitive.
   - Fuzz the JS-to-DOM bridge: random DOM mutation sequences from JS, assert the engine doesn't panic or corrupt the arena.

**7. Filter list as input.** The ad blocker reads EasyList. Treat it as untrusted:

   - Fetch over HTTPS only, pin the host (`easylist.to`)
   - Strict format validation; reject lines that don't match expected grammar
   - Cap at a reasonable size (10 MB?)
   - On parse error, fall back to last-known-good cached copy, never to "no filtering"

### What this section is **not**

- Not a substitute for Chrome-grade security. We don't have site isolation, JIT hardening, or a security team.
- Not a promise the browser is safe to give to other people. Don't hand the binary to friends and family until much later, if ever.
- Not paranoia theater. Every item here exists because a real browser has been pwned by exactly that class of bug.

### Up-front setup checklist (do before phase 0)

- [ ] Install Docker Desktop on macOS. Confirm `docker run --rm hello-world` works.
- [ ] Create a separate macOS user account for phase 5+ windowed testing (`System Settings → Users & Groups → Add User`). No iCloud sign-in, no SSH keys, no password manager. We'll start using it from phase 5.
- [ ] Write the Dockerfile (Rust toolchain, `cargo-deny`, `cargo-audit`, fonts for headless rendering). Build it once: `docker build -t daboss-dev .`
- [ ] Add `cargo deny check` + `cargo audit` to CI
- [ ] Bookmark this section. Re-read it before phases 1, 5, 7, 9.

---

## Phased roadmap

Each phase ends with a demoable artifact and a website that should "work" at that level.

### Phase 0 — Project setup (1 day)

- `cargo init`, set up workspace if we want crates per layer (recommend: single crate to start, split later)
- Add `#![forbid(unsafe_code)]` at the crate root
- Add `winit`, `softbuffer`, `tiny-skia` as deps
- Write the `Dockerfile`: Rust stable, `cargo-deny`, `cargo-audit`, basic font packages, non-root user
- Write a `justfile` so `just build` / `just test` always go through Docker
- Open a window, fill it with a solid color, exit cleanly on close. This one runs on the host briefly to confirm `winit` works — Docker/host split kicks in fully at phase 1.
- Wire up `tracing` for logging
- **Security:** `deny.toml` in place; CI runs `cargo deny check` + `cargo audit` on every PR

**Outcome:** Dockerized build environment, a window that paints one color, and CI rejects sketchy deps.

### Phase 1 — Network layer (3–5 days)

- Build an HTTP/1.1 client by hand against a `TcpStream` — write the request, parse the response line, parse headers, read the body
- Add HTTPS via `rustls`
- Parse the URL with the `url` crate
- CLI mode: `cargo run -- https://example.com` prints the response body
- **Security:** SSRF guard (reject private IPs), scheme allowlist (`http`/`https` only), response-size cap, read timeout, redirect cap, no plaintext fallback on TLS failure. Unit tests for each of these.

**Outcome:** `curl` reimplemented in 200 lines, with guardrails. You now know HTTP.

**Test pages:** `http://example.com`, `https://example.com`, `https://info.cern.ch/hypertext/WWW/TheProject.html`.

### Phase 2 — HTML parsing (1–2 weeks)

- Tokenizer: state machine that emits `StartTag`, `EndTag`, `Text`, `Comment`, `Doctype`. The HTML spec's state machine is long but mechanical — implement maybe 20 of its ~80 states.
- Tree construction: walk tokens, build the arena DOM. Handle implicit `<html>`, `<head>`, `<body>` insertion. Don't try to be spec-perfect; handle the common cases.
- **Element coverage target.** The parser is name-agnostic — these are the elements layout/paint will care about later:
  - text/inline: `p`, `span`, `a`, `em`, `strong`, `code`, `br`
  - structural: `div`, `section`, `header`, `main`, `nav`, `footer`, `article`, `aside`
  - headings: `h1`–`h6`
  - lists: `ul`, `ol`, `li`, `dl`, `dt`, `dd`
  - tables: `table`, `thead`, `tbody`, `tfoot`, `tr`, `td`, `th` (layout in Phase 4)
  - forms: `form`, `input`, `button`, `label`, `select`, `option`, `textarea` (interaction in Phase 6)
  - media: `img` with `src`/`alt`/`width`/`height` (decode in Phase 5), `iframe` with `src`/`sandbox`
- Print the DOM tree to stdout

**Outcome:** `cargo run -- https://example.com` prints a tree.

**Reference:** [html.spec.whatwg.org/#tokenization](https://html.spec.whatwg.org/multipage/parsing.html#tokenization). Read, do not implement verbatim.

### Phase 3 — CSS parsing + cascade (2–3 weeks)

- Tokenize CSS (similar state machine, simpler than HTML)
- Parse rules into `Selector { ... } -> Declarations { ... }`
- Match selectors against DOM nodes. Support:
  - **Simple selectors:** tag (`div`), class (`.foo`), id (`#bar`), universal (`*`)
  - **Attribute selectors:** `[name]`, `[name=value]`, `~=`, `|=`, `^=`, `$=`, `*=`
  - **Combinators:** descendant (`a b`), child (`a > b`), adjacent sibling (`a + b`), general sibling (`a ~ b`)
  - **Pseudo-classes:** parse all, match only stateless ones (`:root`, `:first-child`, `:last-child`). Stateful ones (`:hover`, `:focus`, etc.) are stored but match nothing until Phase 6.
  - **Pseudo-elements:** parse `::before`, `::after`, `::first-line`, etc. — stored, but don't apply to real DOM nodes until Phase 4 generates content boxes.
- Compute specificity (id, class+attr+pseudo-class, tag), apply cascade, inherit
- **CSS variables:** `--foo: value` declarations stored per-element, inherited like color. `var(--foo, fallback)` resolved at apply time. Two-pass apply (custom props first, then normal) so out-of-order works.
- **`calc()`:** full `+ - * /` expression parser with precedence and parens. Absolute lengths evaluate at cascade time; percentages and `vw`/`vh` defer to layout.
- **`background` shorthand:** extracts color (image / position / repeat reserved for Phase 5 paint).
- **External stylesheets:** `<link rel="stylesheet" href="...">` discovered in source order and fetched through the same SSRF-hardened client. Cap of 30 per page.
- Build a parallel "styled tree" indexed by NodeId

**Outcome:** every DOM element has a `ComputedStyle` with color, font-size, display, margins, padding, etc. Real-world pages with external CSS and CSS variables get correctly cascaded.

### Phase 4 — Layout (2–3 weeks, the hardest phase)

Implement in this order — do not skip ahead:

1. **Block layout only.** Vertical stack of block boxes. Compute width from parent, height from children. Margins, padding, borders. No floats, no flex.
2. **Inline layout.** Wrap text into line boxes using `cosmic-text` for shaping. Mix inline boxes (e.g. `<a>` inside a `<p>`).
3. **Block+inline together.** Anonymous block boxes for stray inlines inside blocks.
4. **Replaced elements** (`<img>`, `<iframe>`). Rectangular boxes whose intrinsic size comes from outside the box tree: `<img>` from the decoded image's pixel dimensions; `<iframe>` from CSS `width`/`height` or the default `300×150`. Treat them as inline-blocks.
5. **Tables.** Two-pass: column-width pass (auto layout — measure each cell's natural width), then row-height pass. Support `rowspan`/`colspan`. Skip `table-layout: fixed`.

Skip for now: flexbox, grid, floats, positioning (absolute/fixed). Add later if you want.

**Outcome:** a "box tree" with `(x, y, width, height)` for every visible element.

**Test pages:** static HTML files in `tests/pages/`. Hand-write 5–10 pages that exercise different layouts.

### Phase 5 — Painting (1 week)

- Walk the box tree, emit a **display list**: `[FillRect, DrawText, DrawBorder, DrawImage, ...]`
- Replay the display list onto a `tiny-skia` `Pixmap` and **save it as a PNG** — keep this working forever, it's how we test layout/paint headlessly in Docker
- **Images:** add the [`image`](https://crates.io/crates/image) crate to decode PNG, JPEG, WebP into `tiny-skia` pixmaps. Draw with `Pixmap::draw_pixmap`. Cap decoded image dimensions (refuse images above e.g. 16384×16384 pixels — decompression bomb defense).
- **iframes:** each nested document renders into its own child pixmap, blitted into the parent at the iframe's box position.
- Only after PNG snapshots look right, wire the pixmap into the `softbuffer` window surface
- Redraw on resize
- **Security:** transition point. PNG-snapshot tests still run inside Docker (no GUI needed). The windowed binary now runs on the macOS host. Write `profiles/macos.sb` (Seatbelt) and the `linux-bwrap.sh` wrapper for CI. From this phase on, **only** launch the windowed binary via `cargo run-sandboxed`. Verify with a deliberate test: a debug build that tries to write to `~/Documents/canary.txt` must be blocked by Seatbelt before we trust the profile.

**Outcome:** real pages render to PNG in Docker, and render to a window on the macOS host inside Seatbelt. Probably ugly. That's fine.

**Test page goal:** Wikipedia article renders legibly with paragraphs, headings, links visibly styled.

### Phase 6 — Interaction (1 week)

- Hit testing: map mouse `(x, y)` to a DOM node by walking the box tree
- Click `<a>` → fetch the new URL → reparse → relayout → repaint
- Mouse wheel scroll: translate the display list's origin
- Keyboard shortcuts: Cmd/Ctrl+L (focus URL bar later), Cmd/Ctrl+R (reload)
- History (back/forward) as a `Vec<Url>` + cursor
- **Forms.** Focus on click into an `<input>`, take key events, fill its value. `<button type="submit">` or Enter in a form input collects the form fields, encodes as `application/x-www-form-urlencoded` (`method="get"` puts it in the query string; `method="post"` sends it as the body), and submits via the network layer. Defer `multipart/form-data` and file uploads.
- **iframes.** Each iframe owns its own DOM, style tree, box tree, and slice of history. Mouse/keyboard events inside the iframe's box route to the nested document. Cross-origin scripted access between parent and child is **denied** — that's our toy version of the same-origin policy. The iframe `sandbox` attribute (when present) further strips JS, forms, top-level navigation.

**Outcome:** you can click around Wikipedia using your own browser, fill in a search box, and pages with iframes render their inner content.

### Phase 6.5 — WebSocket (1 week)

A new module in `src/net/websocket.rs` that piggybacks on the HTTP/1.1 transport we already have.

- **Handshake.** Send GET with `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Version: 13`, `Sec-WebSocket-Key: <random 16 bytes, base64>`. Verify the server's `101` response has `Sec-WebSocket-Accept = base64(sha1(key || "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`.
- **Framing** (RFC 6455). 1-byte header (FIN + RSV + 4-bit opcode), 1/3/9-byte payload length, optional 4-byte mask, payload. Client→server frames **must** be masked; server→client must **not**. Implement opcodes: text (1), binary (2), close (8), ping (9), pong (10), continuation (0).
- **Fragmentation.** Reassemble continuation frames into one message before delivering up to the JS layer.
- **Heartbeat.** Reply to server pings with matching pongs. Send our own pings on idle, time out on no response.
- Exposed to JS as `new WebSocket(url)` in Phase 7.
- **Security.** Scheme allowlist extends to `ws` (→ http transport) and `wss` (→ https transport) — both still flow through the SSRF guard in [dns.rs](src/net/dns.rs) because they resolve via the same code. Message size cap (16 MB), max in-flight frames, idle timeout. Reject any masked server→client frame as a protocol error. Treat control frames > 125 bytes as a protocol error. Refuse to upgrade if `Connection: close` was forced.

**Outcome:** a JS demo page does `new WebSocket("wss://echo.websocket.events")`, sends `"hello"`, gets `"hello"` back, logs to console.

### Phase 7 — JavaScript (2 weeks)

- Embed `boa`
- Expose a minimal DOM API: `document.getElementById`, `.textContent`, `.style`, `.addEventListener("click", ...)`
- Run `<script>` tags after the DOM is built. Don't worry about async / defer.
- Re-layout after DOM mutations from JS
- **Security:** document the entire JS-exposed API surface in `docs/js-api.md`. Treat additions as security-sensitive changes. No filesystem, no spawn, no native modules. `fetch()` if exposed goes through the hardened HTTP client. Add a fuzz target (`cargo fuzz`) for the JS↔DOM bridge.

**Outcome:** small interactive pages work. A `<button>` that changes text on click.

**Note:** 95% of real websites' JS will not run because they expect APIs you haven't implemented. That's expected. Pick 3 toy interactive pages, make those work.

### Phase 8 — Browser chrome (3–5 days)

- A URL bar at the top of the window (paint it yourself with `tiny-skia`, handle key events from `winit`)
- Tabs as `Vec<Tab>` where each `Tab` owns its own DOM + box tree + history
- Active tab indicator, close button, new-tab button
- Reload button

**Outcome:** this looks and feels like a browser, not a demo.

### Phase 9 — Ad blocker (2–3 days)

The original motivation. Now trivial because *you wrote the network layer*.

- Bundle [EasyList](https://easylist.to/easylist/easylist.txt) (just hostnames for now, full ABP syntax later)
- Before issuing any HTTP request, check the hostname against the blocklist
- If blocked, return an empty response immediately
- Optional: cosmetic filtering — inject CSS like `.ad-banner { display: none }` from EasyList's cosmetic rules
- **Security:** fetch updates over HTTPS with a pinned host (`easylist.to`), strict format validation, size cap, fall back to last-known-good on parse error (never to "filtering disabled").

**Outcome:** load a news site, see the ad slots empty out. Compare against the same page in vanilla Chrome.

---

## Post-v1 phases

These are real phases with real plans, but **explicitly gated** behind v1 (phases 0–9) shipping. Do not begin them in parallel — they will stall the rest of the project.

### Phase 11 — WebRTC, data channels only (2–4 months full-time)

WebRTC is a parallel protocol stack with effectively zero shared code with the rest of the browser. Plan it as its own multi-month project after v1 is rendering real pages.

- **Scope.** Data channels only. No audio, no video, no screen sharing. Adding A/V is another 2–4 months on top (Opus codec, VP8/VP9, jitter buffer, lipsync).
- **What to build:**
  - **STUN** (RFC 8489) — binding requests, HMAC-SHA1 integrity. ~800 lines.
  - **TURN** (RFC 8656) — allocate, send/data indications, channel bindings, for relayed traffic when P2P fails. ~1000 lines.
  - **ICE** (RFC 8445) — candidate gathering (host/srflx/relay), prioritized connectivity checks, nomination. ~1500 lines.
  - **DTLS** (RFC 6347) — TLS-over-UDP. `rustls` doesn't do DTLS. Either use a pure-Rust DTLS crate or implement the DTLS 1.2 record layer + handshake against rustls' state machine. ~2000 lines if from scratch.
  - **SCTP-over-DTLS** (RFC 4960 + 8261) — reliable framing for data channels. The biggest single piece. ~3000 lines.
  - **SDP** (RFC 8866) — offer/answer text format for session negotiation. ~1000 lines.
- **Security.** DTLS handshake with self-signed cert + SDP fingerprint pinning. TURN credentials never sent over insecure transport. Rate-limit STUN/TURN to prevent reflection-amplification attacks where our browser is used to flood a victim.
- **Honest disclaimer.** This is the only phase where using an existing library is genuinely tempting. [`str0m`](https://github.com/algesten/str0m) hands you the state machine and lets you wire transport — a reasonable middle ground. [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) is the full stack and defeats most of the learning.

**Outcome:** a JS page creates `new RTCPeerConnection()`, exchanges an SDP offer/answer (via a WebSocket-based signaling server you write, ~50 lines of Node or Python), opens a data channel, sends bytes peer-to-peer.

---

## Cross-platform builds

Rust handles this almost for free. Add to CI on day one so we don't accumulate platform-specific bugs:

```yaml
# .github/workflows/build.yml
strategy:
  matrix:
    include:
      - { os: macos-latest,    target: aarch64-apple-darwin }
      - { os: macos-latest,    target: x86_64-apple-darwin }
      - { os: windows-latest,  target: x86_64-pc-windows-msvc }
      - { os: ubuntu-latest,   target: x86_64-unknown-linux-gnu }
```

`winit`, `softbuffer`, `tiny-skia`, and `cosmic-text` all support all three platforms. No platform-specific code expected until phase 8 if we want native menu bars, which we can skip.

---

## Repo layout

```
daboss_browser/
├── Cargo.toml
├── Cargo.lock                   # always committed
├── deny.toml                    # cargo-deny config
├── Dockerfile                   # Rust toolchain + cargo-deny/audit + fonts for headless rendering
├── docker-compose.yml           # source mount + cache volumes
├── justfile                     # `just build`, `just test`, `just run-sandboxed`
├── plan.md                      # this file
├── profiles/
│   ├── macos.sb                 # Seatbelt sandbox profile (phases 5+)
│   └── linux-bwrap.sh           # bubblewrap launcher
├── docs/
│   └── js-api.md                # JS-callable API surface (phase 7)
├── src/
│   ├── main.rs                  # entry, event loop
│   ├── app.rs                   # browser-level state, tabs, history
│   ├── net/
│   │   ├── mod.rs               # Client + redirect handling
│   │   ├── error.rs
│   │   ├── dns.rs               # SSRF-guarded resolver
│   │   ├── transport.rs         # TCP + TLS connection
│   │   ├── http.rs              # HTTP/1.1 wire protocol
│   │   ├── websocket.rs         # phase 6.5
│   │   └── adblock.rs           # blocklist check (phase 9)
│   ├── html/
│   │   ├── mod.rs
│   │   ├── tokenizer.rs
│   │   └── tree_builder.rs
│   ├── dom/
│   │   ├── mod.rs
│   │   ├── arena.rs
│   │   └── iframe.rs            # nested document contexts
│   ├── css/
│   │   ├── mod.rs
│   │   ├── parser.rs
│   │   ├── selector.rs
│   │   └── cascade.rs
│   ├── layout/
│   │   ├── mod.rs
│   │   ├── block.rs
│   │   ├── inline.rs
│   │   ├── replaced.rs          # <img>, <iframe>
│   │   └── table.rs
│   ├── paint/
│   │   ├── mod.rs
│   │   ├── display_list.rs
│   │   └── image.rs             # PNG/JPEG/WebP decoding
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── urlbar.rs
│   │   └── tabs.rs
│   └── js/
│       ├── mod.rs
│       └── dom_bindings.rs
├── filter-lists/
│   └── easylist-hosts.txt
└── tests/
    ├── pages/                   # hand-rolled HTML test pages
    └── snapshots/               # rendered output for regression tests
```

---

## Testing strategy

- **Unit tests per parser** — tokenizer eats input, asserts the token stream. Same for CSS, URLs.
- **Layout snapshots** — render each test page to a `Pixmap`, save the PNG, diff on future runs. Catches "I broke margin collapsing without noticing."
- **No need for a full JS test suite.** This is a toy. We will not pass Acid3.

---

## Realistic timeline (part-time, evenings/weekends)

| Phase | Calendar weeks |
|---|---|
| 0–1 Setup + network | 1 |
| 2 HTML parsing (incl. table/iframe/form/img elements) | 2 |
| 3 CSS + cascade (incl. variables, calc, attribute selectors, external stylesheets) | 3 |
| 4 Layout (block + inline + replaced + tables + pseudo-elements) | 4 |
| 5 Paint (incl. image decoding) | 1.5 |
| 6 Interaction (incl. forms + iframe nested docs) | 2 |
| 6.5 WebSocket | 1 |
| 7 JS | 2 |
| 8 Chrome | 1 |
| 9 Ad blocker | <1 |

**v1 total: ~18 weeks part-time** to a thing you can browse Wikipedia with, fill forms, block ads in, render iframes, and demo to friends. Full-time: 4–5 weeks.

**Post-v1, only after v1 ships:**

| Phase | Calendar weeks (full-time equivalent) |
|---|---|
| 11 WebRTC (data channels only) | 8–16 |

If you stop after phase 5 you still have a respectable "renders real websites" demo. Phases 6–9 are what make it a browser instead of a renderer.

---

## Reading list

- **[browser.engineering](https://browser.engineering)** — Pavel Panchekha. Python, but the algorithms map directly. Use as a curriculum guide.
- **[Let's Build a Browser Engine](https://limpet.net/mbrubeck/2014/08/08/toy-layout-engine-1.html)** — Matt Brubeck, Rust, layout focused.
- **[Servo source](https://github.com/servo/servo)** — reach for it when stuck. Especially `components/style/` and `components/layout/`.
- **[html5ever source](https://github.com/servo/html5ever)** — reference tokenizer if ours gets weird.
- **HTML Living Standard parsing section** — read, do not implement.
- **CSS 2.1 spec** — short, readable, covers 80% of what we need. Skip CSS 3 specs for now.

---

## Known traps

- **The DOM tree borrow checker fight.** Solved by the arena pattern above. Adopt it on day one.
- **`<script>` ordering and document.write.** Don't implement `document.write`. Pretend it doesn't exist.
- **Quirks mode.** Pretend it doesn't exist. We are standards-only.
- **Float layout.** Genuinely hard. Skip. Modern pages mostly use flexbox/grid, which we also skip.
- **Encoding detection.** Assume UTF-8. If a page is in Shift-JIS, it renders wrong. Acceptable for v1.
- **Cookies, localStorage, IndexedDB.** Defer. None of these are needed to make pages render.

---

## When in doubt

Pick the simplest thing that makes the next test page render. The browser spec is enormous; our finish line is "I understand how browsers work," not "I pass WPT."
