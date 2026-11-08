//! `mode=latest`: a worker thread owns the engine and analyzes the newest
//! frame while video passes at full rate.
//!
//! The streaming thread never waits on inference. It hands the worker a
//! packed copy of a frame only when the worker is idle, and attaches each
//! finished result once, to the first buffer after it completes. Results
//! carry a generation, so a flush, a new segment or a stream start discards
//! whatever was in flight.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::describer::Describer;
use super::settings::Request;

/// One frame for the worker. It owns its pixels and never holds a buffer.
pub struct Job {
    pub rgb: Vec<u8>,
    pub size: (u32, u32),
    pub analyzed: Analyzed,
}

/// Which frame a result describes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Analyzed {
    pub pts: Option<u64>,
    pub running_time: Option<u64>,
    pub generation: u64,
}

/// A finished analysis.
pub struct Done {
    pub analyzed: Analyzed,
    pub result: Result<cactus::Description, String>,
    pub elapsed: Duration,
}

pub struct Worker {
    jobs: Option<SyncSender<Job>>,
    results: Receiver<Done>,
    thread: Option<JoinHandle<()>>,
    busy: bool,
}

impl Worker {
    pub fn spawn(mut describer: Box<dyn Describer>, request: Request) -> std::io::Result<Self> {
        let (jobs, inbox) = mpsc::sync_channel::<Job>(1);
        let (outbox, results) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cactusmirage".to_owned())
            .spawn(move || {
                // Ends when the element drops the sender at stop.
                while let Ok(job) = inbox.recv() {
                    let started = Instant::now();
                    let result = describer.describe(&job.rgb, job.size.0, job.size.1, &request);
                    let done = Done {
                        analyzed: job.analyzed,
                        result,
                        elapsed: started.elapsed(),
                    };
                    if outbox.send(done).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            results,
            thread: Some(thread),
            busy: false,
        })
    }

    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Hands a frame to an idle worker; returns whether it was taken.
    pub fn submit(&mut self, job: Job) -> bool {
        if self.busy {
            return false;
        }
        let taken = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.try_send(job).is_ok());
        self.busy = taken;
        taken
    }

    /// Collects finished analyses without waiting. A worker that is gone,
    /// which only a panic in the engine call causes, is an error whatever
    /// the generation.
    pub fn poll(&mut self) -> Result<Vec<Done>, String> {
        let mut done = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    done.push(result);
                }
                Err(TryRecvError::Empty) => return Ok(done),
                Err(TryRecvError::Disconnected) => {
                    self.jobs = None;
                    self.busy = false;
                    return Err("the analysis thread stopped".to_owned());
                }
            }
        }
    }

    /// Stops the worker, waiting for an inference in progress: the engine
    /// call cannot be cancelled.
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.jobs = None;
        match self.thread.take().map(JoinHandle::join) {
            Some(Err(_panic)) => Err("the analysis thread panicked".to_owned()),
            _ => Ok(()),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _joined = self.shutdown();
    }
}
