//! Printing a series: the first few labels one job at a time, each after the printer has
//! finished the one before, so the run can still be stopped; then the rest in one job.

use std::ops::Range;

use web_time::{Duration, Instant};

use crate::printer::{Health, Snapshot};

/// Labels sent one by one before the rest goes in a single job.
pub const ONE_BY_ONE: u64 = 3;
/// A Ready seen this soon after a job may be a poll from before the printer started on it.
const SETTLE: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    /// The printer reported a problem while we waited for it.
    Printer(Health),
    Job(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    Stopped,
    /// `printed` labels went out before this.
    Failed(Reason),
    /// The single job with everything after the first `ONE_BY_ONE` labels.
    RestFailed(String),
}

#[derive(Debug)]
enum Phase {
    /// A job with these labels is out.
    Sending(Range<u64>),
    /// For the printer to finish the last job; `busy` once it was seen doing anything but Ready.
    Waiting { since: Instant, busy: bool },
    Finished(Outcome),
}

#[derive(Debug)]
pub struct Batch {
    pub total: u64,
    /// Labels sent successfully.
    pub printed: u64,
    phase: Phase,
    stopping: bool,
}

impl Batch {
    /// The batch and the labels of its first job, which the caller sends right away.
    pub fn start(total: u64) -> (Self, Range<u64>) {
        let first = 0..1;
        (Self { total, printed: 0, phase: Phase::Sending(first.clone()), stopping: false }, first)
    }

    fn next(&self) -> Range<u64> {
        if self.printed < ONE_BY_ONE { self.printed..self.printed + 1 } else { self.printed..self.total }
    }

    fn finish(&mut self, outcome: Outcome) {
        self.phase = Phase::Finished(outcome);
    }

    /// Follows the printer; returns the labels of the next job when it is time to send it.
    pub fn tick(&mut self, s: &Snapshot) -> Option<Range<u64>> {
        match &mut self.phase {
            Phase::Sending(labels) => {
                if s.busy_printing {
                    return None;
                }
                let labels = labels.clone();
                match s.last_job.clone().unwrap_or_else(|| Err("no result".into())) {
                    Ok(()) => {
                        self.printed = labels.end;
                        if self.printed >= self.total {
                            self.finish(Outcome::Done);
                        } else if self.stopping {
                            self.finish(Outcome::Stopped);
                        } else {
                            self.phase = Phase::Waiting { since: Instant::now(), busy: false };
                        }
                    }
                    Err(e) if labels.start >= ONE_BY_ONE => self.finish(Outcome::RestFailed(e)),
                    Err(e) => self.finish(Outcome::Failed(Reason::Job(e))),
                }
                None
            }
            Phase::Waiting { since, busy } => {
                if self.stopping {
                    self.finish(Outcome::Stopped);
                    return None;
                }
                match &s.health {
                    Some(Health::Ready) if *busy || since.elapsed() >= SETTLE => {
                        let labels = self.next();
                        self.phase = Phase::Sending(labels.clone());
                        Some(labels)
                    }
                    Some(Health::Ready) => None,
                    Some(
                        h @ (Health::NoPaper | Health::HeadOpen | Health::PaperJam | Health::Paused | Health::Error(_)),
                    ) => {
                        let reason = Reason::Printer(h.clone());
                        self.finish(Outcome::Failed(reason));
                        None
                    }
                    // Printing, or silent, or gone: wait until it is back or the user stops.
                    _ => {
                        *busy = true;
                        None
                    }
                }
            }
            Phase::Finished(_) => None,
        }
    }

    /// When the labels could not be turned into a job.
    pub fn fail(&mut self, e: String) {
        self.finish(Outcome::Failed(Reason::Job(e)));
    }

    /// Takes effect once the job in flight, if any, is done.
    pub fn stop(&mut self) {
        self.stopping = true;
    }

    /// Not once the rest is out in one job.
    pub fn can_stop(&self) -> bool {
        !self.stopping
            && match &self.phase {
                Phase::Sending(labels) => labels.start < ONE_BY_ONE,
                Phase::Waiting { .. } => true,
                Phase::Finished(_) => false,
            }
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        match &self.phase {
            Phase::Finished(o) => Some(o),
            _ => None,
        }
    }

    /// The labels being printed or about to be, while running.
    pub fn current(&self) -> Option<Range<u64>> {
        match &self.phase {
            Phase::Sending(labels) => Some(labels.clone()),
            Phase::Waiting { .. } => Some(self.next()),
            Phase::Finished(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(health: Health, busy_printing: bool, last_job: Option<Result<(), String>>) -> Snapshot {
        Snapshot { printers: Vec::new(), health: Some(health), busy_printing, last_job }
    }

    const SENT: Option<Result<(), String>> = Some(Ok(()));

    /// The job in flight is done, the printer prints it and comes back Ready.
    fn print_one(b: &mut Batch) -> Option<Range<u64>> {
        assert_eq!(b.tick(&snapshot(Health::Ready, true, None)), None);
        assert_eq!(b.tick(&snapshot(Health::Ready, false, SENT)), None);
        assert_eq!(b.tick(&snapshot(Health::Printing, false, SENT)), None);
        b.tick(&snapshot(Health::Ready, false, SENT))
    }

    #[test]
    fn three_one_by_one_then_the_rest_at_once() {
        let (mut b, first) = Batch::start(11);
        assert_eq!(first, 0..1);
        assert_eq!(print_one(&mut b), Some(1..2));
        assert_eq!(print_one(&mut b), Some(2..3));
        assert!(b.can_stop());
        b.tick(&snapshot(Health::Ready, false, SENT));
        assert!(b.can_stop(), "the third label is out, the rest is not yet");
        b.tick(&snapshot(Health::Printing, false, SENT));
        assert_eq!(b.tick(&snapshot(Health::Ready, false, SENT)), Some(3..11));
        assert!(!b.can_stop());
        assert_eq!(b.tick(&snapshot(Health::Ready, false, SENT)), None);
        assert_eq!((b.outcome(), b.printed), (Some(&Outcome::Done), 11));
    }

    #[test]
    fn a_stale_ready_right_after_the_job_is_not_trusted() {
        let (mut b, _) = Batch::start(2);
        b.tick(&snapshot(Health::Ready, false, SENT));
        assert_eq!(b.tick(&snapshot(Health::Ready, false, SENT)), None);
    }

    #[test]
    fn stop_waits_for_the_job_in_flight() {
        let (mut b, _) = Batch::start(5);
        b.stop();
        assert_eq!(b.tick(&snapshot(Health::Ready, true, None)), None);
        b.tick(&snapshot(Health::Ready, false, SENT));
        assert_eq!((b.outcome(), b.printed), (Some(&Outcome::Stopped), 1));
    }

    #[test]
    fn a_printer_problem_ends_the_run_but_silence_does_not() {
        let (mut b, _) = Batch::start(5);
        b.tick(&snapshot(Health::Ready, false, SENT));
        b.tick(&snapshot(Health::NoReply, false, SENT));
        b.tick(&snapshot(Health::Disconnected, false, SENT));
        assert_eq!(b.outcome(), None);
        b.tick(&snapshot(Health::NoPaper, false, SENT));
        assert_eq!(b.outcome(), Some(&Outcome::Failed(Reason::Printer(Health::NoPaper))));
        assert_eq!(b.printed, 1);
    }

    #[test]
    fn a_failed_rest_is_told_apart() {
        let (mut b, _) = Batch::start(4);
        for _ in 0..3 {
            print_one(&mut b);
        }
        b.tick(&snapshot(Health::Ready, false, Some(Err("too big".into()))));
        assert_eq!(b.outcome(), Some(&Outcome::RestFailed("too big".into())));
    }

    #[test]
    fn short_series_never_gets_a_rest_job() {
        let (mut b, _) = Batch::start(2);
        assert_eq!(print_one(&mut b), Some(1..2));
        b.tick(&snapshot(Health::Ready, false, Some(Err("jam".into()))));
        assert_eq!(b.outcome(), Some(&Outcome::Failed(Reason::Job("jam".into()))));
    }
}
