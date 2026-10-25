//! The browser side of server mode: the same app, with the printers, `sizes.toml` and
//! `templates.toml` of the machine that served the page, reached over its HTTP API
//! (see `server`), and the state in local storage.

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use eframe::egui;
use web_time::{Duration, Instant};

use crate::{
    config::{self, Size, Template},
    printer::{Health, Snapshot, Status},
    query,
};

pub const STATE_KEY: &str = "itmo-labels-state";
const PRINTERS_POLL: Duration = Duration::from_secs(2);
const FILE_POLL: Duration = Duration::from_secs(1);

pub fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The response body, or why there is none: the server sends its errors as plain text.
fn body(result: ehttp::Result<ehttp::Response>) -> Result<String, String> {
    let response = result?;
    let text = response.text().unwrap_or_default().to_owned();
    if response.ok {
        Ok(text)
    } else if text.is_empty() {
        Err(format!("{} {}", response.status, response.status_text))
    } else {
        Err(text)
    }
}

#[derive(Default)]
struct Remote {
    snapshot: Snapshot,
    polled: Option<Instant>,
    in_flight: bool,
    /// Bumped on every selection change, so a late reply about the previous printer is dropped.
    generation: u64,
}

/// This tab's view of the server's printers: its own selection and its own last job.
pub struct Printers {
    selected: Option<String>,
    remote: Arc<Mutex<Remote>>,
    ctx: egui::Context,
}

impl Printers {
    pub fn new(ctx: &egui::Context, selected: Option<String>) -> Self {
        Self { selected, remote: Arc::default(), ctx: ctx.clone() }
    }

    /// Also polls the server when the last poll is old enough.
    pub fn snapshot(&self) -> Snapshot {
        let mut r = self.remote.lock().unwrap();
        if !r.in_flight && r.polled.is_none_or(|t| t.elapsed() >= PRINTERS_POLL) {
            r.in_flight = true;
            let url = match &self.selected {
                Some(name) => format!("api/status?printer={}", query::encode(name)),
                None => "api/status".to_owned(),
            };
            let (remote, ctx, generation) = (self.remote.clone(), self.ctx.clone(), r.generation);
            ehttp::fetch(ehttp::Request::get(url), move |result| {
                let status = body(result).and_then(|text| serde_json::from_str::<Status>(&text).map_err(|e| e.to_string()));
                let mut r = remote.lock().unwrap();
                r.in_flight = false;
                r.polled = Some(Instant::now());
                if r.generation == generation {
                    match status {
                        Ok(status) => {
                            r.snapshot.printers = status.printers;
                            r.snapshot.health = status.health;
                        }
                        Err(e) => r.snapshot.health = Some(Health::Unavailable(e)),
                    }
                }
                drop(r);
                ctx.request_repaint();
            });
        }
        r.snapshot.clone()
    }

    pub fn select(&mut self, name: Option<String>) {
        self.selected = name;
        let mut r = self.remote.lock().unwrap();
        r.generation += 1;
        r.snapshot.health = None;
        r.polled = None;
    }

    pub fn print(&self, job: Vec<u8>) {
        let Some(printer) = &self.selected else { return };
        self.remote.lock().unwrap().snapshot.busy_printing = true;
        let url = format!("api/print?printer={}", query::encode(printer));
        let (remote, ctx) = (self.remote.clone(), self.ctx.clone());
        ehttp::fetch(ehttp::Request::post(url, job), move |result| {
            let mut r = remote.lock().unwrap();
            r.snapshot.busy_printing = false;
            r.snapshot.last_job = Some(body(result).map(drop));
            r.polled = None;
            drop(r);
            ctx.request_repaint();
        });
    }
}

#[derive(Default)]
struct FileState {
    /// The text last turned into items, to tell a change from a repeat.
    text: Option<String>,
    arrived: Option<Result<String, String>>,
    in_flight: bool,
    /// A save the server turned down, until the editor picks it up.
    failed: Option<String>,
}

/// One of the server's config files, re-fetched every second; the same contract as `WatchedFile`.
pub struct RemoteFile<T> {
    url: &'static str,
    parse: fn(&str) -> Result<T, String>,
    state: Arc<Mutex<FileState>>,
    checked: Option<Instant>,
    ctx: egui::Context,
    _items: PhantomData<T>,
}

impl<T> RemoteFile<T> {
    fn new(ctx: &egui::Context, url: &'static str, parse: fn(&str) -> Result<T, String>) -> std::io::Result<Self> {
        Ok(Self { url, parse, state: Arc::default(), checked: None, ctx: ctx.clone(), _items: PhantomData })
    }

    /// The new contents (or why they are unavailable) when they changed since the last call.
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        let mut s = self.state.lock().unwrap();
        if !s.in_flight && self.checked.is_none_or(|t| t.elapsed() >= FILE_POLL) {
            self.checked = Some(Instant::now());
            s.in_flight = true;
            let (state, ctx) = (self.state.clone(), self.ctx.clone());
            ehttp::fetch(ehttp::Request::get(self.url), move |result| {
                let mut s = state.lock().unwrap();
                s.in_flight = false;
                s.arrived = Some(body(result));
                drop(s);
                ctx.request_repaint();
            });
        }
        match s.arrived.take()? {
            Ok(text) if s.text.as_ref() == Some(&text) => None,
            Ok(text) => {
                let parsed = (self.parse)(&text);
                s.text = Some(text);
                Some(parsed)
            }
            Err(e) => {
                s.text = None;
                Some(Err(e))
            }
        }
    }

    /// Files on the server cannot be opened in a text editor from here.
    pub fn path(&self) -> Option<&std::path::Path> {
        None
    }
}

impl RemoteFile<Vec<Template>> {
    pub fn writer(&self) -> TemplatesWriter {
        self.state.lock().unwrap().failed = None;
        TemplatesWriter { state: self.state.clone(), ctx: self.ctx.clone() }
    }
}

/// Saves the template editor's draft to the server's `templates.toml`.
pub struct TemplatesWriter {
    state: Arc<Mutex<FileState>>,
    ctx: egui::Context,
}

impl TemplatesWriter {
    /// Validates and sends; `Ok` means sent, a refusal shows up later in `failed`.
    pub fn write(&self, templates: &[Template]) -> Result<(), String> {
        let text = config::format_templates(templates)?;
        let (state, ctx) = (self.state.clone(), self.ctx.clone());
        ehttp::fetch(ehttp::Request::put("api/templates", text.clone().into_bytes()), move |result| {
            let mut s = state.lock().unwrap();
            match body(result) {
                // What the server now holds: the next poll of the same text is not a change.
                Ok(_) => s.text = Some(text),
                // Forget the last text, so the next poll puts the server's templates back.
                Err(e) => {
                    s.text = None;
                    s.failed = Some(e);
                }
            }
            drop(s);
            ctx.request_repaint();
        });
        Ok(())
    }

    pub fn failed(&self) -> Option<String> {
        self.state.lock().unwrap().failed.take()
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        None
    }
}

pub fn sizes_file(ctx: &egui::Context) -> std::io::Result<RemoteFile<Vec<Size>>> {
    RemoteFile::new(ctx, "api/sizes", config::parse_sizes)
}

pub fn templates_file(ctx: &egui::Context) -> std::io::Result<RemoteFile<Vec<Template>>> {
    RemoteFile::new(ctx, "api/templates", config::parse_templates)
}
