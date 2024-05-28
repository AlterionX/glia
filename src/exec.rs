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
