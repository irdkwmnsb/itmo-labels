//! The "Server mode" box of the main window: keeps the web server running on the chosen
//! port while the checkbox is on, and shows where to reach it and what it printed.

use std::net::Ipv4Addr;

use eframe::egui::{self, Color32, RichText, TextEdit, Ui, Vec2};
use web_time::{Duration, Instant};

use crate::{
    config::{self, State},
    i18n::Tr,
    printer::{JobRecord, Origin, Service},
    server::{self, Server},
};

const RETRY: Duration = Duration::from_secs(5);
const ADDRESSES_TTL: Duration = Duration::from_secs(5);
const QR_SIDE: f32 = 168.0;

pub struct Serving {
    service: Service,
    server: Option<Server>,
    /// Port, error and when; tried again after `RETRY` or with another port.
    failed: Option<(u16, String, Instant)>,
    port_text: String,
    addresses: Vec<Ipv4Addr>,
    addresses_at: Option<Instant>,
    /// The address in the QR code, when the user picked one other than the first.
    picked: Option<Ipv4Addr>,
    /// URL, modules per side, dark modules row by row.
    qr: Option<(String, usize, Vec<bool>)>,
}

impl Serving {
    pub fn new(service: Service, state: &State) -> Self {
        Self {
            service,
            server: None,
            failed: None,
            port_text: state.port().to_string(),
            addresses: Vec::new(),
            addresses_at: None,
            picked: None,
            qr: None,
        }
    }

    /// Starts, restarts or stops the server to match the checkbox and the port.
    pub fn sync(&mut self, state: &mut State) {
        let wanted = state.server.then(|| state.port());
        if self.server.as_ref().map(Server::port) != wanted {
            self.server = None;
        }
        let Some(port) = wanted else {
            self.failed = None;
            return;
        };
        let retry = self.failed.as_ref().is_none_or(|(p, _, at)| *p != port || at.elapsed() >= RETRY);
        if self.server.is_none() && retry {
            match Server::start(port, self.service.clone(), config::dir()) {
                Ok(s) => {
                    self.server = Some(s);
                    self.failed = None;
                    self.addresses_at = None;
                }
                Err(e) => self.failed = Some((port, e, Instant::now())),
            }
        }
    }

    pub fn controls(&mut self, ui: &mut Ui, t: &Tr, state: &mut State) {
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut state.server, t.server);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let field = ui.add(TextEdit::singleline(&mut self.port_text).desired_width(52.0));
                ui.label(t.port);
                // Enter also ends the edit; a port that does not parse goes back to the old one.
                if field.lost_focus() {
                    if let Ok(port) = self.port_text.trim().parse::<u16>()
                        && port != 0
                    {
                        state.port = (port != config::DEFAULT_PORT).then_some(port);
                    }
                    self.port_text = state.port().to_string();
                }
            });
        });
        if !state.server {
            return;
        }
        if let Some((_, e, _)) = &self.failed {
            ui.colored_label(ui.visuals().error_fg_color, format!("{} {e}", t.server_failed));
        }
        if self.server.is_some() {
            self.addresses(ui, t);
        }
        ui.add_space(8.0);
        ui.label(RichText::new(t.jobs).strong());
        let log = self.service.log();
        if log.is_empty() {
            ui.weak(t.no_jobs);
        }
        for job in &log {
            job_line(ui, t, job);
        }
    }

    fn addresses(&mut self, ui: &mut Ui, t: &Tr) {
        if self.addresses_at.is_none_or(|at| at.elapsed() >= ADDRESSES_TTL) {
            self.addresses = server::addresses();
            self.addresses_at = Some(Instant::now());
        }
        let Some(&first) = self.addresses.first() else {
            ui.colored_label(ui.visuals().warn_fg_color, t.no_addresses);
            return;
        };
        let shown = self.picked.filter(|ip| self.addresses.contains(ip)).unwrap_or(first);
        let url = self.url(shown);
        ui.label(t.server_open);
        if ui.link(RichText::new(&url).size(16.0).strong()).clicked() {
            open_url(&url);
        }
        if self.qr.as_ref().is_none_or(|(u, ..)| *u != url) {
            self.qr = qr_modules(&url).map(|(width, dark)| (url.clone(), width, dark));
        }
        if let Some((_, width, dark)) = &self.qr {
            ui.vertical_centered(|ui| paint_qr(ui, *width, dark));
        }
        if self.addresses.len() > 1 {
            ui.label(t.other_addresses);
            for ip in self.addresses.clone() {
                if ui.selectable_label(ip == shown, self.url(ip)).clicked() {
                    self.picked = Some(ip);
                }
            }
        }
    }

    fn url(&self, ip: Ipv4Addr) -> String {
        format!("http://{ip}:{}/", self.server.as_ref().map_or(0, Server::port))
    }
}

fn qr_modules(url: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let dark = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Some((code.width(), dark))
}

fn paint_qr(ui: &mut Ui, width: usize, dark: &[bool]) {
    // Four modules of quiet zone around the code, as scanners expect.
    let side = width + 8;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(QR_SIDE), egui::Sense::hover());
    let module = rect.width() / side as f32;
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::WHITE);
    for (i, _) in dark.iter().enumerate().filter(|(_, d)| **d) {
        let (x, y) = ((i % width + 4) as f32, (i / width + 4) as f32);
        let min = rect.min + Vec2::new(x, y) * module;
        painter.rect_filled(egui::Rect::from_min_size(min, Vec2::splat(module)), 0.0, Color32::BLACK);
    }
}

fn job_line(ui: &mut Ui, t: &Tr, job: &JobRecord) {
    let from = match job.origin {
        Origin::Local => t.this_device.to_owned(),
        Origin::Remote(ip) => ip.to_string(),
    };
    let size = format!("{:.1} {}", job.bytes as f32 / 1024.0, t.kb);
    let head = format!("{} · {from} · {} · {size} · ", job.at.format("%H:%M:%S"), job.printer);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(RichText::new(head).small());
        match &job.result {
            Ok(()) => ui.label(RichText::new(t.job_sent).small()),
            Err(e) => ui.label(RichText::new(e).small().color(ui.visuals().error_fg_color)),
        };
    });
}

/// In the default browser, like a link on a web page.
fn open_url(url: &str) {
    use std::process::Command;
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
}
