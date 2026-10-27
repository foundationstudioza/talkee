//! The window the desktop app does not have.
//!
//! # Why this is a page and not a window
//!
//! A tray application has no window, and what this one has to show is a QR code, a sentence about
//! what is happening, and a button when a virtual microphone is missing. A native window for that
//! means either a GUI stack added to a 1.8 MB binary or a few hundred lines of hand-written Win32
//! that nobody can look at — and a QR code drawn by hand is exactly the thing that looks right and
//! does not scan.
//!
//! The browser is already installed, renders the code at any size, and can be *read*: the link sits
//! under the code as text, and the page's own state is available as JSON so a failure can be
//! diagnosed rather than described.
//!
//! # Bound to loopback, and that is not a detail
//!
//! The page contains the pairing key. Anyone who can fetch it can speak through this microphone, so
//! it is served on `127.0.0.1` and nowhere else — reachable by this machine and by nothing on the
//! network. The alternative, writing it to a file, is the same exposure with less freshness: a file
//! goes stale and cannot say what is happening now.
//!
//! # Deliberately small
//!
//! There is no router, no keep-alive, no chunked encoding and no method other than `GET`. It answers
//! two paths and closes. A wizard that needs an HTTP framework is a wizard that has grown into a web
//! server.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use talkee_core::live::{LiveControls, Setting};

use crate::pairing;

/// Where the wizard listens. Loopback only: see the module docs.
pub const PORT: u16 = 47_802;

/// How long a client has to send its request line before it is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What the page says about the app right now.
///
/// Kept as plain data, set by the main loop on every tick and read by the HTTP thread. No lock is
/// held across a request: the whole struct is copied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Whether a virtual microphone exists to play into.
    pub cable: bool,
    /// Whether a phone has proved it holds the key, so audio is being accepted.
    pub paired: bool,
    /// Whether audio is flowing *now*, which is not the same as paired: a phone can pair and go quiet.
    pub live: bool,
    /// What the phone calls itself, once it is connected.
    pub phone: Option<String>,
    /// Why the app is not doing what it should be, when there is a reason worth showing.
    pub problem: Option<String>,
    /// Level of the audio arriving from the phone, 0 to 1.
    pub level_in: f32,
    /// Level of the audio leaving into the virtual microphone, 0 to 1.
    pub level_out: f32,
}

impl Snapshot {
    /// The one sentence the page shows.
    ///
    /// Ordered by what the user can act on: a missing cable beats everything because nothing else can
    /// work without it, and a problem beats the happy path because it is the thing they have to do
    /// something about. Written as a chain rather than one big match because the match is where an
    /// uncovered combination hides — and an uncovered combination here is a page with no sentence on it.
    pub fn headline(&self) -> &'static str {
        if !self.cable {
            return "No virtual microphone";
        }
        if self.problem.is_some() {
            return "Something is wrong";
        }
        match (self.paired, self.live) {
            (false, _) => "Scan this code with your phone",
            (true, true) => "Your phone is your microphone",
            (true, false) => "Connected — waiting for audio",
        }
    }

    /// The sentence under the headline: what to do about it.
    pub fn detail(&self) -> String {
        if let Some(problem) = &self.problem {
            return problem.clone();
        }
        match (self.cable, self.paired, self.live) {
            (false, _, _) => {
                "Talkee needs a virtual audio cable to play into, and Windows has no way to make one \
                 without a driver. Choose \"Install VB-CABLE\" in the tray menu."
                    .to_string()
            }
            (true, false, _) => {
                "Open Talkee on your phone and point it at the code below. You only have to do this \
                 once — this PC remembers your phone after that."
                    .to_string()
            }
            (true, true, true) => match &self.phone {
                Some(phone) => format!("{phone} is connected. In Discord, Zoom or OBS, choose the \
                    microphone called \"CABLE Output\"."),
                None => "Your phone is connected. In other apps, choose the microphone called \
                    \"CABLE Output\"."
                    .to_string(),
            },
            (true, true, false) => {
                "Your phone is paired but nothing is arriving. Check that its microphone is not \
                 muted, and that it is still on this network."
                    .to_string()
            }
        }
    }
}
/// Everything a request handler needs, behind one `Arc`.
struct Shared {
    state: Mutex<Snapshot>,
    link: Mutex<Option<String>>,
    controls: LiveControls,
    /// The updater, once the app has one. The window's "restart to update" button reaches it here.
    updater: Mutex<Option<crate::update::Updater>>,
    /// A random value the page is served with and a change request must repeat.
    ///
    /// The server is on loopback, but a browser will happily send a request to loopback on behalf of
    /// *any* web page the user has open. Without this, a page on the internet could mute the
    /// microphone or turn the filters off. It cannot read this value (cross-origin reads are
    /// blocked), so it cannot forge a request.
    token: String,
}

/// A running wizard.
#[derive(Clone)]
pub struct Wizard {
    shared: Arc<Shared>,
    /// The port actually bound, so a test can use an ephemeral one instead of fighting the real app
    /// for the fixed port.
    port: u16,
}

/// 128 random bits as hex, from the standard library's per-process random hasher keys.
///
/// Not a cryptographic generator, and it does not need to be: it only has to be unguessable by a web
/// page that cannot see this machine, for the lifetime of one run.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for salt in 0u64..2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

impl Wizard {
    /// Starts serving on loopback and returns the handle the app publishes into.
    ///
    /// The port is fixed rather than chosen by the OS, because the address is one a person may end up
    /// typing if the browser does not open by itself — a different port every run would make that
    /// useless. A port already in use is reported rather than worked around: another copy of this app
    /// holding it is the likely reason, and that is worth saying.
    pub fn start(controls: LiveControls) -> Result<Self> {
        Self::start_on(PORT, controls)
    }

    /// The same thing on a given port. `0` lets the OS choose, which is what the tests use.
    pub fn start_on(port: u16, controls: LiveControls) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .with_context(|| format!("serving the wizard on 127.0.0.1:{port}"))?;
        let bound = listener
            .local_addr()
            .context("reading the wizard's address")?
            .port();
        let shared = Arc::new(Shared {
            state: Mutex::new(Snapshot::default()),
            link: Mutex::new(None),
            controls,
            updater: Mutex::new(None),
            token: random_token(),
        });

        let served = Arc::clone(&shared);
        thread::Builder::new()
            .name("talkee-wizard".into())
            .spawn(move || {
                for incoming in listener.incoming() {
                    match incoming {
                        Ok(stream) => {
                            let shared = Arc::clone(&served);
                            // One thread per request, detached. A browser opens several connections at
                            // once and abandons them freely, so the accept loop must not wait on any
                            // single one of them.
                            let _ = thread::Builder::new()
                                .name("talkee-wizard-req".into())
                                .spawn(move || {
                                    let _ = answer(stream, &shared);
                                });
                        }
                        // A failed accept is transient on a loopback socket, not a reason to stop
                        // serving for the rest of the run.
                        Err(_) => continue,
                    }
                }
            })
            .context("starting the wizard thread")?;

        Ok(Self {
            shared,
            port: bound,
        })
    }

    /// Gives the wizard the updater, so the window can show an update and start it.
    pub fn set_updater(&self, updater: crate::update::Updater) {
        if let Ok(mut held) = self.shared.updater.lock() {
            *held = Some(updater);
        }
    }

    /// The port the dashboard and the window talk to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The value a request must repeat to change a setting or read the pairing link.
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// The address to open.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Tells the page what is true now.
    ///
    /// Called from the main loop on every tick. Publishing the same value repeatedly is free: the page
    /// polls, so there is no push to do work for.
    pub fn publish(&self, snapshot: Snapshot) {
        if let Ok(mut held) = self.shared.state.lock() {
            *held = snapshot;
        }
    }

    /// Sets or clears the pairing link the page shows as a code.
    pub fn set_link(&self, link: Option<&str>) {
        if let Ok(mut held) = self.shared.link.lock() {
            *held = link.map(str::to_string);
        }
    }
}

/// Reads one request and answers it.
fn answer(mut stream: TcpStream, shared: &Arc<Shared>) -> Result<()> {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .context("setting the wizard read timeout")?;
    stream
        .set_write_timeout(Some(READ_TIMEOUT))
        .context("setting the wizard write timeout")?;

    // Only the request line is needed. Reading headers we do not interpret is reading input we do not
    // need, from a client we have not authenticated.
    let mut reader = BufReader::new(stream.try_clone().context("cloning the wizard socket")?);
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return Ok(());
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let (status, content_type, body) = match (method, path) {
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", page(shared)),
        ("GET", "/status") => ("200 OK", "application/json", status_json(shared)),
        ("GET", "/favicon.ico") => ("204 No Content", "text/plain", String::new()),
        ("POST", "/set") => change_setting(shared, query),
        ("GET", "/link") => read_link(shared, query),
        ("POST", "/update") => start_update(shared, query),
        ("POST", "/reconnect") => {
            let token = query.split('&').find_map(|pair| pair.strip_prefix("t=")).unwrap_or("");
            if token != shared.token {
                ("403 Forbidden", "text/plain; charset=utf-8", "bad token".to_string())
            } else {
                shared.controls.request_reconnect();
                ("200 OK", "application/json", "{}".to_string())
            }
        }
        ("GET" | "POST", _) => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "no such page".to_string(),
        ),
        _ => (
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "GET and POST only".to_string(),
        ),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("writing the wizard response")?;
    stream.flush().ok();
    Ok(())
}

/// Handles `POST /update?t=<token>`: install the update that has been downloaded.
fn start_update(shared: &Arc<Shared>, query: &str) -> (&'static str, &'static str, String) {
    let token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("t="))
        .unwrap_or("");
    if token != shared.token {
        return ("403 Forbidden", "text/plain; charset=utf-8", "bad token".to_string());
    }
    let updater = shared.updater.lock().ok().and_then(|u| u.clone());
    match updater.map(|u| u.install()) {
        Some(Ok(())) => ("200 OK", "application/json", "{}".to_string()),
        Some(Err(e)) => ("409 Conflict", "text/plain; charset=utf-8", format!("{e:#}")),
        None => ("404 Not Found", "text/plain; charset=utf-8", "no updater".to_string()),
    }
}

/// Handles `GET /link?t=<token>`: the pairing link, as text, for the native window to draw as a code.
///
/// Behind the token because the link carries the pairing key. The dashboard page never asks for it —
/// it gets the code drawn server-side — so this exists only for a client that has been handed the
/// token on its command line.
fn read_link(shared: &Arc<Shared>, query: &str) -> (&'static str, &'static str, String) {
    let token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("t="))
        .unwrap_or("");
    if token != shared.token {
        return ("403 Forbidden", "text/plain; charset=utf-8", "bad token".to_string());
    }
    match shared.link.lock().ok().and_then(|l| l.clone()) {
        Some(link) => ("200 OK", "text/plain; charset=utf-8", link),
        None => ("404 Not Found", "text/plain; charset=utf-8", "no link".to_string()),
    }
}

/// Handles `POST /set?k=<name>&v=<0|1>&t=<token>`.
fn change_setting(shared: &Arc<Shared>, query: &str) -> (&'static str, &'static str, String) {
    let mut key = "";
    let mut value = "";
    let mut token = "";
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("k", v)) => key = v,
            Some(("v", v)) => value = v,
            Some(("t", v)) => token = v,
            _ => {}
        }
    }
    if token != shared.token {
        return ("403 Forbidden", "text/plain; charset=utf-8", "bad token".to_string());
    }
    let setting = match key {
        "paused" => Setting::Paused,
        "noise" => Setting::NoiseSuppression,
        "echo" => Setting::EchoCancel,
        "level" => Setting::AutoLevel,
        _ => {
            return ("400 Bad Request", "text/plain; charset=utf-8", "unknown setting".to_string())
        }
    };
    shared.controls.set(setting, value == "1");
    ("200 OK", "application/json", "{}".to_string())
}

/// The page state as JSON, so the page can poll rather than be reloaded.
/// The link is *not* included. It is already in the page as a code, and repeating a credential in a
/// second place is how it ends up in a log.
fn status_json(shared: &Arc<Shared>) -> String {
    let snapshot = shared.state.lock().map(|s| s.clone()).unwrap_or_default();
    let has_code = shared.link.lock().map(|l| l.is_some()).unwrap_or(false);
    let c = &shared.controls;
    format!(
        "{{\"cable\":{},\"paired\":{},\"live\":{},\"hasCode\":{},\"headline\":{},\"detail\":{},\"phone\":{},\"problem\":{},\
\"levelIn\":{:.3},\"levelOut\":{:.3},\"paused\":{},\"noise\":{},\"echo\":{},\"autoLevel\":{},\"update\":{}}}",
        snapshot.cable,
        snapshot.paired,
        snapshot.live,
        has_code,
        json_string(snapshot.headline()),
        json_string(&snapshot.detail()),
        json_string(
            c.phone()
                .or_else(|| snapshot.phone.clone())
                .as_deref()
                .unwrap_or("")
        ),
        json_string(snapshot.problem.as_deref().unwrap_or("")),
        snapshot.level_in.clamp(0.0, 1.0),
        snapshot.level_out.clamp(0.0, 1.0),
        c.get(Setting::Paused),
        c.get(Setting::NoiseSuppression),
        c.get(Setting::EchoCancel),
        c.get(Setting::AutoLevel),
        json_string(
            shared
                .updater
                .lock()
                .ok()
                .and_then(|u| u.as_ref().and_then(|u| u.ready_version()))
                .as_deref()
                .unwrap_or("")
        ),
    )
}

/// A JSON string literal, escaped.
///
/// Hand-rolled because the only values are sentences this crate built and a phone's own name, and a
/// dependency for six escape rules is not a trade worth making. The escapes are the ones that would
/// otherwise break the document.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The dashboard.
///
/// The code is rendered once, server-side, and everything else is updated by polling. That split
/// matters: re-rendering a QR every few hundred milliseconds would be work for nothing, and a code
/// that redraws is a code a camera can catch mid-change.
fn page(shared: &Arc<Shared>) -> String {
    let snapshot = shared.state.lock().map(|s| s.clone()).unwrap_or_default();
    let link = shared.link.lock().ok().and_then(|l| l.clone());

    let code = match &link {
        Some(link) => match pairing::qr_svg(link) {
            Ok(svg) => format!("<div class=\"qr\" id=\"code\">{svg}</div>"),
            Err(e) => format!("<p class=\"warn\">could not draw the code: {}</p>", escape_html(&e.to_string())),
        },
        // No link means pairing is off, which is a state worth naming rather than hiding: the app is
        // accepting audio from anyone who can reach it.
        None => "<p class=\"warn\">Pairing is off, so no code is needed — this PC accepts audio from \
                 any phone that can reach it.</p>"
            .to_string(),
    };

    DASHBOARD
        .replace("{{HEADLINE}}", &escape_html(snapshot.headline()))
        .replace("{{DETAIL}}", &escape_html(&snapshot.detail()))
        .replace("{{CODE}}", &code)
        .replace("{{TOKEN}}", &shared.token)
}

/// The dashboard markup. Plain HTML with two placeholders filled in by [`page`]; kept as a constant
/// rather than a `format!` string because it is mostly braces.
const DASHBOARD: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Talkee</title>
<style>
  :root { color-scheme: dark; --bg:#0B0D10; --panel:#12151A; --line:#1E232B; --text:#E8E8EA;
          --dim:#8B8F98; --faint:#4A4F58; --green:#4ADE80; --amber:#FBBF24; --red:#F87171; }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--bg); color:var(--text);
         font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; }
  main { max-width:520px; margin:0 auto; padding:28px 20px 56px; }
  header { display:flex; align-items:center; justify-content:space-between; margin-bottom:28px; }
  .brand { font-weight:600; letter-spacing:.2px; }
  .pill { display:flex; align-items:center; gap:8px; font-size:13px; color:var(--dim);
          background:var(--panel); border:1px solid var(--line); border-radius:999px; padding:5px 12px; }
  .dot { width:8px; height:8px; border-radius:50%; background:var(--amber); }
  .live .dot { background:var(--green); box-shadow:0 0 8px var(--green); }
  .muted .dot, .error .dot { background:var(--red); }
  .hero { text-align:center; padding:8px 0 28px; }
  .orb-wrap { height:150px; display:flex; align-items:center; justify-content:center; }
  .orb { width:92px; height:92px; border-radius:50%; background:var(--amber);
         opacity:.25; transition:transform .12s ease-out, opacity .12s, background .3s; }
  .live .orb { background:var(--green); } .muted .orb, .error .orb { background:var(--red); }
  h1 { font-size:24px; font-weight:300; letter-spacing:-.4px; margin:6px 0 4px; }
  .sub { color:var(--dim); font-size:14px; margin:0 auto; max-width:40ch; }
  .meters { display:grid; gap:12px; margin:0 0 22px; }
  .meter label { display:flex; justify-content:space-between; font-size:12px; color:var(--dim); margin-bottom:5px; }
  .bar { height:6px; background:var(--panel); border-radius:3px; overflow:hidden; }
  .bar i { display:block; height:100%; width:0; background:var(--green); border-radius:3px;
           transition:width .1s linear; }
  .panel { background:var(--panel); border:1px solid var(--line); border-radius:14px; }
  .row { display:flex; align-items:center; justify-content:space-between; padding:14px 16px;
         border-top:1px solid var(--line); cursor:pointer; user-select:none; }
  .row:first-child { border-top:0; }
  .row b { font-weight:500; display:block; } .row span { color:var(--dim); font-size:13px; }
  .switch { width:42px; height:24px; border-radius:12px; background:#2A2F38; position:relative;
            transition:background .2s; flex:none; margin-left:16px; }
  .switch::after { content:""; position:absolute; top:3px; left:3px; width:18px; height:18px;
                   border-radius:50%; background:#fff; transition:transform .2s; }
  .on .switch { background:var(--green); } .on .switch::after { transform:translateX(18px); }
  details { margin-top:18px; } summary { cursor:pointer; color:var(--dim); font-size:14px; padding:8px 2px; }
  .qr { background:#fff; border-radius:14px; padding:16px; display:inline-block; margin-top:10px; }
  .qr svg { display:block; width:220px; height:220px; }
  .center { text-align:center; }
  .warn { color:var(--amber); font-size:14px; }
  .hint { margin-top:22px; color:var(--faint); font-size:13px; text-align:center; }
  .hint code { color:var(--dim); background:var(--panel); padding:2px 7px; border-radius:6px; }
</style>
</head>
<body>
<main>
  <header>
    <span class="brand">Talkee</span>
    <span class="pill" id="pill"><span class="dot"></span><span id="pilltext">Starting</span></span>
  </header>

  <section class="hero" id="hero">
    <div class="orb-wrap"><div class="orb" id="orb"></div></div>
    <h1 id="headline">{{HEADLINE}}</h1>
    <p class="sub" id="detail">{{DETAIL}}</p>
  </section>

  <section class="meters">
    <div class="meter"><label><span>From your phone</span></label><div class="bar"><i id="in"></i></div></div>
    <div class="meter"><label><span>Into the virtual microphone</span></label><div class="bar"><i id="out"></i></div></div>
  </section>

  <section class="panel" id="switches">
    <div class="row" data-k="paused" data-invert="1"><div><b>Microphone</b><span>Turn off to mute everywhere</span></div><div class="switch"></div></div>
    <div class="row" data-k="noise"><div><b>Noise suppression</b><span>Cleans up room noise. Loud music can clip quiet speech</span></div><div class="switch"></div></div>
    <div class="row" data-k="echo"><div><b>Echo cancellation</b><span>Removes this PC's speakers from your voice</span></div><div class="switch"></div></div>
    <div class="row" data-k="level"><div><b>Auto level</b><span>Lifts a quiet phone to a usable volume</span></div><div class="switch"></div></div>
  </section>

  <details id="pair"><summary>Pair a phone</summary>
    <div class="center">{{CODE}}</div>
    <p class="sub center" style="margin-top:10px">Open Talkee on the phone and point it at this code.</p>
  </details>

  <p class="hint">In Discord, Zoom or OBS, choose the microphone <code>CABLE Output</code></p>
</main>
<script>
  const TOKEN = "{{TOKEN}}";
  const $ = (id) => document.getElementById(id);
  let last = null, touchedAt = 0;

  async function refresh() {
    try {
      const s = await (await fetch('/status', { cache: 'no-store' })).json();
      last = s;
      const mode = s.problem ? 'error' : s.paused ? 'muted' : s.live ? 'live' : '';
      $('hero').className = 'hero ' + mode;
      $('pill').className = 'pill ' + mode;
      $('pilltext').textContent = s.problem ? 'Problem' : s.paused ? 'Muted'
        : s.live ? 'Live' : s.paired ? 'Connected' : 'Waiting';
      $('headline').textContent = s.paused ? 'Microphone is off' : s.headline;
      $('detail').textContent = s.paused ? 'Turn the microphone back on, here or on your phone.' : s.detail;
      const level = Math.min(1, s.levelIn * 1.6);
      $('orb').style.transform = 'scale(' + (1 + level * 0.7) + ')';
      $('orb').style.opacity = s.live && !s.paused ? 0.35 + level * 0.65 : 0.25;
      $('in').style.width = (s.paused ? 0 : Math.min(100, s.levelIn * 160)) + '%';
      $('out').style.width = (s.paused ? 0 : Math.min(100, s.levelOut * 160)) + '%';
      // Leave a switch alone for a moment after it is clicked, so a poll that was already in flight
      // cannot flick it back before the change lands.
      if (Date.now() - touchedAt > 600) {
        document.querySelectorAll('.row').forEach((row) => {
          const v = s[{paused:'paused',noise:'noise',echo:'echo',level:'autoLevel'}[row.dataset.k]];
          row.classList.toggle('on', row.dataset.invert ? !v : v);
        });
      }
      if (!s.hasCode) $('pair').style.display = 'none';
      else if (!window.__opened) { $('pair').open = !s.paired; window.__opened = true; }
    } catch (e) {
      $('pilltext').textContent = 'No contact';
    }
  }

  document.querySelectorAll('.row').forEach((row) => row.addEventListener('click', async () => {
    const on = !row.classList.contains('on');
    row.classList.toggle('on', on);
    touchedAt = Date.now();
    const value = row.dataset.invert ? !on : on;
    await fetch('/set?k=' + row.dataset.k + '&v=' + (value ? 1 : 0) + '&t=' + TOKEN, { method: 'POST' });
  }));

  refresh();
  setInterval(refresh, 150);
</script>
</body>
</html>
"##;

/// Escapes the five characters that change meaning inside HTML text.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Reads one whole HTTP response, so a test can assert on the status line and the body.
    fn fetch(wizard: &Wizard, path: &str) -> (String, String) {
        let mut stream =
            TcpStream::connect((Ipv4Addr::LOCALHOST, wizard.port)).expect("the wizard is listening");
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .expect("the request goes out");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("a response comes back");
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("a blank line separates them");
        let status = head.lines().next().unwrap_or("").to_string();
        (status, body.to_string())
    }

    fn state(cable: bool, paired: bool, live: bool) -> Snapshot {
        Snapshot {
            cable,
            paired,
            live,
            phone: Some("Pixel 8".to_string()),
            problem: None,
            ..Snapshot::default()
        }
    }

    #[test]
    fn every_state_says_what_to_do_about_it() {
        // The wizard exists to answer one question at a time. A state with no sentence, or two states
        // with the same sentence, is a wizard that tells nobody anything.
        let empty = Snapshot::default();
        assert_eq!(empty.headline(), "No virtual microphone");

        let searching = state(true, false, false);
        assert_eq!(searching.headline(), "Scan this code with your phone");
        assert!(
            searching.detail().contains("only have to do this once"),
            "the user is not told the pairing persists: {}",
            searching.detail()
        );

        let live = state(true, true, true);
        assert_eq!(live.headline(), "Your phone is your microphone");
        assert!(
            live.detail().contains("CABLE Output"),
            "the user is not told which microphone to pick elsewhere: {}",
            live.detail()
        );
        assert!(live.detail().contains("Pixel 8"), "the phone is not named");

        // Paired but silent is its own state, and the one that otherwise looks like "it works".
        let quiet = state(true, true, false);
        assert_eq!(quiet.headline(), "Connected — waiting for audio");
        assert!(quiet.detail().contains("muted"), "{}", quiet.detail());

        // A problem overrides the happy path, because it is the thing the user has to act on.
        let broken = Snapshot {
            problem: Some("no LAN address".to_string()),
            ..state(true, false, false)
        };
        assert_eq!(broken.headline(), "Something is wrong");
        assert_eq!(broken.detail(), "no LAN address");

        // A missing cable overrides everything: nothing else can work without it.
        let no_cable = Snapshot {
            problem: Some("ignored".to_string()),
            ..state(false, false, false)
        };
        assert_eq!(no_cable.headline(), "No virtual microphone");
    }

    #[test]
    fn the_status_json_is_a_document_a_page_can_parse() {
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");
        wizard.publish(state(true, true, true));
        wizard.set_link(Some("talkee://pair?v=1&h=1.2.3.4&p=47800&k=AA"));

        let (status, body) = fetch(&wizard, "/status");
        assert!(status.contains("200"), "{status}");

        // Parsed properly rather than string-matched: a document that is *nearly* JSON is a document
        // the browser reports as "lost contact with Talkee".
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("the status endpoint returns valid JSON");
        assert_eq!(parsed["cable"], true);
        assert_eq!(parsed["paired"], true);
        assert_eq!(parsed["live"], true);
        assert_eq!(parsed["hasCode"], true);
        assert_eq!(parsed["phone"], "Pixel 8");
        assert!(parsed["headline"].as_str().unwrap().contains("microphone"));
        // The link is deliberately absent: it is already on the page as a code, and a credential in
        // two places is a credential in a log.
        assert!(
            !body.contains("talkee://pair"),
            "the link leaked into the status: {body}"
        );
    }

    #[test]
    fn the_page_carries_the_code_and_updates_itself() {
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");
        wizard.publish(state(true, false, false));
        wizard.set_link(Some("talkee://pair?v=1&h=1.2.3.4&p=47800&k=AA"));

        let (status, body) = fetch(&wizard, "/");
        assert!(status.contains("200"), "{status}");
        assert!(body.contains("<svg"), "there is nothing to scan");
        assert!(body.contains("/status"), "the page cannot update itself");
        assert!(
            body.contains("Scan this code"),
            "the page does not say what to do"
        );

        // With no link there is no code, and the page says why rather than showing a blank card.
        wizard.set_link(None);
        let (_, body) = fetch(&wizard, "/");
        assert!(!body.contains("<svg"), "a code appeared with no link to encode");
        assert!(
            body.to_lowercase().contains("pairing is off"),
            "the page should say pairing is off: {body}"
        );
    }

    #[test]
    fn it_serves_only_what_it_means_to() {
        // Everything else is refused, including anything that is not a GET. This is a loopback socket
        // holding a credential, and the smallest possible surface is the whole point.
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");

        let (status, _) = fetch(&wizard, "/nope");
        assert!(status.contains("404"), "{status}");

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, wizard.port)).unwrap();
        stream
            .write_all(b"DELETE / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
        assert!(response.contains("405"), "{response}");
    }

    #[test]
    fn text_from_the_network_cannot_change_the_page() {
        // `problem` and the phone's name both come from somewhere else, and they land in a document that
        // also displays a key. Checked in two states, because a problem replaces the phone sentence —
        // so a single state can only prove one of them is escaped.
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");

        // A problem is what the page shows when there is one, so that is where the problem's text
        // reaches the document.
        wizard.publish(Snapshot {
            cable: true,
            problem: Some("<img src=x onerror=alert(1)>".to_string()),
            ..Snapshot::default()
        });
        let (_, body) = fetch(&wizard, "/");
        assert!(!body.contains("<img src=x"), "a problem injected markup");
        assert!(body.contains("&lt;img src=x"), "it should be shown as text");

        // And with no problem, the detail line names the phone — the other place foreign text lands.
        wizard.publish(Snapshot {
            cable: true,
            paired: true,
            live: true,
            phone: Some("<script>alert(1)</script>".to_string()),
            problem: None,
            ..Snapshot::default()
        });
        let (_, body) = fetch(&wizard, "/");
        assert!(!body.contains("<script>alert"), "a name injected markup");
        assert!(body.contains("&lt;script&gt;"), "it should be shown as text");

        // The JSON keeps the markup verbatim, which is correct: `<` needs no escape in JSON, and the
        // page never inserts this string as markup — it is assigned with `textContent`. What matters
        // is that the document still parses and the value survives intact.
        let (_, json) = fetch(&wizard, "/status");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("markup in a name must not break the JSON");
        assert_eq!(parsed["phone"], "<script>alert(1)</script>");
    }

    #[test]
    fn json_strings_escape_what_would_otherwise_break_the_document() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
        // A quote in a phone's name must not end the string early.
        let parsed: Result<serde_json::Value, _> =
            serde_json::from_str(&format!("{{\"n\":{}}}", json_string("Pixel \"8\"")));
        assert_eq!(parsed.expect("still valid JSON")["n"], "Pixel \"8\"");
    }

    /// Sends one request with any method, returning the status line.
    fn send(wizard: &Wizard, request_line: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, wizard.port)).unwrap();
        stream
            .write_all(format!("{request_line}
Host: localhost

").as_bytes())
            .unwrap();
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
        response.lines().next().unwrap_or("").to_string()
    }

    #[test]
    fn a_setting_changes_only_with_the_token_this_run_gave_the_page() {
        // The page is on loopback, but a browser will send a request there on behalf of any web page the
        // user has open. Without the token, one of those could mute the microphone.
        let controls = LiveControls::new(true, true, true);
        let wizard = Wizard::start_on(0, controls.clone()).expect("an ephemeral port");

        let status = send(&wizard, "POST /set?k=noise&v=0&t=wrong HTTP/1.1");
        assert!(status.contains("403"), "{status}");
        assert!(controls.get(Setting::NoiseSuppression), "a forged request changed a setting");

        let ok = format!("POST /set?k=noise&v=0&t={} HTTP/1.1", wizard.shared.token);
        assert!(send(&wizard, &ok).contains("200"));
        assert!(!controls.get(Setting::NoiseSuppression));

        // A GET must never change anything: a link or an image tag on another page issues one.
        let get = format!("GET /set?k=noise&v=1&t={} HTTP/1.1", wizard.shared.token);
        assert!(send(&wizard, &get).contains("404"));
        assert!(!controls.get(Setting::NoiseSuppression));
    }

    #[test]
    fn the_status_reports_the_live_settings() {
        let controls = LiveControls::new(true, false, true);
        controls.set(Setting::Paused, true);
        let wizard = Wizard::start_on(0, controls).expect("an ephemeral port");
        let (_, body) = fetch(&wizard, "/status");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["paused"], true);
        assert_eq!(parsed["noise"], true);
        assert_eq!(parsed["echo"], false);
        assert_eq!(parsed["autoLevel"], true);
    }
}
