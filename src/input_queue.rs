use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedInputKind {
    Steer,
    FollowUp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedInput {
    pub id: u64,
    pub kind: QueuedInputKind,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub steering: Vec<QueuedInput>,
    pub follow_ups: Vec<QueuedInput>,
}

impl QueueSnapshot {
    pub fn is_empty(&self) -> bool {
        self.steering.is_empty() && self.follow_ups.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steering.len() + self.follow_ups.len()
    }
}

#[derive(Clone, Debug, Default)]
pub struct InputQueue {
    inner: Arc<Mutex<QueueState>>,
}

#[derive(Debug, Default)]
struct QueueState {
    next_id: u64,
    steering: VecDeque<QueuedInput>,
    follow_ups: VecDeque<QueuedInput>,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn steer(&self, text: impl Into<String>) -> QueuedInput {
        self.enqueue(QueuedInputKind::Steer, text.into())
    }

    pub fn follow_up(&self, text: impl Into<String>) -> QueuedInput {
        self.enqueue(QueuedInputKind::FollowUp, text.into())
    }

    pub fn has_steering(&self) -> bool {
        !self
            .inner
            .lock()
            .expect("input queue lock poisoned")
            .steering
            .is_empty()
    }

    pub fn take_steering(&self, take_all: bool) -> Vec<QueuedInput> {
        let mut state = self.inner.lock().expect("input queue lock poisoned");
        drain(&mut state.steering, take_all)
    }

    pub fn take_follow_ups(&self, take_all: bool) -> Vec<QueuedInput> {
        let mut state = self.inner.lock().expect("input queue lock poisoned");
        drain(&mut state.follow_ups, take_all)
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let state = self.inner.lock().expect("input queue lock poisoned");
        QueueSnapshot {
            steering: state.steering.iter().cloned().collect(),
            follow_ups: state.follow_ups.iter().cloned().collect(),
        }
    }

    pub fn clear(&self) -> QueueSnapshot {
        let mut state = self.inner.lock().expect("input queue lock poisoned");
        QueueSnapshot {
            steering: state.steering.drain(..).collect(),
            follow_ups: state.follow_ups.drain(..).collect(),
        }
    }

    fn enqueue(&self, kind: QueuedInputKind, text: String) -> QueuedInput {
        let mut state = self.inner.lock().expect("input queue lock poisoned");
        state.next_id = state.next_id.saturating_add(1);
        let input = QueuedInput {
            id: state.next_id,
            kind,
            text,
        };
        match kind {
            QueuedInputKind::Steer => state.steering.push_back(input.clone()),
            QueuedInputKind::FollowUp => state.follow_ups.push_back(input.clone()),
        }
        input
    }
}

fn drain(queue: &mut VecDeque<QueuedInput>, take_all: bool) -> Vec<QueuedInput> {
    if take_all {
        queue.drain(..).collect()
    } else {
        queue.pop_front().into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_independent_ordered_queues() {
        let queue = InputQueue::new();
        let first = queue.steer("first");
        let follow_up = queue.follow_up("later");
        let second = queue.steer("second");

        assert!(first.id < follow_up.id && follow_up.id < second.id);
        assert_eq!(queue.take_steering(false), [first]);
        assert_eq!(queue.take_steering(true), [second]);
        assert_eq!(queue.take_follow_ups(false), [follow_up]);
        assert!(queue.snapshot().is_empty());
    }

    #[test]
    fn snapshot_and_clear_preserve_kinds() {
        let queue = InputQueue::new();
        queue.steer("redirect");
        queue.follow_up("next");

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(queue.clear(), snapshot);
        assert!(queue.snapshot().is_empty());
    }
}
