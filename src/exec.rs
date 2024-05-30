use std::{future::Future, sync::{atomic::AtomicUsize, Arc}};

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

pub fn spawn_kill_reporting<T: Send + 'static>(reporter: Arc<AtomicUsize>, f: impl Future<Output=T> + Send + 'static) -> tokio::task::JoinHandle<T> {
    tokio::spawn(async move {
        let a = f.await;
        reporter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        a
    })
}
