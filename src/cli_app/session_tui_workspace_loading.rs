use std::sync::mpsc::{Receiver, TryRecvError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkspaceLoadState {
    Loading,
    Ready,
    Failed { message: String, retryable: bool },
}

#[derive(Debug)]
pub(super) enum WorkspaceLoadPoll<T> {
    Pending,
    Ready(T),
    Failed(String),
}

/// Tracks one async workspace load while keeping retry policy outside the
/// renderer and event loop. A future workspace supplies its own spawn function
/// and can decide when to call it again after `Failed`.
#[derive(Debug)]
pub(super) struct WorkspaceLoad<T> {
    receiver: Receiver<Result<T, String>>,
    state: WorkspaceLoadState,
}

impl<T> WorkspaceLoad<T> {
    pub(super) fn new(receiver: Receiver<Result<T, String>>) -> Self {
        Self {
            receiver,
            state: WorkspaceLoadState::Loading,
        }
    }

    #[cfg(test)]
    pub(super) fn state(&self) -> &WorkspaceLoadState {
        &self.state
    }

    pub(super) fn is_loading(&self) -> bool {
        self.state == WorkspaceLoadState::Loading
    }

    #[allow(dead_code)] // Consumed by the next workspace when it adds a retry command.
    pub(super) fn retry(&mut self, receiver: Receiver<Result<T, String>>) -> bool {
        if !matches!(self.state, WorkspaceLoadState::Failed { .. }) {
            return false;
        }
        self.receiver = receiver;
        self.state = WorkspaceLoadState::Loading;
        true
    }

    pub(super) fn poll(&mut self) -> WorkspaceLoadPoll<T> {
        if !self.is_loading() {
            return WorkspaceLoadPoll::Pending;
        }
        match self.receiver.try_recv() {
            Ok(Ok(value)) => {
                self.state = WorkspaceLoadState::Ready;
                WorkspaceLoadPoll::Ready(value)
            }
            Ok(Err(message)) => {
                self.state = WorkspaceLoadState::Failed {
                    message: message.clone(),
                    retryable: true,
                };
                WorkspaceLoadPoll::Failed(message)
            }
            Err(TryRecvError::Empty) => WorkspaceLoadPoll::Pending,
            Err(TryRecvError::Disconnected) => {
                let message = "workspace loader stopped before reporting a result".to_string();
                self.state = WorkspaceLoadState::Failed {
                    message: message.clone(),
                    retryable: true,
                };
                WorkspaceLoadPoll::Failed(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn records_a_retryable_error_from_an_async_loader() {
        let (sender, receiver) = mpsc::channel();
        let mut load = WorkspaceLoad::<()>::new(receiver);
        sender
            .send(Err("unavailable".to_string()))
            .expect("send result");

        assert!(
            matches!(load.poll(), WorkspaceLoadPoll::Failed(message) if message == "unavailable")
        );
        assert_eq!(
            load.state(),
            &WorkspaceLoadState::Failed {
                message: "unavailable".to_string(),
                retryable: true,
            }
        );

        let (retry_sender, retry_receiver) = mpsc::channel();
        assert!(load.retry(retry_receiver));
        retry_sender.send(Ok(())).expect("send retried result");
        assert!(matches!(load.poll(), WorkspaceLoadPoll::Ready(())));
        assert!(matches!(load.poll(), WorkspaceLoadPoll::Pending));
    }
}
