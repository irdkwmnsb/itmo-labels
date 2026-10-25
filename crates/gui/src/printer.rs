//! Printer state as the UI sees it, and (native only) the background thread that owns all
//! printer I/O: rescans every 2 s, polls the status of every printer someone is looking at,
//! and runs print jobs in between, so neither the UI nor the web server blocks on USB.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Health {
    Ready,
    Printing,
    NoPaper,
    HeadOpen,
    PaperJam,
    Paused,
    /// Status bits we have no better name for.
    Error(u8),
    /// The printer did not answer `<ESC>!?`.
    NoReply,
    /// Someone else (CUPS, the spooler) has it open.
    Busy,
    /// Present but could not be opened, e.g. no permission.
    Unavailable(String),
    /// The selected printer is not connected.
    Disconnected,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub printers: Vec<String>,
    /// Of the selected printer; `None` until the first poll.
    pub health: Option<Health>,
    pub busy_printing: bool,
    /// Outcome of the last print job.
    pub last_job: Option<Result<(), String>>,
}

/// What the web server answers to a status poll.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub printers: Vec<String>,
    pub health: Option<Health>,
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use crate::web::Printers;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{
        collections::{HashMap, VecDeque},
        io,
        net::IpAddr,
        sync::{
            Arc, Mutex,
            mpsc::{self, RecvTimeoutError, Sender},
        },
        thread,
        time::{Duration, Instant},
    };

    use itmo_labels_core::{
        transport::{self, Found, Transport},
        tspl,
    };

    use super::{Health, Snapshot, Status};

    const POLL: Duration = Duration::from_secs(2);
    /// A printer nobody has asked about for this long is no longer polled.
    const WATCH: Duration = Duration::from_secs(10);
    pub const LOG_LEN: usize = 10;

    impl Health {
        fn from_status(s: tspl::Status) -> Self {
            let b = s.0;
            match () {
                _ if b == 0 => Self::Ready,
                _ if b & (0x01 | 0x40) != 0 => Self::HeadOpen,
                _ if b & 0x04 != 0 => Self::NoPaper,
                _ if b & 0x02 != 0 => Self::PaperJam,
                _ if b & (0x08 | 0x80) != 0 => Self::Error(b),
                _ if b & 0x10 != 0 => Self::Paused,
                _ if b & 0x20 != 0 => Self::Printing,
                _ => Self::Error(b),
            }
        }

        fn from_open_error(e: &io::Error) -> Self {
            match e.kind() {
                io::ErrorKind::ResourceBusy => Self::Busy,
                _ => Self::Unavailable(e.to_string()),
            }
        }
    }

    /// Where a print job came from.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Origin {
        /// This app's own window.
        Local,
        Remote(IpAddr),
    }

    #[derive(Debug, Clone)]
    pub struct JobRecord {
        pub at: chrono::DateTime<chrono::Local>,
        pub origin: Origin,
        pub printer: String,
        pub bytes: usize,
        pub result: Result<(), String>,
    }

    #[derive(Default)]
    struct Shared {
        printers: Vec<String>,
        health: HashMap<String, Health>,
        /// Printer → when someone last asked about it.
        watched: HashMap<String, Instant>,
        /// Newest first.
        log: VecDeque<JobRecord>,
    }

    type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

    enum Command {
        /// A printer started being watched: poll now rather than at the next tick.
        Rescan,
        Print { printer: String, job: Vec<u8>, origin: Origin, done: Done },
    }

    /// Handle to the printer thread; clones share it.
    #[derive(Clone)]
    pub struct Service {
        shared: Arc<Mutex<Shared>>,
        commands: Sender<Command>,
    }

    impl Service {
        /// `wake` is called after every change (typically `ctx.request_repaint`).
        pub fn spawn(wake: impl Fn() + Send + 'static) -> Self {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (commands, rx) = mpsc::channel();
            let state = shared.clone();
            thread::spawn(move || {
                let publish = || {
                    let watched: Vec<String> = {
                        let mut s = state.lock().unwrap();
                        s.watched.retain(|_, at| at.elapsed() < WATCH);
                        s.watched.keys().cloned().collect()
                    };
                    let (printers, health) = scan(&watched);
                    let mut s = state.lock().unwrap();
                    s.printers = printers;
                    s.health = health;
                    drop(s);
                    wake();
                };
                publish();
                loop {
                    match rx.recv_timeout(POLL) {
                        Ok(Command::Rescan) | Err(RecvTimeoutError::Timeout) => {}
                        Ok(Command::Print { printer, job, origin, done }) => {
                            let result = print(&printer, &job);
                            let record = JobRecord {
                                at: chrono::Local::now(),
                                origin,
                                printer,
                                bytes: job.len(),
                                result: result.clone(),
                            };
                            let mut s = state.lock().unwrap();
                            s.log.push_front(record);
                            s.log.truncate(LOG_LEN);
                            drop(s);
                            done(result);
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    publish();
                }
            });
            Self { shared, commands }
        }

        /// The printers present, and the last known health of `printer`, which from now on
        /// is kept polled for a while.
        pub fn status(&self, printer: Option<&str>) -> Status {
            let mut s = self.shared.lock().unwrap();
            let health = printer.and_then(|name| {
                if s.watched.insert(name.to_owned(), Instant::now()).is_none() {
                    let _ = self.commands.send(Command::Rescan);
                }
                s.health.get(name).cloned()
            });
            Status { printers: s.printers.clone(), health }
        }

        /// Queues a job; `done` is called on the printer thread once it is sent or has failed.
        pub fn print(&self, printer: String, job: Vec<u8>, origin: Origin, done: impl FnOnce(Result<(), String>) + Send + 'static) {
            let done: Done = Box::new(done);
            if let Err(mpsc::SendError(Command::Print { done, .. })) =
                self.commands.send(Command::Print { printer, job, origin, done })
            {
                done(Err("printer thread stopped".into()));
            }
        }

        /// The last few jobs, newest first.
        pub fn log(&self) -> Vec<JobRecord> {
            self.shared.lock().unwrap().log.iter().cloned().collect()
        }
    }

    #[derive(Default)]
    struct Job {
        busy: bool,
        last: Option<Result<(), String>>,
    }

    /// This window's view of the printers: its own selection and its own last job.
    pub struct Printers {
        service: Service,
        selected: Option<String>,
        job: Arc<Mutex<Job>>,
        wake: Arc<dyn Fn() + Send + Sync>,
    }

    impl Printers {
        pub fn new(service: Service, selected: Option<String>, wake: impl Fn() + Send + Sync + 'static) -> Self {
            Self { service, selected, job: Arc::default(), wake: Arc::new(wake) }
        }

        pub fn snapshot(&self) -> Snapshot {
            let status = self.service.status(self.selected.as_deref());
            let job = self.job.lock().unwrap();
            Snapshot { printers: status.printers, health: status.health, busy_printing: job.busy, last_job: job.last.clone() }
        }

        pub fn select(&mut self, name: Option<String>) {
            self.selected = name;
        }

        pub fn print(&self, job: Vec<u8>) {
            let Some(printer) = self.selected.clone() else { return };
            self.job.lock().unwrap().busy = true;
            let (state, wake) = (self.job.clone(), self.wake.clone());
            self.service.print(printer, job, Origin::Local, move |result| {
                let mut s = state.lock().unwrap();
                s.busy = false;
                s.last = Some(result);
                drop(s);
                wake();
            });
        }
    }

    fn query_status(p: &mut dyn Transport) -> io::Result<Option<tspl::Status>> {
        p.write(tspl::Status::QUERY)?;
        Ok(p.read(Duration::from_millis(300))?.first().map(|&b| tspl::Status(b)))
    }

    fn health(found: &[Found], name: &str) -> Health {
        match found.iter().find(|f| f.name == name) {
            None => Health::Disconnected,
            Some(f) => match f.open() {
                Err(e) => Health::from_open_error(&e),
                Ok(mut p) => match query_status(p.as_mut()) {
                    Ok(Some(s)) => Health::from_status(s),
                    Ok(None) => Health::NoReply,
                    Err(e) => Health::Unavailable(e.to_string()),
                },
            },
        }
    }

    fn scan(watched: &[String]) -> (Vec<String>, HashMap<String, Health>) {
        match transport::discover() {
            Ok(found) => {
                let names = found.iter().map(|f| f.name.clone()).collect();
                (names, watched.iter().map(|name| (name.clone(), health(&found, name))).collect())
            }
            Err(e) => {
                let health = Health::Unavailable(e.to_string());
                (Vec::new(), watched.iter().map(|name| (name.clone(), health.clone())).collect())
            }
        }
    }

    fn print(name: &str, job: &[u8]) -> Result<(), String> {
        let found = transport::discover().map_err(|e| e.to_string())?;
        let f = found.iter().find(|f| f.name == name).ok_or("printer disconnected")?;
        let mut p = f.open().map_err(|e| e.to_string())?;
        // Re-check right before sending: the 2 s poll may be stale.
        match query_status(p.as_mut()).map_err(|e| e.to_string())? {
            Some(s) if s.is_ready() => {}
            Some(s) => return Err(s.problems().join(", ")),
            None => return Err("no status reply".into()),
        }
        p.write(job).map_err(|e| e.to_string())
    }
}
