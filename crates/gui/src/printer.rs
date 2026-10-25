//! Background thread that owns all printer I/O: rescans every 2 s, polls the selected
//! printer's status, and runs print jobs in between, so the UI never blocks on USB.

use std::{
    io,
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use itmo_labels_core::{
    transport::{self, Transport},
    tspl::Status,
};

const POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
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

impl Health {
    fn from_status(s: Status) -> Self {
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

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub printers: Vec<String>,
    /// Of the selected printer; `None` until the first poll.
    pub health: Option<Health>,
    pub busy_printing: bool,
    /// Outcome of the last print job.
    pub last_job: Option<Result<(), String>>,
}

enum Command {
    Select(Option<String>),
    Print(Vec<u8>),
}

pub struct Worker {
    snapshot: Arc<Mutex<Snapshot>>,
    commands: Sender<Command>,
}

impl Worker {
    /// `wake` is called after every snapshot change (typically `ctx.request_repaint`).
    pub fn spawn(selected: Option<String>, wake: impl Fn() + Send + 'static) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let (commands, rx) = mpsc::channel();
        let shared = snapshot.clone();
        thread::spawn(move || {
            let mut selected = selected;
            let publish = |selected: Option<&str>| {
                let (printers, health) = scan(selected);
                let mut s = shared.lock().unwrap();
                s.printers = printers;
                s.health = health;
                drop(s);
                wake();
            };
            publish(selected.as_deref());
            loop {
                let job = match rx.recv_timeout(POLL) {
                    Ok(Command::Select(name)) => {
                        selected = name;
                        shared.lock().unwrap().health = None;
                        None
                    }
                    Ok(Command::Print(job)) => Some(job),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if let Some(job) = job {
                    shared.lock().unwrap().busy_printing = true;
                    wake();
                    let result = print(selected.as_deref(), &job);
                    let mut s = shared.lock().unwrap();
                    s.busy_printing = false;
                    s.last_job = Some(result);
                }
                publish(selected.as_deref());
            }
        });
        Self { snapshot, commands }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn select(&self, name: Option<String>) {
        let _ = self.commands.send(Command::Select(name));
    }

    pub fn print(&self, job: Vec<u8>) {
        let _ = self.commands.send(Command::Print(job));
    }
}

fn query_status(p: &mut dyn Transport) -> io::Result<Option<Status>> {
    Status::query(p, Duration::from_millis(300))
}

fn scan(selected: Option<&str>) -> (Vec<String>, Option<Health>) {
    let found = match transport::discover() {
        Ok(found) => found,
        Err(e) => return (Vec::new(), Some(Health::Unavailable(e.to_string()))),
    };
    let names = found.iter().map(|f| f.name.clone()).collect();
    let health = selected.map(|name| match found.iter().find(|f| f.name == name) {
        None => Health::Disconnected,
        Some(f) => match f.open() {
            Err(e) => Health::from_open_error(&e),
            Ok(mut p) => match query_status(p.as_mut()) {
                Ok(Some(s)) => Health::from_status(s),
                Ok(None) => Health::NoReply,
                Err(e) => Health::Unavailable(e.to_string()),
            },
        },
    });
    (names, health)
}

fn print(selected: Option<&str>, job: &[u8]) -> Result<(), String> {
    let name = selected.ok_or("no printer selected")?;
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
