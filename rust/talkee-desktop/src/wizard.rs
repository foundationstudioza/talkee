//! The app's local endpoint: what the Talkee window reads and changes.
//!
//! The window (`window.rs`) is a separate process, so it talks to the running app over a tiny HTTP
//! server on loopback: `/status` for what is true now, `/link` for the pairing code, and `POST` routes
//! for the switches, reconnect, the firewall fix and the update. One source of truth for what is on,
//! whether it was changed from the window, the tray or the phone.
//!
//! # Bound to loopback, and that is not a detail
//!
//! `/link` hands out the pairing key. Anyone who can fetch it can speak through this microphone, so it
//! is served on `127.0.0.1` and nowhere else, and every route that reveals or changes anything needs the
//! per-run token the window is given on its command line.
//!
//! # Deliberately small
//!
//! There is no router, no keep-alive and no chunked encoding. It reads one request line, answers it
//! and closes.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use talkee_core::live::{LiveControls, Setting};

/// Where the wizard listens. Loopback only: see the module docs.
pub const PORT: u16 = 47_802;

/// How long a client has to send its request line before it is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What the window says about the app right now.
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
    /// About how long a word takes from the phone to the application, in ms; 0 when not known yet.
    pub delay_ms: u32,
}

impl Snapshot {
    /// The one sentence the window shows.
    ///
    /// Ordered by what the user can act on: a missing cable beats everything because nothing else can
    /// work without it, and a problem beats the happy path because it is the thing they have to do
    /// something about. Written as a chain rather than one big match because the match is where an
    /// uncovered combination hides — and an uncovered combination here is a window with no sentence in it.
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
    /// What the firewall says about this program, refreshed by the main thread.
    firewall: Mutex<crate::firewall::State>,
    /// The updater, once the app has one. The window's "restart to update" button reaches it here.
    updater: Mutex<Option<crate::update::Updater>>,
    /// A random value the window is handed on its command line and every request must repeat.
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
    /// The port is fixed rather than chosen by the OS so it is easy to find in a log. A port already in
    /// use is reported rather than worked around: another copy of this app holding it is the likely
    /// reason, and that is worth saying.
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
            firewall: Mutex::new(crate::firewall::State::Unknown),
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
                            // One thread per request, detached, so the accept loop never waits on a
                            // slow or abandoned connection.
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

    /// Tells the window what the firewall says.
    pub fn set_firewall(&self, state: crate::firewall::State) {
        if let Ok(mut held) = self.shared.firewall.lock() {
            *held = state;
        }
    }

    /// Gives the wizard the updater, so the window can show an update and start it.
    pub fn set_updater(&self, updater: crate::update::Updater) {
        if let Ok(mut held) = self.shared.updater.lock() {
            *held = Some(updater);
        }
    }

    /// The port the window talks to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The value a request must repeat to change a setting or read the pairing link.
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// Tells the window what is true now.
    ///
    /// Called from the main loop on every tick. Publishing the same value repeatedly is free: the window
    /// polls, so there is no push to do work for.
    pub fn publish(&self, snapshot: Snapshot) {
        if let Ok(mut held) = self.shared.state.lock() {
            *held = snapshot;
        }
    }

    /// Sets or clears the pairing link the window shows as a code.
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
        ("GET", "/status") => ("200 OK", "application/json", status_json(shared)),
        ("POST", "/set") => change_setting(shared, query),
        ("GET", "/link") => read_link(shared, query),
        ("POST", "/update") => start_update(shared, query),
        ("POST", "/firewall-fix") => {
            let token = query.split('&').find_map(|pair| pair.strip_prefix("t=")).unwrap_or("");
            if token != shared.token {
                ("403 Forbidden", "text/plain; charset=utf-8", "bad token".to_string())
            } else {
                // Asks Windows for administrator rights; the person sees the standard prompt.
                match crate::firewall::fix() {
                    Ok(()) => ("200 OK", "application/json", "{}".to_string()),
                    Err(e) => ("500 Internal Server Error", "text/plain; charset=utf-8", e.to_string()),
                }
            }
        }
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
            "not found".to_string(),
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
/// Behind the token because the link carries the pairing key, so only a client that was handed the
/// token on its command line can read it.
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
        "guide" => Setting::GuideDone,
        _ => {
            return ("400 Bad Request", "text/plain; charset=utf-8", "unknown setting".to_string())
        }
    };
    shared.controls.set(setting, value == "1");
    ("200 OK", "application/json", "{}".to_string())
}

/// The current state as JSON, for the window to poll.
/// The link is *not* included: it has its own token-guarded route, and repeating a credential in a
/// second place is how it ends up in a log.
fn status_json(shared: &Arc<Shared>) -> String {
    let snapshot = shared.state.lock().map(|s| s.clone()).unwrap_or_default();
    let has_code = shared.link.lock().map(|l| l.is_some()).unwrap_or(false);
    let c = &shared.controls;
    format!(
        "{{\"cable\":{},\"paired\":{},\"live\":{},\"hasCode\":{},\"headline\":{},\"detail\":{},\"phone\":{},\"problem\":{},\
\"levelIn\":{:.3},\"levelOut\":{:.3},\"paused\":{},\"noise\":{},\"echo\":{},\"autoLevel\":{},\"update\":{},\"firewall\":{},\"guideDone\":{},\"delayMs\":{}}}",
        snapshot.cable,
        // Paired means a phone is connected, not that audio happens to be flowing this instant: a muted
        // phone sends nothing and is still very much paired.
        snapshot.paired || c.phone().is_some(),
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
        json_string(
            shared
                .firewall
                .lock()
                .map(|f| f.as_str())
                .unwrap_or("unknown")
        ),
        c.get(Setting::GuideDone),
        snapshot.delay_ms,
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
    fn the_status_json_is_a_document_the_window_can_parse() {
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");
        wizard.publish(state(true, true, true));
        wizard.set_link(Some("talkee://pair?v=1&h=1.2.3.4&p=47800&k=AA"));

        let (status, body) = fetch(&wizard, "/status");
        assert!(status.contains("200"), "{status}");

        // Parsed properly rather than string-matched: a document that is *nearly* JSON is a document
        // the window reports as "lost contact with Talkee".
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("the status endpoint returns valid JSON");
        assert_eq!(parsed["cable"], true);
        assert_eq!(parsed["paired"], true);
        assert_eq!(parsed["live"], true);
        assert_eq!(parsed["hasCode"], true);
        assert_eq!(parsed["phone"], "Pixel 8");
        assert!(parsed["headline"].as_str().unwrap().contains("microphone"));
        // The link is deliberately absent: it has its own guarded route, and a credential in
        // two places is a credential in a log.
        assert!(
            !body.contains("talkee://pair"),
            "the link leaked into the status: {body}"
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
    fn text_from_the_network_survives_the_status_intact() {
        // The phone's name comes from somewhere else. The window draws it as text, so what matters is
        // that the document still parses and the value arrives unchanged.
        let wizard = Wizard::start_on(0, LiveControls::new(true, true, true)).expect("an ephemeral port");
        wizard.publish(Snapshot {
            cable: true,
            paired: true,
            live: true,
            phone: Some("<script>\"x\"</script>".to_string()),
            problem: None,
            ..Snapshot::default()
        });
        let (_, json) = fetch(&wizard, "/status");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("markup in a name must not break the JSON");
        assert_eq!(parsed["phone"], "<script>\"x\"</script>");
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
    fn a_setting_changes_only_with_the_token_this_run_gave_the_window() {
        // The server is on loopback, but a browser will send a request there on behalf of any web page the
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
