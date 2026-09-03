//! Event model (specification sections 37, 43).
//!
//! Events are immutable facts about something that already happened. They never
//! perform actions and nothing subscribes to them synchronously: the core
//! appends, the shell drains.
//!
//! The queue is bounded. When a producer outruns the drain the oldest events are
//! dropped and counted, because an unbounded queue is a memory leak with extra
//! steps (specification section 43).

use crate::document::{ContentRevision, DocumentId};
use std::collections::VecDeque;
use std::time::SystemTime;

/// Default queue depth. Large enough for a burst of edits between frames, small
/// enough to be a fixed, visible cost.
pub const DEFAULT_CAPACITY: usize = 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u64);

impl EventId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What happened. Stage 1 emits only the events its subsystems can produce;
/// search, Git, terminal and task events arrive with those subsystems.
#[derive(Clone, Debug, PartialEq)]
pub enum EventPayload {
    WorkspaceOpened {
        root: String,
    },
    WorkspaceClosed,
    /// A load was admitted and a loading tab now exists.
    DocumentLoadStarted {
        document: DocumentId,
        path: String,
        task: u64,
    },
    /// Another request attached to a load already in flight.
    DocumentLoadJoined {
        document: DocumentId,
        task: u64,
        requests: u32,
    },
    /// A load finished and the document is editable.
    DocumentOpened {
        document: DocumentId,
        path: String,
        bytes: u64,
        lines: usize,
    },
    /// A load failed; the tab is gone and the reason is in the payload.
    DocumentLoadFailed {
        document: DocumentId,
        code: &'static str,
    },
    /// A load was cancelled before it finished.
    DocumentLoadCancelled {
        document: DocumentId,
    },
    DocumentEdited {
        document: DocumentId,
        revision: ContentRevision,
    },
    /// A save was admitted; the document is now in `PersistenceState::Saving`.
    DocumentSaveStarted {
        document: DocumentId,
        task: u64,
        revision: ContentRevision,
    },
    DocumentSaved {
        document: DocumentId,
        path: String,
        revision: ContentRevision,
    },
    DocumentSaveFailed {
        document: DocumentId,
        code: &'static str,
    },
    DocumentClosed {
        document: DocumentId,
    },
    CursorChanged {
        document: DocumentId,
        line: usize,
        column: usize,
    },
    SelectionChanged {
        document: DocumentId,
        chars: usize,
    },
    ViewportChanged {
        document: DocumentId,
        first_line: usize,
        visible_lines: usize,
    },
    RenderSnapshotPublished {
        document: DocumentId,
        revision: ContentRevision,
    },
    PerformanceBudgetExceeded {
        metric: &'static str,
        p95_micros: u64,
        threshold_micros: u64,
    },
}

impl EventPayload {
    /// Stable event name for logs and future automation.
    pub const fn name(&self) -> &'static str {
        match self {
            EventPayload::WorkspaceOpened { .. } => "workspace_opened",
            EventPayload::WorkspaceClosed => "workspace_closed",
            EventPayload::DocumentLoadStarted { .. } => "document_load_started",
            EventPayload::DocumentLoadJoined { .. } => "document_load_joined",
            EventPayload::DocumentOpened { .. } => "document_opened",
            EventPayload::DocumentLoadFailed { .. } => "document_load_failed",
            EventPayload::DocumentLoadCancelled { .. } => "document_load_cancelled",
            EventPayload::DocumentEdited { .. } => "document_edited",
            EventPayload::DocumentSaveStarted { .. } => "document_save_started",
            EventPayload::DocumentSaved { .. } => "document_saved",
            EventPayload::DocumentSaveFailed { .. } => "document_save_failed",
            EventPayload::DocumentClosed { .. } => "document_closed",
            EventPayload::CursorChanged { .. } => "cursor_changed",
            EventPayload::SelectionChanged { .. } => "selection_changed",
            EventPayload::ViewportChanged { .. } => "viewport_changed",
            EventPayload::RenderSnapshotPublished { .. } => "render_snapshot_published",
            EventPayload::PerformanceBudgetExceeded { .. } => "performance_budget_exceeded",
        }
    }
}

/// One event: a fact, with the identity and timestamp needed to order it.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub timestamp: SystemTime,
    pub source: &'static str,
    pub payload: EventPayload,
}

/// Bounded event queue.
#[derive(Debug)]
pub struct EventQueue {
    events: VecDeque<Event>,
    capacity: usize,
    next_id: u64,
    dropped: u64,
}

impl Default for EventQueue {
    fn default() -> Self {
        EventQueue::new(DEFAULT_CAPACITY)
    }
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        EventQueue {
            events: VecDeque::with_capacity(capacity.min(64)),
            capacity: capacity.max(1),
            next_id: 1,
            dropped: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Events discarded because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Appends an event, dropping the oldest if the queue is full.
    pub fn emit(&mut self, source: &'static str, payload: EventPayload) -> EventId {
        let id = EventId(self.next_id);
        self.next_id += 1;
        if self.events.len() == self.capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(Event { id, timestamp: SystemTime::now(), source, payload });
        id
    }

    /// Takes every queued event.
    pub fn drain(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }

    pub fn peek(&self) -> Option<&Event> {
        self.events.front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(n: u64) -> EventPayload {
        EventPayload::DocumentClosed { document: DocumentId::new(n) }
    }

    #[test]
    fn events_are_ordered_and_identified() {
        let mut queue = EventQueue::default();
        let first = queue.emit("test", payload(1));
        let second = queue.emit("test", payload(2));
        assert!(second > first);

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id, first);
        assert_eq!(drained[0].source, "test");
        assert!(queue.is_empty(), "draining empties the queue");
    }

    #[test]
    fn the_queue_is_bounded_and_counts_what_it_drops() {
        let mut queue = EventQueue::new(4);
        for n in 0..10 {
            queue.emit("test", payload(n));
        }
        assert_eq!(queue.len(), 4, "the queue never grows past its capacity");
        assert_eq!(queue.dropped(), 6);

        let drained = queue.drain();
        assert_eq!(drained[0].payload, payload(6), "the oldest events are the ones dropped");
    }

    #[test]
    fn every_payload_has_a_stable_name() {
        let payloads = [
            EventPayload::WorkspaceClosed,
            payload(1),
            EventPayload::DocumentEdited {
                document: DocumentId::new(1),
                revision: ContentRevision::default(),
            },
            EventPayload::PerformanceBudgetExceeded {
                metric: "input.to_state",
                p95_micros: 9000,
                threshold_micros: 5000,
            },
        ];
        for payload in payloads {
            assert!(!payload.name().is_empty());
            assert!(!payload.name().contains(' '));
        }
    }

    #[test]
    fn a_capacity_of_zero_is_treated_as_one() {
        let mut queue = EventQueue::new(0);
        queue.emit("test", payload(1));
        queue.emit("test", payload(2));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.dropped(), 1);
    }
}
