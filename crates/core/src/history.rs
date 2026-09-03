//! Undo/redo (specification sections 23, 63).
//!
//! History is a stack of reversible operations, never a stack of document
//! copies: an [`Edit`] records what was removed and what was inserted at one
//! position, so its inverse is the same edit with those two swapped.
//!
//! Adjacent compatible edits are grouped, so typing `hello` is one undo step
//! rather than five. A group ends when the specification says it must: a cursor
//! jump, a selection change, a paste, a delete of a selection, a command, a
//! save, an undo or a redo - or when the coalescing window expires.

use crate::selection::SelectionSet;
use ls_buffer::{CharOffset, TextBuffer};
use std::time::{Duration, Instant};

/// Initial coalescing window (specification section 23). Configurable, and
/// explicitly not an architectural invariant.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// Identifies one transaction. Also serves as the document's "which state am I
/// in" token, which is how clean/dirty survives undo and redo.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId(u64);

impl TransactionId {
    /// The state of a document that has had no transactions applied.
    pub const ROOT: TransactionId = TransactionId(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One reversible change: replace `removed` with `inserted` at `at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub at: CharOffset,
    pub removed: Box<str>,
    pub inserted: Box<str>,
}

impl Edit {
    pub fn insert(at: CharOffset, text: impl Into<Box<str>>) -> Self {
        Edit { at, removed: "".into(), inserted: text.into() }
    }

    pub fn delete(at: CharOffset, removed: impl Into<Box<str>>) -> Self {
        Edit { at, removed: removed.into(), inserted: "".into() }
    }

    pub fn removed_chars(&self) -> usize {
        self.removed.chars().count()
    }

    pub fn inserted_chars(&self) -> usize {
        self.inserted.chars().count()
    }

    /// Position just after the inserted text once this edit has been applied.
    pub fn end_after_apply(&self) -> CharOffset {
        self.at + self.inserted_chars()
    }

    /// The edit that undoes this one.
    pub fn inverse(&self) -> Edit {
        Edit { at: self.at, removed: self.inserted.clone(), inserted: self.removed.clone() }
    }

    /// Applies this edit to a buffer.
    pub fn apply(&self, buffer: &mut TextBuffer) {
        let removed = self.removed_chars();
        if removed > 0 {
            buffer.remove(self.at..(self.at + removed));
        }
        if !self.inserted.is_empty() {
            buffer.insert(self.at, &self.inserted);
        }
    }

    /// Maps a position recorded before this edit to its position afterwards.
    ///
    /// Positions inside the removed range collapse to the edit's start, which is
    /// the only sensible answer: the text they pointed at is gone.
    pub fn transform(&self, position: CharOffset) -> CharOffset {
        let removed = self.removed_chars();
        let inserted = self.inserted_chars();
        if position <= self.at {
            position
        } else if position >= self.at + removed {
            position + inserted - removed
        } else {
            self.at + inserted
        }
    }
}

/// What produced an edit. Only typing-like kinds are allowed to coalesce.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// Characters typed at the caret.
    Typing,
    /// Backspace at the caret.
    Backspace,
    /// Delete at the caret.
    DeleteForward,
    Paste,
    Cut,
    /// Typing or pasting over a selection.
    ReplaceSelection,
    /// Anything applied by code rather than by a keystroke.
    Programmatic,
}

impl EditKind {
    pub fn coalescable(self) -> bool {
        matches!(self, EditKind::Typing | EditKind::Backspace | EditKind::DeleteForward)
    }
}

/// One undo step: a group of edits plus the selection on either side of it.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub id: TransactionId,
    pub edits: Vec<Edit>,
    pub before: SelectionSet,
    pub after: SelectionSet,
    pub kind: EditKind,
    last_touched: Instant,
    open: bool,
}

impl Transaction {
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

/// Undo and redo stacks for one document.
#[derive(Debug)]
pub struct EditHistory {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    coalesce_window: Duration,
    next_id: u64,
}

impl Default for EditHistory {
    fn default() -> Self {
        EditHistory::new(DEFAULT_COALESCE_WINDOW)
    }
}

impl EditHistory {
    pub fn new(coalesce_window: Duration) -> Self {
        EditHistory { undo: Vec::new(), redo: Vec::new(), coalesce_window, next_id: 1 }
    }

    pub fn coalesce_window(&self) -> Duration {
        self.coalesce_window
    }

    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.coalesce_window = window;
    }

    /// Token identifying the current content state. Comparing it against the
    /// token captured at save time is what makes clean/dirty survive undo.
    pub fn state_token(&self) -> TransactionId {
        self.undo.last().map(|t| t.id).unwrap_or(TransactionId::ROOT)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// Ends the current coalescing group. Idempotent.
    pub fn force_boundary(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.open = false;
        }
    }

    /// Records one edit, extending the open group when the specification's
    /// continuation conditions all hold.
    pub fn record(
        &mut self,
        edit: Edit,
        kind: EditKind,
        before: &SelectionSet,
        after: &SelectionSet,
        now: Instant,
    ) {
        self.redo.clear();

        if self.can_coalesce(&edit, kind, before, now) {
            let last = self.undo.last_mut().expect("can_coalesce checked the stack");
            last.edits.push(edit);
            last.after = after.clone();
            last.last_touched = now;
            // A line break ends the group: the next word starts a new thought.
            if last.edits.last().is_some_and(|e| e.inserted.contains('\n')) {
                last.open = false;
            }
            return;
        }

        let open = kind.coalescable() && !edit.inserted.contains('\n');
        let id = TransactionId(self.next_id);
        self.next_id += 1;
        self.undo.push(Transaction {
            id,
            edits: vec![edit],
            before: before.clone(),
            after: after.clone(),
            kind,
            last_touched: now,
            open,
        });
    }

    /// Records several edits as one indivisible undo step.
    pub fn record_group(
        &mut self,
        edits: Vec<Edit>,
        kind: EditKind,
        before: &SelectionSet,
        after: &SelectionSet,
        now: Instant,
    ) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        let id = TransactionId(self.next_id);
        self.next_id += 1;
        self.undo.push(Transaction {
            id,
            edits,
            before: before.clone(),
            after: after.clone(),
            kind,
            last_touched: now,
            open: false,
        });
    }

    fn can_coalesce(
        &self,
        edit: &Edit,
        kind: EditKind,
        before: &SelectionSet,
        now: Instant,
    ) -> bool {
        if !kind.coalescable() {
            return false;
        }
        let Some(last) = self.undo.last() else { return false };
        if !last.open || last.kind != kind {
            return false;
        }
        if now.saturating_duration_since(last.last_touched) > self.coalesce_window {
            return false;
        }
        // No selection interruption: a group only continues from a plain caret.
        if !before.primary().is_caret() {
            return false;
        }
        // No cursor discontinuity: the caret must still be where the group left it.
        if last.after.primary().head != before.primary().head {
            return false;
        }
        let Some(previous) = last.edits.last() else { return false };
        match kind {
            EditKind::Typing => edit.removed.is_empty() && edit.at == previous.end_after_apply(),
            EditKind::Backspace => {
                edit.inserted.is_empty() && edit.at + edit.removed_chars() == previous.at
            }
            EditKind::DeleteForward => edit.inserted.is_empty() && edit.at == previous.at,
            _ => false,
        }
    }

    /// Takes the most recent transaction off the undo stack.
    pub fn pop_undo(&mut self) -> Option<Transaction> {
        self.undo.pop()
    }

    pub fn push_redo(&mut self, transaction: Transaction) {
        self.redo.push(transaction);
    }

    pub fn pop_redo(&mut self) -> Option<Transaction> {
        self.redo.pop()
    }

    /// Puts a redone transaction back on the undo stack, closed so that typing
    /// after a redo starts a fresh group.
    pub fn push_undo(&mut self, mut transaction: Transaction) {
        transaction.open = false;
        self.undo.push(transaction);
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selection;

    fn selections(head: usize) -> SelectionSet {
        SelectionSet::new(Selection::caret(CharOffset::new(head)))
    }

    fn range_selection(anchor: usize, head: usize) -> SelectionSet {
        SelectionSet::new(Selection::new(CharOffset::new(anchor), CharOffset::new(head)))
    }

    /// Types `text` one character at a time, as the editor would.
    fn type_text(history: &mut EditHistory, buffer: &mut TextBuffer, text: &str, start: usize) {
        let now = Instant::now();
        let mut position = CharOffset::new(start);
        for ch in text.chars() {
            let edit = Edit::insert(position, ch.to_string());
            let before = selections(position.get());
            edit.apply(buffer);
            position = edit.end_after_apply();
            history.record(edit, EditKind::Typing, &before, &selections(position.get()), now);
        }
    }

    #[test]
    fn an_edit_and_its_inverse_cancel_out() {
        let mut buffer = TextBuffer::from_str("hello world");
        let edit = Edit::insert(CharOffset::new(5), " there");
        edit.apply(&mut buffer);
        assert_eq!(buffer.to_string(), "hello there world");
        edit.inverse().apply(&mut buffer);
        assert_eq!(buffer.to_string(), "hello world");
    }

    #[test]
    fn a_replacement_inverts_cleanly() {
        let mut buffer = TextBuffer::from_str("value = 1");
        let edit = Edit { at: CharOffset::new(8), removed: "1".into(), inserted: "42".into() };
        edit.apply(&mut buffer);
        assert_eq!(buffer.to_string(), "value = 42");
        edit.inverse().apply(&mut buffer);
        assert_eq!(buffer.to_string(), "value = 1");
    }

    #[test]
    fn transform_moves_positions_after_an_edit() {
        let edit = Edit::insert(CharOffset::new(5), "abc");
        assert_eq!(edit.transform(CharOffset::new(2)), CharOffset::new(2));
        assert_eq!(edit.transform(CharOffset::new(5)), CharOffset::new(5));
        assert_eq!(edit.transform(CharOffset::new(6)), CharOffset::new(9));

        let deletion = Edit::delete(CharOffset::new(5), "abc");
        assert_eq!(deletion.transform(CharOffset::new(4)), CharOffset::new(4));
        assert_eq!(deletion.transform(CharOffset::new(6)), CharOffset::new(5), "inside the cut");
        assert_eq!(deletion.transform(CharOffset::new(10)), CharOffset::new(7));
    }

    #[test]
    fn typing_a_word_is_one_undo_step() {
        let mut history = EditHistory::default();
        let mut buffer = TextBuffer::new();
        type_text(&mut history, &mut buffer, "hello", 0);

        assert_eq!(history.undo_depth(), 1, "five keystrokes, one group");
        let transaction = history.pop_undo().unwrap();
        assert_eq!(transaction.len(), 5);
        for edit in transaction.edits.iter().rev() {
            edit.inverse().apply(&mut buffer);
        }
        assert_eq!(buffer.to_string(), "");
    }

    #[test]
    fn a_pause_longer_than_the_window_starts_a_new_group() {
        let mut history = EditHistory::new(Duration::from_millis(500));
        let start = Instant::now();
        let before = selections(0);
        let first = Edit::insert(CharOffset::ZERO, "a");
        history.record(first, EditKind::Typing, &before, &selections(1), start);

        let later = start + Duration::from_millis(501);
        let second = Edit::insert(CharOffset::new(1), "b");
        history.record(second, EditKind::Typing, &selections(1), &selections(2), later);

        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn a_cursor_jump_breaks_the_group() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        // The caret moved elsewhere before the next keystroke.
        history.record(
            Edit::insert(CharOffset::new(20), "b"),
            EditKind::Typing,
            &selections(20),
            &selections(21),
            now,
        );
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn a_selection_breaks_the_group() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        history.record(
            Edit { at: CharOffset::new(1), removed: "xyz".into(), inserted: "b".into() },
            EditKind::Typing,
            &range_selection(1, 4),
            &selections(2),
            now,
        );
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn different_kinds_never_coalesce() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        history.record(
            Edit::delete(CharOffset::ZERO, "a"),
            EditKind::Backspace,
            &selections(1),
            &selections(0),
            now,
        );
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn pasting_is_never_coalesced() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        for index in 0..3 {
            history.record(
                Edit::insert(CharOffset::new(index * 4), "text"),
                EditKind::Paste,
                &selections(index * 4),
                &selections(index * 4 + 4),
                now,
            );
        }
        assert_eq!(history.undo_depth(), 3);
    }

    #[test]
    fn consecutive_backspaces_group_together() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        for position in (1..=4).rev() {
            history.record(
                Edit::delete(CharOffset::new(position - 1), "x"),
                EditKind::Backspace,
                &selections(position),
                &selections(position - 1),
                now,
            );
        }
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.pop_undo().unwrap().len(), 4);
    }

    #[test]
    fn a_line_break_closes_the_group() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "hi"),
            EditKind::Typing,
            &selections(0),
            &selections(2),
            now,
        );
        history.record(
            Edit::insert(CharOffset::new(2), "\n"),
            EditKind::Typing,
            &selections(2),
            &selections(3),
            now,
        );
        history.record(
            Edit::insert(CharOffset::new(3), "next"),
            EditKind::Typing,
            &selections(3),
            &selections(7),
            now,
        );
        assert_eq!(history.undo_depth(), 2, "the newline joins the first group and closes it");
    }

    #[test]
    fn force_boundary_ends_a_group() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        history.force_boundary();
        history.record(
            Edit::insert(CharOffset::new(1), "b"),
            EditKind::Typing,
            &selections(1),
            &selections(2),
            now,
        );
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let mut history = EditHistory::default();
        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        let transaction = history.pop_undo().unwrap();
        history.push_redo(transaction);
        assert!(history.can_redo());

        history.record(
            Edit::insert(CharOffset::ZERO, "z"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        assert!(!history.can_redo(), "redo is not reachable after a new edit");
    }

    #[test]
    fn a_group_is_one_step_even_with_many_edits() {
        let mut history = EditHistory::default();
        let edits = vec![
            Edit { at: CharOffset::new(10), removed: "old".into(), inserted: "new".into() },
            Edit { at: CharOffset::new(0), removed: "old".into(), inserted: "new".into() },
        ];
        history.record_group(
            edits,
            EditKind::Programmatic,
            &selections(0),
            &selections(3),
            Instant::now(),
        );
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.pop_undo().unwrap().len(), 2);
    }

    #[test]
    fn the_state_token_identifies_the_content_state() {
        let mut history = EditHistory::default();
        assert_eq!(history.state_token(), TransactionId::ROOT);

        let now = Instant::now();
        history.record(
            Edit::insert(CharOffset::ZERO, "a"),
            EditKind::Typing,
            &selections(0),
            &selections(1),
            now,
        );
        let after_first = history.state_token();
        assert_ne!(after_first, TransactionId::ROOT);

        let transaction = history.pop_undo().unwrap();
        assert_eq!(history.state_token(), TransactionId::ROOT, "undo returns to the old state");
        history.push_undo(transaction);
        assert_eq!(history.state_token(), after_first, "redo returns to the new state");
    }
}
