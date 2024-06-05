use std::{borrow::Cow, future::Future, sync::{atomic::AtomicUsize, Arc}};

use chrono::TimeDelta;

use tokio::sync::mpsc::Receiver;

pub fn kill_requested(kill_rx: &mut tokio::sync::oneshot::Receiver<()>) -> bool {
    match kill_rx.try_recv() {
        Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            true
        },
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
            false
        },
    }
}

pub struct ThreadDeathReporter {
    name: Cow<'static, str>,
    death_tally: Arc<AtomicUsize>,
}

impl ThreadDeathReporter {
    pub fn new(tally: &Arc<AtomicUsize>, name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            death_tally: Arc::clone(tally),
            name: name.into(),
        }
    }

    pub fn spawn<T: Send + 'static>(self, f: impl Future<Output=T> + Send + 'static) -> tokio::task::JoinHandle<T> {
        tokio::spawn(async move {
            let a = f.await;
            trc::info!("KILL Death: {}", self.name.as_ref());
            self.death_tally.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            a
        })
    }
}

pub enum TimeoutOutcome<T> {
    Value(T),
    Closed,
    Timeout,
}

impl <T> TimeoutOutcome<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v),
            Self::Timeout | Self::Closed => None,
        }
    }
}

pub trait ReceiverTimeoutExt<T> {
    async fn recv_for_ms(&mut self, timeout_ms: i64) -> TimeoutOutcome<T> {
        self.recv_for(TimeDelta::milliseconds(timeout_ms)).await
    }
    /// Try to `recv_for` a certain amount of time.
    async fn recv_for(&mut self, timeout: TimeDelta) -> TimeoutOutcome<T>;
}

impl <T> ReceiverTimeoutExt<T> for Receiver<T> {
    async fn recv_for(&mut self, timeout: TimeDelta) -> TimeoutOutcome<T> {
        tokio::select! {
            opt = self.recv() => match opt {
                Some(val) => {
                    TimeoutOutcome::Value(val)
                },
                None => {
                    TimeoutOutcome::Closed
                },
            },
            _ = tokio::time::sleep(timeout.to_std().unwrap()) => TimeoutOutcome::Timeout,
        }
    }
}
