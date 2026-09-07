//! The `Document` (specification sections 14, 22, 25).
//!
//! A document owns its text, its cursor, its history and its metadata, and it
//! is the only thing allowed to mutate document content. Its three state
//! dimensions are deliberately independent (specification section 25):
//!
//! ```text
//! ContentState      Clean | Dirty
//! ExternalState     Unchanged | ExternallyChanged | Missing | Conflict
//! PersistenceState  Idle | Saving | SaveSucceeded | SaveFailed
//! ```
//!
//! A document being saved is not "less dirty", and a document changed on disk
//! is not "less saved"; collapsing those into one enum is what makes editors
//! lose work.

use crate::encoding::Encoding;
use crate::history::{Edit, EditHistory, EditKind, TransactionId};
use crate::language::Language;
use crate::render::Invalidation;
use crate::selection::{self, Movement, MovementContext, Selection, SelectionSet};
use ls_buffer::{unicode, CharOffset, LineEnding, LineIndex, TextBuffer};
use ls_platform::CanonicalPath;
use std::time::{Duration, Instant, SystemTime};

/// Identity of an open document.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(u64);

impl DocumentId {
    pub const fn new(value: u64) -> Self {
        DocumentId(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "doc#{}", self.0)
    }
}

/// Monotonic content version (specification section 22).
///
/// Every successful mutation increments it, undo included: the revision
/// identifies *which* content an asynchronous observer saw, so it must never
/// move backwards even when the text returns to an earlier state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ContentRevision(u64);

impl ContentRevision {
    pub const fn get(self) -> u64 {
        self.0
    }
    fn next(self) -> Self {
        ContentRevision(self.0 + 1)
    }
}

impl std::fmt::Display for ContentRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContentState {
    Clean,
    Dirty,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExternalState {
    Unchanged,
    ExternallyChanged,
    Missing,
    /// Changed on disk while the buffer also has unsaved edits.
    Conflict,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PersistenceState {
    Idle,
    Saving,
    SaveSucceeded,
    SaveFailed,
}

/// What the file looked like on disk when it was last read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    pub modified: Option<SystemTime>,
    pub len_bytes: u64,
}

/// Per-document settings taken from the effective configuration.
#[derive(Copy, Clone, Debug)]
pub struct DocumentSettings {
    pub tab_width: usize,
    pub coalesce_window: Duration,
    /// Characters inserted by the Tab key.
    pub insert_spaces: bool,
}

impl Default for DocumentSettings {
    fn default() -> Self {
        DocumentSettings {
            tab_width: 4,
            coalesce_window: crate::history::DEFAULT_COALESCE_WINDOW,
            insert_spaces: true,
        }
    }
}

/// Outcome of a content mutation.
#[derive(Clone, Debug)]
pub struct EditResult {
    pub revision: ContentRevision,
    pub invalidation: Invalidation,
}

/// One open document.
#[derive(Debug)]
pub struct Document {
    id: DocumentId,
    path: Option<CanonicalPath>,
    display_name: String,
    buffer: TextBuffer,
    encoding: Encoding,
    line_ending: LineEnding,
    mixed_line_endings: bool,
    language: Language,
    revision: ContentRevision,
    history: EditHistory,
    selections: SelectionSet,
    content_state: ContentState,
    external_state: ExternalState,
    persistence_state: PersistenceState,
    saved_token: TransactionId,
    disk_stamp: Option<DiskStamp>,
    settings: DocumentSettings,
    pending_invalidation: Invalidation,
    find: crate::search::FindState,
    /// From an external tool (item 9: LSP diagnostics). Empty unless
    /// something outside the editor -- currently only the LSP client --
    /// reported something about this document.
    diagnostics: Vec<crate::render::Diagnostic>,
    /// Incremental syntax-lexing state (item 8): `lex_states[i]` is the state
    /// line `i` *exits* with, computed lazily up to however far rendering has
    /// asked. Truncated from the edited line forward on every edit, so a
    /// keystroke invalidates what follows it, never the whole document.
    lex_states: Vec<crate::highlight::LexState>,
}

impl Document {
    /// A new empty document with no file behind it.
    pub fn untitled(id: DocumentId, name: impl Into<String>, settings: DocumentSettings) -> Self {
        Document {
            id,
            path: None,
            display_name: name.into(),
            buffer: TextBuffer::new(),
            encoding: Encoding::Utf8,
            line_ending: LineEnding::platform_default(),
            mixed_line_endings: false,
            language: Language::PlainText,
            revision: ContentRevision::default(),
            history: EditHistory::new(settings.coalesce_window),
            selections: SelectionSet::default(),
            content_state: ContentState::Clean,
            external_state: ExternalState::Unchanged,
            persistence_state: PersistenceState::Idle,
            saved_token: TransactionId::ROOT,
            disk_stamp: None,
            settings,
            pending_invalidation: Invalidation::everything(),
            find: crate::search::FindState::default(),
            diagnostics: Vec::new(),
            lex_states: Vec::new(),
        }
    }

    /// A document constructed from a file that has already been read, decoded
    /// and normalized (specification section 24).
    #[allow(clippy::too_many_arguments)]
    pub fn loaded(
        id: DocumentId,
        path: CanonicalPath,
        text: &str,
        encoding: Encoding,
        line_ending: LineEnding,
        mixed_line_endings: bool,
        stamp: DiskStamp,
        settings: DocumentSettings,
    ) -> Self {
        Self::from_buffer(
            id,
            path,
            TextBuffer::from_str(text),
            encoding,
            line_ending,
            mixed_line_endings,
            stamp,
            settings,
        )
    }

    /// Builds a document around a buffer that already exists.
    ///
    /// Asynchronous loads use this: the rope is built on the scheduler worker
    /// (amendment section 7), so the interactive thread only has to take
    /// ownership of it. Building a 100 MB rope inline would be a ~110 ms frame.
    #[allow(clippy::too_many_arguments)]
    pub fn from_buffer(
        id: DocumentId,
        path: CanonicalPath,
        buffer: TextBuffer,
        encoding: Encoding,
        line_ending: LineEnding,
        mixed_line_endings: bool,
        stamp: DiskStamp,
        settings: DocumentSettings,
    ) -> Self {
        let language = crate::language::detect_language(path.as_path());
        Document {
            id,
            display_name: path.file_name(),
            path: Some(path),
            buffer,
            encoding,
            line_ending,
            mixed_line_endings,
            language,
            revision: ContentRevision::default(),
            history: EditHistory::new(settings.coalesce_window),
            selections: SelectionSet::default(),
            content_state: ContentState::Clean,
            external_state: ExternalState::Unchanged,
            persistence_state: PersistenceState::Idle,
            saved_token: TransactionId::ROOT,
            disk_stamp: Some(stamp),
            settings,
            pending_invalidation: Invalidation::everything(),
            find: crate::search::FindState::default(),
            diagnostics: Vec::new(),
            lex_states: Vec::new(),
        }
    }

    pub fn id(&self) -> DocumentId {
        self.id
    }

    pub fn path(&self) -> Option<&CanonicalPath> {
        self.path.as_ref()
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn text(&self) -> &TextBuffer {
        &self.buffer
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, ending: LineEnding) {
        self.line_ending = ending;
    }

    pub fn has_mixed_line_endings(&self) -> bool {
        self.mixed_line_endings
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn revision(&self) -> ContentRevision {
        self.revision
    }

    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }

    pub fn find(&self) -> &crate::search::FindState {
        &self.find
    }

    /// Drops cached lex states from the first changed line onward, so the
    /// next render re-lexes only what could actually have changed -- a block
    /// comment's extent can only be affected by edits at or after where it
    /// starts.
    fn invalidate_lex_states(&mut self, invalidation: &Invalidation) {
        if let Some(range) = &invalidation.text_lines {
            self.lex_states.truncate(range.start.min(self.lex_states.len()));
        }
    }

    /// Tokens for one line, extending the incremental lex-state cache
    /// forward as needed.
    ///
    /// Bounded by how far down the document rendering has ever needed to
    /// look, not by the document's length: the first time a viewport reaches
    /// a given line, every line above it that is not already cached gets
    /// lexed once to learn whether it leaves a block comment open; after
    /// that, every subsequent frame reuses the cached exit states and
    /// re-lexes only the lines actually on screen.
    pub fn tokenize_visible_line(&mut self, line_index: usize) -> Vec<crate::highlight::Token> {
        use crate::highlight::{tokenize_line, LexState};
        while self.lex_states.len() < line_index {
            let index = self.lex_states.len();
            let entering = self.lex_states.last().copied().unwrap_or_default();
            let text = self.buffer.line_text(LineIndex::new(index));
            let (_, exiting) = tokenize_line(&text, self.language, entering);
            self.lex_states.push(exiting);
        }
        let entering =
            if line_index == 0 { LexState::default() } else { self.lex_states[line_index - 1] };
        let text = self.buffer.line_text(LineIndex::new(line_index));
        let (tokens, exiting) = tokenize_line(&text, self.language, entering);
        if self.lex_states.len() == line_index {
            self.lex_states.push(exiting);
        } else {
            self.lex_states[line_index] = exiting;
        }
        tokens
    }

    pub fn diagnostics(&self) -> &[crate::render::Diagnostic] {
        &self.diagnostics
    }

    /// Replaces this document's diagnostics wholesale -- an LSP server always
    /// reports the complete current set for a file, never a delta, so there
    /// is nothing to merge.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<crate::render::Diagnostic>) {
        self.diagnostics = diagnostics;
    }

    /// Replaces the find query and recomputes matches, jumping to the nearest
    /// one at or after the primary cursor.
    pub fn set_find_query(&mut self, query: String) {
        let from = self.buffer.char_to_line(self.selections.primary().head);
        self.find.set_query(query, &self.buffer, from);
    }

    pub fn clear_find(&mut self) {
        self.find.clear();
    }

    pub fn advance_find(&mut self, delta: isize) {
        self.find.advance(delta);
    }

    /// Selects the find state's current match, so it is drawn with the
    /// ordinary selection highlight and the caret lands on it -- one
    /// mechanism for "here is the match you're on", reusing what already
    /// exists rather than adding a second kind of highlight.
    pub fn select_current_find_match(&mut self) {
        let Some(found) = self.find.current_match() else { return };
        self.move_to(found.line, found.start_column_chars, false);
        self.move_to(found.line, found.end_column_chars, true);
    }

    pub fn content_state(&self) -> ContentState {
        self.content_state
    }

    pub fn external_state(&self) -> ExternalState {
        self.external_state
    }

    pub fn persistence_state(&self) -> PersistenceState {
        self.persistence_state
    }

    pub fn is_dirty(&self) -> bool {
        self.content_state == ContentState::Dirty
    }

    pub fn settings(&self) -> DocumentSettings {
        self.settings
    }


    /// Adopts new editor settings after a configuration change.
    pub fn apply_settings(&mut self, settings: DocumentSettings) {
        self.settings = settings;
        self.history.set_coalesce_window(settings.coalesce_window);
        // Tab width changes how every line measures, so all layout is stale.
        self.invalidate_all();
    }

    pub fn disk_stamp(&self) -> Option<&DiskStamp> {
        self.disk_stamp.as_ref()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    pub fn redo_depth(&self) -> usize {
        self.history.redo_depth()
    }

    pub fn movement_context(&self, page_lines: usize) -> MovementContext {
        MovementContext { tab_width: self.settings.tab_width, page_lines }
    }

    /// Text covered by the primary selection.
    pub fn selected_text(&self) -> String {
        let range = self.selections.primary().range();
        if range.start == range.end {
            String::new()
        } else {
            self.buffer.slice(range)
        }
    }

    /// Takes the accumulated invalidation and resets it. Called when a snapshot
    /// is built (specification section 28).
    pub fn take_invalidation(&mut self) -> Invalidation {
        std::mem::take(&mut self.pending_invalidation)
    }

    pub fn peek_invalidation(&self) -> &Invalidation {
        &self.pending_invalidation
    }

    pub fn invalidate_all(&mut self) {
        self.pending_invalidation.merge(Invalidation::everything());
    }

    // --- editing -------------------------------------------------------------

    /// Applies one edit as part of `kind`, updating history, revision, cursor
    /// and dirty state. This is the single funnel every mutation goes through.
    pub fn apply_edit(
        &mut self,
        edit: Edit,
        kind: EditKind,
        selection_after: SelectionSet,
    ) -> EditResult {
        self.apply_edits(vec![edit], kind, selection_after)
    }

    /// Applies several edits as one undo step. Edits must be ordered from the
    /// end of the document backwards, so earlier edits do not shift later ones.
    pub fn apply_edits(
        &mut self,
        edits: Vec<Edit>,
        kind: EditKind,
        selection_after: SelectionSet,
    ) -> EditResult {
        let before = self.selections.clone();
        let mut invalidation = Invalidation::default();
        for edit in &edits {
            invalidation.merge(self.invalidation_for(edit));
            edit.apply(&mut self.buffer);
        }

        let now = Instant::now();
        if edits.len() == 1 {
            let edit = edits.into_iter().next().expect("length checked");
            self.history.record(edit, kind, &before, &selection_after, now);
        } else {
            self.history.record_group(edits, kind, &before, &selection_after, now);
        }

        self.selections = selection_after;
        self.revision = self.revision.next();
        self.refresh_content_state();
        self.invalidate_lex_states(&invalidation);
        self.pending_invalidation.merge(invalidation.clone());
        EditResult { revision: self.revision, invalidation }
    }

    /// Inserts text at the caret, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str, kind: EditKind) -> EditResult {
        let selection = self.selections.primary();
        let range = selection.range();
        let removed =
            if selection.is_caret() { String::new() } else { self.buffer.slice(range.clone()) };
        let kind = if removed.is_empty() { kind } else { EditKind::ReplaceSelection };
        let edit = Edit { at: range.start, removed: removed.into(), inserted: text.into() };
        let caret = edit.end_after_apply();
        self.apply_edit(edit, kind, SelectionSet::new(Selection::caret(caret)))
    }

    /// Removes up to one indent step from every line the selection touches
    /// (or just the caret's line, for a plain caret) -- Shift+Tab.
    ///
    /// A line indented with a tab loses that one tab; a line indented with
    /// spaces loses up to `tab_width` of them; a line with no leading
    /// whitespace is left alone. Edits are built from the last touched line
    /// to the first, which is what lets them apply directly to the
    /// pre-edit buffer without adjusting offsets for edits already made on
    /// earlier lines (the requirement `apply_edits` already documents).
    pub fn dedent(&mut self) -> Option<EditResult> {
        let selection = self.selections.primary();
        let start_line = self.buffer.char_to_line(selection.start()).get();
        let end_line = self.buffer.char_to_line(selection.end()).get();
        let tab_width = self.settings.tab_width;

        let mut edits = Vec::new();
        let mut removed_from_anchor_line = 0usize;
        let mut removed_from_head_line = 0usize;
        for line_number in (start_line..=end_line).rev() {
            let line = LineIndex::new(line_number);
            let line_start = self.buffer.line_range(line).start;
            let text = self.buffer.line_text(line);

            let removed = if text.starts_with('\t') {
                1
            } else {
                text.chars().take(tab_width).take_while(|&c| c == ' ').count()
            };
            if removed == 0 {
                continue;
            }
            let removed_text: String = text.chars().take(removed).collect();
            edits.push(Edit::delete(line_start, removed_text));

            if line_number == self.buffer.char_to_line(selection.anchor).get() {
                removed_from_anchor_line = removed;
            }
            if line_number == self.buffer.char_to_line(selection.head).get() {
                removed_from_head_line = removed;
            }
        }
        if edits.is_empty() {
            return None;
        }

        let anchor_line_start =
            self.buffer.line_range(self.buffer.char_to_line(selection.anchor)).start;
        let head_line_start =
            self.buffer.line_range(self.buffer.char_to_line(selection.head)).start;
        let anchor_column = selection.anchor.get().saturating_sub(anchor_line_start.get());
        let head_column = selection.head.get().saturating_sub(head_line_start.get());
        let new_anchor =
            selection.anchor.saturating_sub(removed_from_anchor_line.min(anchor_column));
        let new_head = selection.head.saturating_sub(removed_from_head_line.min(head_column));

        Some(self.apply_edits(
            edits,
            EditKind::Programmatic,
            SelectionSet::new(Selection { anchor: new_anchor, head: new_head, goal_column: None }),
        ))
    }

    /// Deletes the selection, or the grapheme before the caret.
    pub fn backspace(&mut self) -> Option<EditResult> {
        let selection = self.selections.primary();
        if !selection.is_caret() {
            return Some(self.delete_selection());
        }
        let head = selection.head;
        if head == CharOffset::ZERO {
            return None;
        }
        let start = unicode::prev_grapheme_boundary(&self.buffer, head);
        let removed = self.buffer.slice(start..head);
        let edit = Edit::delete(start, removed);
        Some(self.apply_edit(edit, EditKind::Backspace, SelectionSet::new(Selection::caret(start))))
    }

    /// Deletes the selection, or the grapheme after the caret.
    pub fn delete_forward(&mut self) -> Option<EditResult> {
        let selection = self.selections.primary();
        if !selection.is_caret() {
            return Some(self.delete_selection());
        }
        let head = selection.head;
        if head == self.buffer.end() {
            return None;
        }
        let end = unicode::next_grapheme_boundary(&self.buffer, head);
        let removed = self.buffer.slice(head..end);
        let edit = Edit::delete(head, removed);
        Some(self.apply_edit(
            edit,
            EditKind::DeleteForward,
            SelectionSet::new(Selection::caret(head)),
        ))
    }

    /// Deletes the current selection. Assumes there is one.
    pub fn delete_selection(&mut self) -> EditResult {
        let range = self.selections.primary().range();
        let removed = self.buffer.slice(range.clone());
        let edit = Edit::delete(range.start, removed);
        self.apply_edit(
            edit,
            EditKind::ReplaceSelection,
            SelectionSet::new(Selection::caret(range.start)),
        )
    }

    /// Deletes from the caret to the previous word boundary.
    pub fn delete_word_left(&mut self) -> Option<EditResult> {
        let selection = self.selections.primary();
        if !selection.is_caret() {
            return Some(self.delete_selection());
        }
        let context = self.movement_context(1);
        let target =
            selection::move_selection(&self.buffer, selection, Movement::WordLeft, false, context)
                .head;
        if target == selection.head {
            return None;
        }
        let removed = self.buffer.slice(target..selection.head);
        let edit = Edit::delete(target, removed);
        Some(self.apply_edit(
            edit,
            EditKind::Programmatic,
            SelectionSet::new(Selection::caret(target)),
        ))
    }

    /// Deletes from the caret to the next word boundary.
    pub fn delete_word_right(&mut self) -> Option<EditResult> {
        let selection = self.selections.primary();
        if !selection.is_caret() {
            return Some(self.delete_selection());
        }
        let context = self.movement_context(1);
        let target =
            selection::move_selection(&self.buffer, selection, Movement::WordRight, false, context)
                .head;
        if target == selection.head {
            return None;
        }
        let removed = self.buffer.slice(selection.head..target);
        let edit = Edit::delete(selection.head, removed);
        Some(self.apply_edit(
            edit,
            EditKind::Programmatic,
            SelectionSet::new(Selection::caret(selection.head)),
        ))
    }

    /// Reverses the most recent transaction.
    pub fn undo(&mut self) -> Option<EditResult> {
        let transaction = self.history.pop_undo()?;
        let mut invalidation = Invalidation::default();
        for edit in transaction.edits.iter().rev() {
            let inverse = edit.inverse();
            invalidation.merge(self.invalidation_for(&inverse));
            inverse.apply(&mut self.buffer);
        }
        self.selections = transaction.before.clone();
        self.history.push_redo(transaction);
        // Undo is a mutation like any other: the revision moves forward.
        self.revision = self.revision.next();
        self.refresh_content_state();
        self.invalidate_lex_states(&invalidation);
        self.pending_invalidation.merge(invalidation.clone());
        Some(EditResult { revision: self.revision, invalidation })
    }

    /// Re-applies the most recently undone transaction.
    pub fn redo(&mut self) -> Option<EditResult> {
        let transaction = self.history.pop_redo()?;
        let mut invalidation = Invalidation::default();
        for edit in &transaction.edits {
            invalidation.merge(self.invalidation_for(edit));
            edit.apply(&mut self.buffer);
        }
        self.selections = transaction.after.clone();
        self.history.push_undo(transaction);
        self.revision = self.revision.next();
        self.refresh_content_state();
        self.invalidate_lex_states(&invalidation);
        self.pending_invalidation.merge(invalidation.clone());
        Some(EditResult { revision: self.revision, invalidation })
    }

    // --- cursor --------------------------------------------------------------

    /// Moves the caret. Cursor movement never changes content, so it never
    /// touches the revision - but it does end the current undo group.
    pub fn move_cursor(&mut self, movement: Movement, extend: bool, page_lines: usize) {
        let context = self.movement_context(page_lines);
        let previous = self.selections.primary();
        let moved = selection::move_selection(&self.buffer, previous, movement, extend, context);
        self.set_selection_internal(moved, previous);
    }

    pub fn set_selection(&mut self, selection: Selection) {
        let previous = self.selections.primary();
        self.set_selection_internal(selection.clamped(&self.buffer), previous);
    }

    fn set_selection_internal(&mut self, next: Selection, previous: Selection) {
        if next == previous {
            return;
        }
        // A cursor jump or selection change forces an undo boundary
        // (specification section 23).
        if next.head != previous.head || next.is_caret() != previous.is_caret() {
            self.history.force_boundary();
        }
        let old_line = self.buffer.char_to_line(previous.head).get();
        let new_line = self.buffer.char_to_line(next.head).get();
        self.selections.set_primary(next);
        self.pending_invalidation.merge(Invalidation::cursor(old_line, new_line));
        if !next.is_caret() || !previous.is_caret() {
            let start = self.buffer.char_to_line(next.start().min_value(previous.start())).get();
            let end = self.buffer.char_to_line(next.end().max_value(previous.end())).get();
            self.pending_invalidation.merge(Invalidation::selection(start, end + 1));
        }
    }

    pub fn select_all(&mut self) {
        let all = Selection::new(CharOffset::ZERO, self.buffer.end());
        self.set_selection(all);
    }

    /// Places the caret at a line and column, as "go to line" does.
    pub fn move_to(&mut self, line: LineIndex, column: usize, extend: bool) {
        let target = self.buffer.position_at(line, column);
        let previous = self.selections.primary();
        let next = if extend {
            Selection { anchor: previous.anchor, head: target, goal_column: None }
        } else {
            Selection::caret(target)
        };
        self.set_selection(next);
    }

    // --- persistence ---------------------------------------------------------

    /// Begins a save.
    ///
    /// Closing the open edit group happens here rather than at completion: the
    /// user's Save press is the coalescing boundary (baseline section 23), and
    /// a completion must never mutate history (amendment section 8). Closing a
    /// group does not change `state_token`, so a token captured now stays valid
    /// until the save lands.
    pub fn mark_saving(&mut self) {
        self.history.force_boundary();
        self.persistence_state = PersistenceState::Saving;
    }

    /// Records a successful save of the state the document is in right now.
    pub fn mark_saved(&mut self, path: CanonicalPath, stamp: DiskStamp) {
        let token = self.history.state_token();
        self.mark_saved_at(path, stamp, token);
    }

    /// Records a successful save of the state identified by `saved_token`.
    ///
    /// This is the asynchronous form: the token was captured when the save
    /// started, and the document may have moved on since. Clean/dirty is
    /// decided by comparing that captured token against the current one
    /// (amendment section 8.1), so a save that lands stale leaves the document
    /// dirty while still recording that the file on disk is now current.
    ///
    /// It mutates no text, cursor, selection, revision or history.
    pub fn mark_saved_at(
        &mut self,
        path: CanonicalPath,
        stamp: DiskStamp,
        saved_token: TransactionId,
    ) {
        self.display_name = path.file_name();
        self.language = crate::language::detect_language(path.as_path());
        self.path = Some(path);
        self.disk_stamp = Some(stamp);
        self.saved_token = saved_token;
        self.content_state = if self.history.state_token() == saved_token {
            ContentState::Clean
        } else {
            ContentState::Dirty
        };
        self.external_state = ExternalState::Unchanged;
        self.persistence_state = PersistenceState::SaveSucceeded;
    }

    /// The history position the document is currently at.
    pub fn transaction_token(&self) -> TransactionId {
        self.history.state_token()
    }

    /// The history position last written to disk.
    pub fn saved_token(&self) -> TransactionId {
        self.saved_token
    }

    pub fn mark_save_failed(&mut self) {
        self.persistence_state = PersistenceState::SaveFailed;
    }

    pub fn set_external_state(&mut self, state: ExternalState) {
        self.external_state = state;
    }

    /// Recomputes clean/dirty from the history position, so that undoing back to
    /// the saved state marks the document clean again.
    fn refresh_content_state(&mut self) {
        self.content_state = if self.history.state_token() == self.saved_token {
            ContentState::Clean
        } else {
            ContentState::Dirty
        };
        if self.persistence_state == PersistenceState::SaveSucceeded {
            self.persistence_state = PersistenceState::Idle;
        }
    }

    /// Which lines an edit invalidates (specification section 28).
    fn invalidation_for(&self, edit: &Edit) -> Invalidation {
        let line = self.buffer.char_to_line(self.buffer.clamp(edit.at)).get();
        let structural = edit.inserted.contains('\n') || edit.removed.contains('\n');
        if structural {
            // Line numbering below the edit shifts, so everything after it is stale.
            Invalidation::text_from(line)
        } else {
            Invalidation::text(line, line + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        let mut document =
            Document::untitled(DocumentId::new(1), "untitled-1", DocumentSettings::default());
        if !text.is_empty() {
            document.insert(text, EditKind::Paste);
            document.history.clear();
            document.revision = ContentRevision::default();
            document.content_state = ContentState::Clean;
            document.set_selection(Selection::caret(CharOffset::ZERO));
        }
        document
    }

    fn text_of(document: &Document) -> String {
        document.text().to_string()
    }

    #[test]
    fn a_new_document_is_clean_and_empty() {
        let document = document("");
        assert_eq!(document.content_state(), ContentState::Clean);
        assert_eq!(document.external_state(), ExternalState::Unchanged);
        assert_eq!(document.persistence_state(), PersistenceState::Idle);
        assert_eq!(document.revision(), ContentRevision::default());
        assert!(!document.can_undo());
        assert!(document.text().is_empty());
    }

    #[test]
    fn typing_advances_the_revision_and_dirties_the_document() {
        let mut document = document("");
        document.insert("a", EditKind::Typing);
        assert_eq!(text_of(&document), "a");
        assert_eq!(document.revision().get(), 1);
        assert!(document.is_dirty());
    }

    #[test]
    fn undo_advances_the_revision_rather_than_rewinding_it() {
        let mut document = document("");
        document.insert("hello", EditKind::Paste);
        let after_edit = document.revision();
        document.undo().unwrap();
        assert!(
            document.revision() > after_edit,
            "revision must not move backwards on undo (specification section 22)"
        );
        assert_eq!(text_of(&document), "");
    }

    #[test]
    fn undo_and_redo_restore_content_and_selection() {
        let mut document = document("start");
        document.set_selection(Selection::caret(CharOffset::new(5)));
        document.insert(" more", EditKind::Typing);
        assert_eq!(text_of(&document), "start more");

        document.undo().unwrap();
        assert_eq!(text_of(&document), "start");
        assert_eq!(document.selections().primary().head, CharOffset::new(5));

        document.redo().unwrap();
        assert_eq!(text_of(&document), "start more");
        assert_eq!(document.selections().primary().head, CharOffset::new(10));
    }

    #[test]
    fn undo_with_nothing_to_undo_is_a_no_op() {
        let mut document = document("x");
        assert!(document.undo().is_none());
        assert!(document.redo().is_none());
    }

    #[test]
    fn typing_over_a_selection_replaces_it_in_one_step() {
        let mut document = document("hello world");
        document.set_selection(Selection::new(CharOffset::new(0), CharOffset::new(5)));
        document.insert("goodbye", EditKind::Typing);
        assert_eq!(text_of(&document), "goodbye world");
        assert_eq!(document.selections().primary().head, CharOffset::new(7));

        document.undo().unwrap();
        assert_eq!(text_of(&document), "hello world");
        assert_eq!(
            document.selections().primary().range(),
            CharOffset::new(0)..CharOffset::new(5),
            "undo restores the selection that was replaced"
        );
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let mut document = document("a\u{1F469}\u{200D}\u{1F467}");
        document.set_selection(Selection::caret(document.text().end()));
        document.backspace().unwrap();
        assert_eq!(text_of(&document), "a");
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut document = document("abc");
        document.set_selection(Selection::caret(CharOffset::ZERO));
        assert!(document.backspace().is_none());
        assert_eq!(text_of(&document), "abc");
    }

    #[test]
    fn delete_forward_at_the_end_does_nothing() {
        let mut document = document("abc");
        document.set_selection(Selection::caret(document.text().end()));
        assert!(document.delete_forward().is_none());
    }

    #[test]
    fn delete_word_left_removes_the_previous_word() {
        let mut document = document("one two three");
        document.set_selection(Selection::caret(CharOffset::new(13)));
        document.delete_word_left().unwrap();
        assert_eq!(text_of(&document), "one two ");
    }

    #[test]
    fn delete_word_right_removes_the_next_word() {
        let mut document = document("one two three");
        document.set_selection(Selection::caret(CharOffset::ZERO));
        document.delete_word_right().unwrap();
        assert_eq!(text_of(&document), " two three");
    }

    #[test]
    fn typing_then_undo_returns_to_the_clean_state() {
        let mut document = document("saved content");
        document.mark_saved(
            CanonicalPath::unverified(if cfg!(windows) { r"C:\a.txt" } else { "/a.txt" }).unwrap(),
            DiskStamp { modified: None, len_bytes: 13 },
        );
        assert!(!document.is_dirty());

        document.insert("!", EditKind::Typing);
        assert!(document.is_dirty());

        document.undo().unwrap();
        assert!(!document.is_dirty(), "undoing back to the saved state is clean again");
    }

    #[test]
    fn saving_keeps_content_and_external_state_independent() {
        let mut document = document("content");
        document.insert("!", EditKind::Typing);
        document.set_external_state(ExternalState::ExternallyChanged);
        document.mark_saving();

        assert!(document.is_dirty(), "a save in progress does not clean the content");
        assert_eq!(document.persistence_state(), PersistenceState::Saving);
        assert_eq!(document.external_state(), ExternalState::ExternallyChanged);
    }

    #[test]
    fn a_single_line_edit_invalidates_one_line() {
        let mut document = document("one\ntwo\nthree");
        document.take_invalidation();
        document.set_selection(Selection::caret(CharOffset::new(5)));
        document.take_invalidation();

        let result = document.insert("X", EditKind::Typing);
        assert_eq!(
            result.invalidation.text_lines,
            Some(1..2),
            "editing line 1 must not invalidate the whole document"
        );
    }

    #[test]
    fn an_edit_that_adds_a_line_invalidates_everything_below() {
        let mut document = document("one\ntwo\nthree");
        document.set_selection(Selection::caret(CharOffset::new(5)));
        let result = document.insert("\n", EditKind::Typing);
        let lines = result.invalidation.text_lines.unwrap();
        assert_eq!(lines.start, 1);
        assert_eq!(lines.end, usize::MAX, "line numbering below the edit shifted");
    }

    #[test]
    fn cursor_movement_does_not_change_the_revision() {
        let mut document = document("one\ntwo");
        let revision = document.revision();
        document.move_cursor(Movement::CharRight, false, 10);
        document.move_cursor(Movement::LineDown, false, 10);
        assert_eq!(document.revision(), revision);
        assert!(!document.is_dirty());
    }

    #[test]
    fn select_all_covers_the_document() {
        let mut document = document("abc\ndef");
        document.select_all();
        assert_eq!(document.selected_text(), "abc\ndef");
    }

    #[test]
    fn move_to_places_the_caret_at_a_line_and_column() {
        let mut document = document("first\nsecond\nthird");
        document.move_to(LineIndex::new(1), 3, false);
        assert_eq!(document.selections().primary().head, CharOffset::new(9));
        // A column past the end of the line clamps to the line end.
        document.move_to(LineIndex::new(0), 99, false);
        assert_eq!(document.selections().primary().head, CharOffset::new(5));
    }

    #[test]
    fn typing_is_grouped_but_a_cursor_jump_splits_the_group() {
        let mut document = document("");
        for ch in "hello".chars() {
            document.insert(&ch.to_string(), EditKind::Typing);
        }
        assert_eq!(document.undo_depth(), 1);

        document.set_selection(Selection::caret(CharOffset::ZERO));
        document.insert("X", EditKind::Typing);
        assert_eq!(document.undo_depth(), 2);

        document.undo().unwrap();
        assert_eq!(text_of(&document), "hello");
        document.undo().unwrap();
        assert_eq!(text_of(&document), "");
    }

    #[test]
    fn edits_are_deterministic_across_runs() {
        // Specification section 25: the same script must rebuild the same state.
        let run = || {
            let mut document = document("");
            for ch in "fn main() {}".chars() {
                document.insert(&ch.to_string(), EditKind::Typing);
            }
            document.set_selection(Selection::caret(CharOffset::new(3)));
            document.insert("very_long_name", EditKind::Typing);
            document.backspace();
            document.undo();
            (text_of(&document), document.revision().get())
        };
        assert_eq!(run(), run());
    }
}
