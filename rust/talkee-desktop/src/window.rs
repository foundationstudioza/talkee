//! The Talkee window: the same switches as the tray and the phone, a live level, and the pairing code.
//!
//! # A separate process, on purpose
//!
//! The window is `talkee-app --window <port> <token>`, started by the tray and gone when it is closed.
//! That costs nothing while it is closed — no window, no GPU context, no memory — and it means the
//! tray's own loop never has to hand control to a GUI toolkit. It talks to the running app over the
//! loopback endpoints the app already serves (`/status`, `/set`, `/link`), so there is one source of
//! truth for what is on, and the phone hears about a change made here exactly as it does about one
//! made in the tray.
//!
//! # What it draws with
//!
//! `egui` with the OpenGL backend and nothing else: no browser engine, no web view. It repaints only
//! when the status changes or the user touches something.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, Pos2, Rect, Sense, Stroke, Vec2,
};
use qrcode::{Color as Module, QrCode};

use crate::wizard::Wizard;

const BG: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x10);
const PANEL: Color32 = Color32::from_rgb(0x12, 0x15, 0x1A);
const LINE: Color32 = Color32::from_rgb(0x1E, 0x23, 0x2B);
const TEXT: Color32 = Color32::from_rgb(0xE8, 0xE8, 0xEA);
const DIM: Color32 = Color32::from_rgb(0x8B, 0x8F, 0x98);
const FAINT: Color32 = Color32::from_rgb(0x4A, 0x4F, 0x58);
const GREEN: Color32 = Color32::from_rgb(0x4A, 0xDE, 0x80);
const AMBER: Color32 = Color32::from_rgb(0xFB, 0xBF, 0x24);
const RED: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);

/// Starts the window in its own process, unless one is already open.
///
/// The caller keeps the `Child` and passes it back in, so a second click on the tray item does not
/// stack a second window on top of the first.
pub fn spawn(wizard: &Wizard, existing: &mut Option<Child>) -> Result<()> {
    if let Some(child) = existing.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            return Ok(());
        }
    }
    let exe = std::env::current_exe()?;
    let child = Command::new(exe)
        .arg("--window")
        .arg(wizard.port().to_string())
        .arg(wizard.token())
        .spawn()
        .map_err(|e| anyhow!("could not start the window: {e}"))?;
    *existing = Some(child);
    Ok(())
}

/// What the app last said about itself.
#[derive(Debug, Clone, Default)]
struct Status {
    reachable: bool,
    live: bool,
    paired: bool,
    has_code: bool,
    paused: bool,
    noise: bool,
    echo: bool,
    auto_level: bool,
    level_in: f32,
    level_out: f32,
    headline: String,
    detail: String,
    phone: String,
    problem: String,
    /// The version of an update that is downloaded and waiting, or empty.
    update: String,
    /// A virtual microphone exists to play into.
    cable: bool,
    /// `allowed`, `blocked`, `not-asked` or `unknown`.
    firewall: String,
    /// The setup guide has been dismissed.
    guide_done: bool,
    /// About how long a word takes from the phone to the application, in ms; 0 when not known.
    delay_ms: u32,
}

impl Status {
    fn parse(body: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let flag = |k: &str| v[k].as_bool().unwrap_or(false);
        let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some(Self {
            reachable: true,
            live: flag("live"),
            paired: flag("paired"),
            has_code: flag("hasCode"),
            paused: flag("paused"),
            noise: flag("noise"),
            echo: flag("echo"),
            auto_level: flag("autoLevel"),
            level_in: v["levelIn"].as_f64().unwrap_or(0.0) as f32,
            level_out: v["levelOut"].as_f64().unwrap_or(0.0) as f32,
            headline: text("headline"),
            detail: text("detail"),
            phone: text("phone"),
            problem: text("problem"),
            update: text("update"),
            cable: flag("cable"),
            firewall: text("firewall"),
            guide_done: flag("guideDone"),
            delay_ms: v["delayMs"].as_u64().unwrap_or(0) as u32,
        })
    }
}

/// One loopback request. Returns the body of a 2xx response.
fn http(port: u16, method: &str, path: &str) -> Option<String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(800))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(800))).ok()?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    head.lines().next()?.contains(" 2").then(|| body.to_string())
}

struct Shared {
    status: Mutex<Status>,
    link: Mutex<Option<String>>,
}

struct App {
    port: u16,
    token: String,
    shared: Arc<Shared>,
    /// The QR modules, built once from the link: (width, dark?).
    qr: Option<(usize, Vec<bool>)>,
    /// A switch just clicked, shown as clicked until the app answers.
    touched: Option<(String, bool, Instant)>,
    show_code: Option<bool>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, port: u16, token: String) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = BG;
        visuals.override_text_color = Some(TEXT);
        cc.egui_ctx.set_visuals(visuals);

        let shared = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            link: Mutex::new(None),
        });
        // Polls the app a few times a second. Closing the window when the app has gone, rather than
        // leaving a dead panel on screen, is what keeps "I quit Talkee" meaning it.
        let worker = Arc::clone(&shared);
        let ctx = cc.egui_ctx.clone();
        let poll_token = token.clone();
        thread::spawn(move || {
            let mut misses = 0;
            loop {
                match http(port, "GET", "/status").and_then(|b| Status::parse(&b)) {
                    Some(status) => {
                        misses = 0;
                        if let Ok(mut held) = worker.status.lock() {
                            *held = status;
                        }
                        if worker.link.lock().map(|l| l.is_none()).unwrap_or(false) {
                            if let Some(link) = http(port, "GET", &format!("/link?t={poll_token}")) {
                                if let Ok(mut held) = worker.link.lock() {
                                    *held = Some(link);
                                }
                            }
                        }
                    }
                    None => {
                        misses += 1;
                        if misses >= 12 {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            return;
                        }
                    }
                }
                ctx.request_repaint();
                thread::sleep(Duration::from_millis(100));
            }
        });

        Self {
            port,
            token,
            shared,
            qr: None,
            touched: None,
            show_code: None,
        }
    }

    /// The value to draw for a switch: what was just clicked while waiting, else what the app says.
    fn shown(&self, key: &str, actual: bool) -> bool {
        match &self.touched {
            Some((k, v, at)) if k == key && at.elapsed() < Duration::from_millis(700) => *v,
            _ => actual,
        }
    }

    /// Asks the app to turn a row `on` or off. For the microphone row that means the opposite
    /// `paused` value, which is the only place the two senses meet.
    fn ask(&mut self, key: &str, on: bool) {
        self.touched = Some((key.to_string(), on, Instant::now()));
        let value = if key == "paused" { !on } else { on };
        let (port, path) = (
            self.port,
            format!("/set?k={key}&v={}&t={}", value as u8, self.token),
        );
        thread::spawn(move || {
            http(port, "POST", &path);
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let status = self.shared.status.lock().map(|s| s.clone()).unwrap_or_default();
        if self.qr.is_none() {
            if let Some(link) = self.shared.link.lock().ok().and_then(|l| l.clone()) {
                if let Ok(code) = QrCode::new(link.as_bytes()) {
                    let modules = code.to_colors().into_iter().map(|c| c == Module::Dark).collect();
                    self.qr = Some((code.width(), modules));
                }
            }
        }
        let open = *self.show_code.get_or_insert(!status.paired);

        egui::CentralPanel::default()
            .frame(Frame::default().fill(BG).inner_margin(Margin::symmetric(22, 18)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    header(ui, &status);
                    ui.add_space(10.0);
                    if let Some(action) = setup_card(ui, &status) {
                        let (port, path) = match action {
                            SetupAction::FixFirewall => (self.port, format!("/firewall-fix?t={}", self.token)),
                            SetupAction::Done => (self.port, format!("/set?k=guide&v=1&t={}", self.token)),
                            SetupAction::Reopen => (self.port, format!("/set?k=guide&v=0&t={}", self.token)),
                        };
                        thread::spawn(move || {
                            http(port, "POST", &path);
                        });
                    }
                    if !status.update.is_empty() && update_banner(ui, &status.update) {
                        let (port, path) = (self.port, format!("/update?t={}", self.token));
                        thread::spawn(move || {
                            http(port, "POST", &path);
                        });
                    }
                    hero(ui, &status, self.qr.is_some());
                    ui.add_space(16.0);
                    meter(ui, "From your phone", if status.paused { 0.0 } else { status.level_in });
                    ui.add_space(10.0);
                    meter(ui, "Into the virtual microphone", if status.paused { 0.0 } else { status.level_out });
                    if status.delay_ms > 0 && status.paired {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!("About {} ms from your phone to your apps", status.delay_ms))
                                .color(FAINT)
                                .size(12.0),
                        );
                    }
                    ui.add_space(20.0);

                    // (setting, title, subtitle, is it on). The microphone row reads "on" when audio
                    // flows, which is the inverse of the `paused` setting behind it.
                    let rows = [
                        ("paused", "Microphone", "Turn off to mute everywhere", !status.paused),
                        ("noise", "Noise suppression", "Cleans up room noise. Loud music can clip quiet speech", status.noise),
                        ("echo", "Echo cancellation", "Removes this PC's speakers from your voice", status.echo),
                        ("level", "Auto level", "Lifts a quiet phone to a usable volume", status.auto_level),
                    ];
                    Frame::default()
                        .fill(PANEL)
                        .stroke(Stroke::new(1.0_f32, LINE))
                        .corner_radius(CornerRadius::same(14))
                        .show(ui, |ui| {
                            for (i, (key, title, sub, actual)) in rows.iter().enumerate() {
                                if i > 0 {
                                    let y = ui.cursor().top();
                                    ui.painter().hline(ui.min_rect().x_range(), y, Stroke::new(1.0_f32, LINE));
                                }
                                let shown = self.shown(key, *actual);
                                if switch_row(ui, title, sub, shown) {
                                    self.ask(key, !shown);
                                }
                            }
                        });

                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        // For a connection that looks fine and carries nothing: drops the phone's
                        // connection, and it pairs again by itself within a few seconds.
                        if ui
                            .add(egui::Button::new(egui::RichText::new("Reconnect phone").color(DIM).size(13.0)).fill(PANEL))
                            .clicked()
                        {
                            let (port, path) = (self.port, format!("/reconnect?t={}", self.token));
                            thread::spawn(move || {
                                http(port, "POST", &path);
                            });
                        }
                    });
                    ui.add_space(8.0);
                    if status.has_code {
                        let label = if open { "Hide pairing code" } else { "Pair a phone" };
                        if ui
                            .add(egui::Label::new(egui::RichText::new(label).color(DIM)).sense(Sense::click()))
                            .clicked()
                        {
                            self.show_code = Some(!open);
                        }
                        if open {
                            ui.add_space(8.0);
                            if let Some((width, modules)) = &self.qr {
                                ui.vertical_centered(|ui| qr(ui, *width, modules));
                                ui.add_space(6.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(egui::RichText::new("Open Talkee on the phone and point it at this code.").color(DIM).size(13.0));
                                });
                            }
                        }
                    }
                    ui.add_space(18.0);
                    if status.guide_done {
                        ui.vertical_centered(|ui| {
                            if ui
                                .add(egui::Label::new(egui::RichText::new("Show the setup guide").color(FAINT).size(12.0)).sense(Sense::click()))
                                .clicked()
                            {
                                let (port, path) = (self.port, format!("/set?k=guide&v=0&t={}", self.token));
                                thread::spawn(move || {
                                    http(port, "POST", &path);
                                });
                            }
                        });
                    }
                });
            });
    }
}

/// What the setup card asks for.
enum SetupAction {
    /// Repair the firewall rules (Windows asks for permission).
    FixFirewall,
    /// Hide the guide.
    Done,
    /// Show it again.
    Reopen,
}

/// One line of the checklist: a mark, a title, and what to do about it.
fn step(ui: &mut egui::Ui, mark: &str, colour: Color32, title: &str, detail: &str) {
    ui.horizontal_top(|ui| {
        // Drawn rather than typed: the built-in font has no check mark, and a missing glyph shows as
        // an empty box — which reads as "not done" on the one line that means "done".
        let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 20.0), Sense::hover());
        let centre = Pos2::new(rect.center().x, rect.top() + 10.0);
        match mark {
            "•" => {
                ui.painter().circle_stroke(centre, 4.5, Stroke::new(1.5_f32, colour));
            }
            _ => {
                ui.painter().circle_filled(centre, 5.0, colour);
            }
        }
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(13.5));
            if !detail.is_empty() {
                ui.label(egui::RichText::new(detail).color(DIM).size(12.0));
            }
        });
    });
    ui.add_space(6.0);
}

/// The first-run checklist.
///
/// Shown until the person says they are done, and again at any time a step goes wrong — a firewall
/// that has started blocking, a virtual microphone that has vanished — because those are the moments
/// the guide is for. Each step says whether it is fine; the last is the one the app cannot check, which
/// is choosing the microphone in the other program, so it has the name ready to copy.
fn setup_card(ui: &mut egui::Ui, status: &Status) -> Option<SetupAction> {
    let blocked = status.firewall == "blocked";
    let problem = blocked || !status.cable;
    if status.guide_done && !problem {
        return None;
    }
    let mut action = None;
    Frame::default()
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, if problem { AMBER.gamma_multiply(0.5) } else { LINE }))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Set up").size(15.0).strong());
            ui.add_space(8.0);

            if status.cable {
                step(ui, "✓", GREEN, "Virtual microphone installed", "");
            } else {
                step(ui, "!", AMBER, "No virtual microphone", "Choose “Install VB-CABLE” in the tray menu, then restart Windows.");
            }

            match status.firewall.as_str() {
                "allowed" => step(ui, "✓", GREEN, "Windows lets your phone reach Talkee", ""),
                "blocked" => {
                    step(
                        ui,
                        "!",
                        AMBER,
                        "Windows is blocking Talkee",
                        "Your phone cannot connect until this is fixed. It usually means the permission prompt was dismissed.",
                    );
                    if ui
                        .add(egui::Button::new(egui::RichText::new("Fix it (Windows will ask permission)").color(BG).size(12.5)).fill(AMBER))
                        .clicked()
                    {
                        action = Some(SetupAction::FixFirewall);
                    }
                    ui.add_space(6.0);
                }
                _ => step(
                    ui,
                    "•",
                    DIM,
                    "Allow Talkee through Windows Firewall",
                    "When Windows asks, click “Allow access” (private networks is enough).",
                ),
            }

            if status.paired {
                step(ui, "✓", GREEN, "Phone paired", "");
            } else {
                step(ui, "•", DIM, "Pair your phone", "Open Talkee on your phone and scan the code below.");
            }

            step(
                ui,
                "•",
                DIM,
                "Choose the microphone in your apps",
                "In Discord, Zoom, OBS or Claude, pick the microphone named:",
            );
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("CABLE Output").monospace().size(13.0));
                if ui.add(egui::Button::new(egui::RichText::new("Copy").size(12.0).color(DIM)).fill(BG)).clicked() {
                    ui.ctx().copy_text("CABLE Output".to_string());
                }
            });
            ui.add_space(10.0);
            if status.guide_done {
                if ui.add(egui::Button::new(egui::RichText::new("Show later").size(12.0).color(DIM)).fill(BG)).clicked() {
                    action = Some(SetupAction::Reopen);
                }
            } else if ui
                .add(egui::Button::new(egui::RichText::new("I've done this").size(12.5).color(BG)).fill(GREEN))
                .clicked()
            {
                action = Some(SetupAction::Done);
            }
        });
    ui.add_space(12.0);
    action
}

/// A quiet strip offering the downloaded update. Returns true when it was clicked.
fn update_banner(ui: &mut egui::Ui, version: &str) -> bool {
    let mut clicked = false;
    Frame::default()
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, GREEN.gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Update {version} is ready")).size(13.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    clicked = ui
                        .add(egui::Button::new(egui::RichText::new("Restart to update").color(BG).size(12.5)).fill(GREEN))
                        .clicked();
                });
            });
        });
    ui.add_space(10.0);
    clicked
}

fn mode_colour(status: &Status) -> Color32 {
    if !status.problem.is_empty() || status.paused {
        RED
    } else if status.live {
        GREEN
    } else {
        AMBER
    }
}

fn header(ui: &mut egui::Ui, status: &Status) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Talkee").strong().size(16.0));
        if !status.phone.is_empty() {
            ui.label(egui::RichText::new(&status.phone).color(FAINT).size(12.0));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let word = if !status.reachable {
                "No contact"
            } else if !status.problem.is_empty() {
                "Problem"
            } else if status.paused {
                "Muted"
            } else if status.live {
                "Live"
            } else if status.paired {
                "Connected"
            } else {
                "Waiting"
            };
            ui.label(egui::RichText::new(word).color(DIM).size(13.0));
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, mode_colour(status));
        });
    });
}

fn hero(ui: &mut egui::Ui, status: &Status, _has_code: bool) {
    let accent = mode_colour(status);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::hover());
    let level = if status.live && !status.paused {
        (status.level_in * 1.6).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let radius = 40.0 + level * 26.0;
    let alpha = 0.18 + level * 0.5;
    ui.painter().circle_filled(rect.center(), radius, accent.gamma_multiply(alpha));
    ui.painter().circle_stroke(rect.center(), radius, Stroke::new(1.5_f32, accent.gamma_multiply(0.55)));

    let headline = if status.paused { "Microphone is off" } else { status.headline.as_str() };
    let detail = if status.paused {
        "Turn the microphone back on, here or on your phone."
    } else {
        status.detail.as_str()
    };
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(headline).size(22.0).color(TEXT));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(detail).size(13.0).color(DIM));
    });
}

fn meter(ui: &mut egui::Ui, label: &str, level: f32) {
    ui.label(egui::RichText::new(label).size(12.0).color(DIM));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    let radius = CornerRadius::same(3);
    ui.painter().rect_filled(rect, radius, PANEL);
    let filled = Rect::from_min_size(rect.min, Vec2::new(rect.width() * (level * 1.6).clamp(0.0, 1.0), rect.height()));
    ui.painter().rect_filled(filled, radius, GREEN);
}

/// One row with a switch on the right. Returns true when it was clicked.
fn switch_row(ui: &mut egui::Ui, title: &str, sub: &str, on: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), Sense::click());
    let painter = ui.painter();
    painter.text(rect.left_top() + Vec2::new(16.0, 10.0), Align2::LEFT_TOP, title, FontId::proportional(15.0), TEXT);
    painter.text(rect.left_top() + Vec2::new(16.0, 32.0), Align2::LEFT_TOP, sub, FontId::proportional(11.5), DIM);

    let t = ui.ctx().animate_bool(response.id, on);
    let track = Rect::from_center_size(Pos2::new(rect.right() - 16.0 - 21.0, rect.center().y), Vec2::new(42.0, 24.0));
    let off = Color32::from_rgb(0x2A, 0x2F, 0x38);
    let colour = Color32::from_rgb(
        (off.r() as f32 + (GREEN.r() as f32 - off.r() as f32) * t) as u8,
        (off.g() as f32 + (GREEN.g() as f32 - off.g() as f32) * t) as u8,
        (off.b() as f32 + (GREEN.b() as f32 - off.b() as f32) * t) as u8,
    );
    painter.rect_filled(track, CornerRadius::same(12), colour);
    let knob_x = track.left() + 12.0 + 18.0 * t;
    painter.circle_filled(Pos2::new(knob_x, track.center().y), 9.0, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Draws the pairing code: dark modules on a white card, with the quiet zone a camera needs.
fn qr(ui: &mut egui::Ui, width: usize, modules: &[bool]) {
    let cell = 5.0;
    let quiet = 4.0;
    let side = (width as f32 + quiet * 2.0) * cell;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(12), Color32::WHITE);
    for (i, dark) in modules.iter().enumerate() {
        if *dark {
            let (x, y) = ((i % width) as f32, (i / width) as f32);
            let min = rect.min + Vec2::new((x + quiet) * cell, (y + quiet) * cell);
            ui.painter().rect_filled(Rect::from_min_size(min, Vec2::splat(cell)), CornerRadius::ZERO, Color32::from_rgb(0x0B, 0x0D, 0x10));
        }
    }
}

/// Runs the window until it is closed. `port` and `token` are the running app's.
pub fn run(port: u16, token: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Talkee")
            .with_inner_size([420.0, 760.0])
            .with_min_inner_size([360.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Talkee",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, port, token)))),
    )
    .map_err(|e| anyhow!("the window failed: {e}"))
}
