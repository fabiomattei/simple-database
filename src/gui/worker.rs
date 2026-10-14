//! Database work off the UI thread. A [`Worker`] owns the open connection on
//! its own thread and runs jobs in order; a [`Task`] is one-off work (connect,
//! read another database) on a thread of its own.

use eframe::egui;
use simple_database::db::{Canceller, Database};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// What a finished job does to the app, back on the UI thread.
pub type Reply<T> = Box<dyn FnOnce(&mut T) + Send>;
type Job<T> = Box<dyn FnOnce(&mut Database) -> Reply<T> + Send>;

pub struct Worker<T> {
    jobs: Sender<Job<T>>,
    replies: Receiver<Reply<T>>,
    /// Labels of jobs not yet replied to, oldest (running) first.
    pending: VecDeque<String>,
    /// When the running job started.
    started: Instant,
    canceller: Arc<Canceller>,
    /// Cancel was asked for the running job.
    cancelling: bool,
    /// Why cancelling failed, from the thread that tried.
    cancel_errors: (Sender<String>, Receiver<String>),
}

impl<T: 'static> Worker<T> {
    /// Move `db` to a new thread. Dropping the worker lets the thread finish
    /// its current job and close the connection.
    pub fn spawn(mut db: Database, ctx: egui::Context) -> Self {
        let canceller = db.canceller();
        let (jobs, inbox) = mpsc::channel::<Job<T>>();
        let (outbox, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("sdb-db".into())
            .spawn(move || {
                for job in inbox {
                    db.clear_cancel();
                    if outbox.send(job(&mut db)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("cannot start the database thread");
        Worker {
            jobs,
            replies,
            pending: VecDeque::new(),
            started: Instant::now(),
            canceller,
            cancelling: false,
            cancel_errors: mpsc::channel(),
        }
    }

    /// Queue `work`; `then` gets its result on the UI thread, from [`Worker::poll`].
    pub fn run<R: Send + 'static>(
        &mut self,
        what: impl Into<String>,
        work: impl FnOnce(&mut Database) -> R + Send + 'static,
        then: impl FnOnce(&mut T, R) + Send + 'static,
    ) {
        let job: Job<T> = Box::new(move |db| {
            let r = work(db);
            Box::new(move |app: &mut T| then(app, r))
        });
        if self.jobs.send(job).is_ok() {
            if self.pending.is_empty() {
                self.started = Instant::now();
            }
            self.pending.push_back(what.into());
        }
    }

    /// Replies of the jobs finished since the last call, in order.
    pub fn poll(&mut self) -> Vec<Reply<T>> {
        let mut out = Vec::new();
        loop {
            match self.replies.try_recv() {
                Ok(r) => {
                    self.pending.pop_front();
                    self.started = Instant::now();
                    self.cancelling = false;
                    out.push(r);
                }
                Err(TryRecvError::Empty) => break,
                // The thread panicked: nothing will reply any more.
                Err(TryRecvError::Disconnected) => {
                    self.pending.clear();
                    break;
                }
            }
        }
        out
    }

    /// Stop the running job (queued ones still run). Postgres and MySQL
    /// connect again to cancel, so that happens on a thread of its own.
    pub fn cancel(&mut self) {
        if self.pending.is_empty() || self.cancelling {
            return;
        }
        self.cancelling = true;
        let (canceller, errors) = (self.canceller.clone(), self.cancel_errors.0.clone());
        std::thread::spawn(move || {
            if let Err(e) = canceller.cancel() {
                let _ = errors.send(format!("{e:#}"));
            }
        });
    }

    /// Whether cancel was asked for the running job.
    pub fn cancelling(&self) -> bool {
        self.cancelling
    }

    /// Why the last cancel failed, once.
    pub fn cancel_error(&mut self) -> Option<String> {
        self.cancel_errors.1.try_recv().ok()
    }

    /// The running job and for how long, if any.
    pub fn busy(&self) -> Option<(&str, Duration)> {
        self.pending.front().map(|w| (w.as_str(), self.started.elapsed()))
    }

    /// Whether a job with this label is queued or running.
    pub fn is_running(&self, what: &str) -> bool {
        self.pending.iter().any(|w| w == what)
    }
}

/// One-off work on its own thread.
pub struct Task<R> {
    rx: Receiver<R>,
    pub what: String,
    pub started: Instant,
}

impl<R: Send + 'static> Task<R> {
    pub fn spawn(what: impl Into<String>, ctx: &egui::Context, work: impl FnOnce() -> R + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint();
        });
        Task { rx, what: what.into(), started: Instant::now() }
    }

    /// The result once it is ready; `Err` if the thread panicked.
    pub fn poll(&self) -> Option<Result<R, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(Ok(r)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(format!("{} failed unexpectedly", self.what))),
        }
    }
}
