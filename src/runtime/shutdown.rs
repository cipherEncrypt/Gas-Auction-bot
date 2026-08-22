/// Broadcasts shutdown signals for graceful drain.
#[derive(Clone)]
pub struct ShutdownCoordinator {
    sender: tokio::sync::watch::Sender<bool>,
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl ShutdownCoordinator {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        Self { sender, receiver }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.receiver.clone()
    }

    pub fn trigger(&self) {
        let _ = self.sender.send(true);
    }

    pub fn is_shutdown(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn wait_for_shutdown(&self) {
        let mut receiver = self.receiver.clone();
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_coordinator_triggers() {
        let coordinator = ShutdownCoordinator::new();
        assert!(!coordinator.is_shutdown());
        coordinator.trigger();
        assert!(coordinator.is_shutdown());
    }
}
