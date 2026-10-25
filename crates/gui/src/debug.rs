//! Debug builds only: `ITMO_LABELS_SCREENSHOT=out.ppm` saves the window a few seconds
//! after start (once the printer worker has reported) and quits. Used to check the layout
//! without a human looking at the screen.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use eframe::egui::{self, Event, UserData, ViewportCommand};

const DELAY: Duration = Duration::from_secs(3);

pub struct Screenshot {
    path: Option<PathBuf>,
    started: Instant,
    requested: bool,
}

impl Screenshot {
    pub fn from_env() -> Self {
        Self { path: std::env::var_os("ITMO_LABELS_SCREENSHOT").map(PathBuf::from), started: Instant::now(), requested: false }
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(path) = &self.path else { return };
        if !self.requested {
            if self.started.elapsed() >= DELAY {
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(UserData::default()));
                self.requested = true;
            } else {
                ctx.request_repaint_after(DELAY - self.started.elapsed());
            }
            return;
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let mut ppm = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
            ppm.extend(image.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b()]));
            let _ = fs::write(path, ppm);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}
