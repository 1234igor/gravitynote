//! Undo / redo over the note buffer.
//!
//! The whole note is one `String`, and it can be big — 20 years of daily notes
//! is ~10 MB and ~200k lines. So the history stores **only the spans that
//! changed**: an [`Edit`] holds the bytes that were removed, the bytes that
//! replaced them, and the byte offset they sit at. Nothing here ever holds a
//! copy of the document, so a thousand undo steps of ordinary typing costs a
//! few kilobytes rather than gigabytes.
//!
//! Edits are recorded *after* the caller has already applied them to the
//! buffer. [`History::record`] never touches the text; [`History::undo`] and
//! [`History::redo`] do, by `replace_range`-ing the inverse (or the original)
//! span back in.
//!
//! # Grouping
//!
//! A user's "one undo" is rarely one keystroke. Consecutive typing merges into
//! a single group, so ⌘Z takes back a word rather than a letter, and each line
//! ends up its own undo step. The exact rules are in [`History::record`]; the
//! short version is *same kind, adjacent, recent, and not a paste*.
//!
//! # Desynchronisation
//!
//! `undo` and `redo` are told the buffer to apply to, and the caller could
//! hand over a buffer the history was never recorded against (a reload from
//! disk, a note swap, a bug). Every span is checked against the text it claims
//! to be sitting in *before* anything is written, and a group that has already
//! partly applied is rolled back exactly. A mismatch returns `None`, clears the
//! history, and leaves the buffer byte-for-byte untouched — never a panic and
//! never a corrupted document. (Redoing a pure *insert* has no bytes to compare
//! against, so only its bounds and `char` boundaries can be checked; that is
//! enough to keep the buffer valid UTF-8 and the process alive, which is what
//! the guarantee is for. Call [`History::clear`] on a reload rather than
//! relying on the guard.)
//!
//! This module is pure logic: no `gpui`, no I/O, `std` only.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A single applied change: `removed` used to sit at `start`, `inserted` is
/// there now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub removed: String,
    pub inserted: String,
    /// Selection (start, end) before the edit — restored on undo.
    pub before: (usize, usize),
    /// Selection (start, end) after the edit — restored on redo.
    pub after: (usize, usize),
}

impl Edit {
    /// The single span in which two versions of the buffer differ.
    ///
    /// A structural change — moving a note to the top, starting a new one —
    /// rewrites the whole buffer, but only a run in the middle actually moved.
    /// Recording that run keeps undo a span rather than a pair of copies of the
    /// document, which on a twenty-year note is the difference between a few
    /// hundred bytes of history and eighteen megabytes of it.
    ///
    /// `None` when the two are identical.
    pub fn between(
        before: &str,
        after: &str,
        before_selection: (usize, usize),
        after_selection: (usize, usize),
    ) -> Option<Edit> {
        if before == after {
            return None;
        }
        let (b, a) = (before.as_bytes(), after.as_bytes());
        let common = b.len().min(a.len());

        let mut start = 0;
        while start < common && b[start] == a[start] {
            start += 1;
        }
        // Back off to a boundary both sides agree on, so the recorded strings
        // are always whole characters.
        while start > 0 && !(before.is_char_boundary(start) && after.is_char_boundary(start)) {
            start -= 1;
        }

        let mut tail = 0;
        while tail < common - start && b[b.len() - 1 - tail] == a[a.len() - 1 - tail] {
            tail += 1;
        }
        let (mut before_end, mut after_end) = (b.len() - tail, a.len() - tail);
        while !(before.is_char_boundary(before_end) && after.is_char_boundary(after_end)) {
            before_end += 1;
            after_end += 1;
        }

        Some(Edit {
            start,
            removed: before[start..before_end].to_string(),
            inserted: after[start..after_end].to_string(),
            before: before_selection,
            after: after_selection,
        })
    }
}

/// How long consecutive typing keeps merging into one undo step.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(900);

/// Maximum number of undo groups retained.
pub const DEFAULT_MAX_GROUPS: usize = 500;

/// An edit with more than this many bytes on either side is a paste or a large
/// cut, not typing, and always gets an undo step to itself.
pub const LARGE_EDIT: usize = 1024;

/// What an edit does, which decides what it is allowed to merge with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Pure insert: nothing removed, something inserted.
    Insert,
    /// Pure delete: something removed, nothing inserted.
    Delete,
    /// Both sides non-empty. Never merges, in either direction.
    Replace,
}

impl Kind {
    fn of(edit: &Edit) -> Kind {
        match (edit.removed.is_empty(), edit.inserted.is_empty()) {
            (true, false) => Kind::Insert,
            (false, true) => Kind::Delete,
            // (false, false) is a replacement; (true, true) is filtered out by
            // `record` before it ever gets here.
            _ => Kind::Replace,
        }
    }
}

/// What an undo or redo did to the buffer.
///
/// The selection to restore, and — when the group's edits all sat in one run of
/// the document, which every merged group does by construction — the single
/// span that changed. The caller uses that span to *patch* its line index,
/// fence map and row heights instead of rebuilding them, which on a nine-megabyte
/// note is the difference between 0.1 ms and 11 ms per ⌘Z.
///
/// `span` is `None` for a compound group (see [`History::record_compound`]),
/// whose edits are deliberately not adjacent; the caller rebuilds for those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    /// Selection to restore: the `before` of the group's first edit for an
    /// undo, the `after` of its last for a redo.
    pub selection: (usize, usize),
    /// `(start, removed, inserted)`: `removed` sat at `start` before the
    /// replay, `inserted` is there now.
    pub span: Option<(usize, String, String)>,
}

/// One undo step: the edits that were merged into it, oldest first.
#[derive(Clone, Debug)]
struct Group {
    /// In the order they were applied. Undo walks this backwards.
    edits: Vec<Edit>,
    kind: Kind,
    /// Lowest byte offset any edit in the group touches. Also the left edge a
    /// backspace run grows from.
    min_start: usize,
    /// One past the last byte the most recent edit inserted — where the next
    /// keystroke of a typing run has to land to be contiguous.
    insert_end: usize,
    /// When the most recent edit in the group was recorded.
    last: Instant,
    /// Whether every edit in the group sits in one run of the document, so the
    /// whole group can be described to the caller as a single replaced span.
    /// True for everything [`History::record`] builds — merging *requires*
    /// contiguity — and false for a compound group.
    contiguous: bool,
}

impl Group {
    fn new(edit: Edit, kind: Kind, now: Instant) -> Group {
        let min_start = edit.start;
        let insert_end = edit.start.saturating_add(edit.inserted.len());
        Group {
            edits: vec![edit],
            kind,
            min_start,
            insert_end,
            last: now,
            contiguous: true,
        }
    }

    /// How many bytes the group's edits, taken together, put into the document
    /// at [`Self::min_start`], and how many they took out.
    ///
    /// Every group is one kind and contiguous, so the run at `min_start` that
    /// held `removed_bytes` before the group holds `inserted_bytes` after it:
    /// a typing run inserted and removed nothing, a backspace run removed and
    /// inserted nothing, and a replacement is a group of exactly one edit.
    fn extent(&self) -> (usize, usize) {
        let removed = self.edits.iter().map(|e| e.removed.len()).sum();
        let inserted = self.edits.iter().map(|e| e.inserted.len()).sum();
        (removed, inserted)
    }

    /// The span an undo of this group rewrites: the `inserted` run becomes the
    /// `removed` one. `text` is the buffer as it stands *before* the undo for
    /// `before`, and after it for `after` — [`read_span`] does the reading.
    fn undo_span(&self) -> Option<(usize, usize, usize)> {
        let (removed, inserted) = self.extent();
        self.contiguous.then_some((self.min_start, inserted, removed))
    }

    /// The mirror, for a redo.
    fn redo_span(&self) -> Option<(usize, usize, usize)> {
        let (removed, inserted) = self.extent();
        self.contiguous.then_some((self.min_start, removed, inserted))
    }

    /// Is `edit` contiguous with what this group has already swallowed?
    ///
    /// Typing extends the group to the right; deleting eats into its left edge
    /// (backspace) or repeatedly out of the same hole (forward delete).
    fn accepts(&self, edit: &Edit) -> bool {
        match self.kind {
            Kind::Insert => edit.start == self.insert_end,
            Kind::Delete => {
                edit.start.saturating_add(edit.removed.len()) == self.min_start
                    || edit.start == self.min_start
            }
            Kind::Replace => false,
        }
    }

    fn push(&mut self, edit: Edit, now: Instant) {
        self.min_start = self.min_start.min(edit.start);
        self.insert_end = edit.start.saturating_add(edit.inserted.len());
        self.last = now;
        self.edits.push(edit);
    }
}

/// Undo / redo stacks over a single text buffer.
///
/// See the [module docs](self) for the storage model and the grouping rules.
#[derive(Clone, Debug)]
pub struct History {
    /// Oldest group at the front, newest at the back. A `VecDeque` so evicting
    /// the oldest group at the limit is O(1).
    undo_stack: VecDeque<Group>,
    /// Newest undone group last. Cleared by any [`History::record`].
    redo_stack: Vec<Group>,
    /// Whether the newest undo group is still accepting merges.
    open: bool,
    max_groups: usize,
}

impl Default for History {
    fn default() -> Self {
        History::new()
    }
}

impl History {
    /// A history retaining [`DEFAULT_MAX_GROUPS`] undo steps.
    pub fn new() -> Self {
        History::with_limit(DEFAULT_MAX_GROUPS)
    }

    /// A history retaining `max_groups` undo steps. Clamped to at least 1, so
    /// `with_limit(0)` still gives you a working single-step undo rather than a
    /// history that silently drops everything.
    pub fn with_limit(max_groups: usize) -> Self {
        History {
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            open: false,
            max_groups: max_groups.max(1),
        }
    }

    /// Record an edit that has ALREADY been applied to the buffer.
    ///
    /// Clears the redo stack. An edit that changes nothing (`removed` and
    /// `inserted` both empty) is ignored entirely — it is a selection move, not
    /// an edit, and it neither opens a group nor drops the redo stack.
    ///
    /// The edit merges into the open group only when **all** of these hold:
    ///
    /// 1. Less than [`COALESCE_WINDOW`] has passed since the previous edit in
    ///    that group.
    /// 2. [`History::break_group`] has not been called since.
    /// 3. It is contiguous with the group's current extent — typing must start
    ///    exactly where the last insert ended; a delete must be eating into the
    ///    group's left edge (backspace) or out of the same hole (forward
    ///    delete).
    /// 4. It is the same kind: inserts merge only with inserts, deletes only
    ///    with deletes. A replacement (both sides non-empty) never merges, and
    ///    closes the group behind it.
    /// 5. Inserting any whitespace — a space, tab or `\n` — joins the group and
    ///    then *closes* it, so each word (and each line) is its own undo step,
    ///    the way macOS steps back a word at a time.
    /// 6. An edit longer than [`LARGE_EDIT`] bytes on either side — a paste, a
    ///    large cut — is always a group of its own.
    ///
    /// Past `max_groups`, the *oldest* group is dropped.
    pub fn record(&mut self, edit: Edit, now: Instant) {
        if edit.removed.is_empty() && edit.inserted.is_empty() {
            return;
        }
        self.redo_stack.clear();

        let kind = Kind::of(&edit);
        // Rule 6.
        let large = edit.inserted.len() > LARGE_EDIT || edit.removed.len() > LARGE_EDIT;
        // Rules 4 and 5: what closes the group behind this edit. An inserted
        // whitespace (a space, tab or newline) joins the word it followed and
        // then closes the group, so undo steps back a word at a time the way
        // macOS does — one ⌘Z takes back "fox", not the whole sentence.
        let closes = large
            || kind == Kind::Replace
            || (kind == Kind::Insert && edit.inserted.chars().any(char::is_whitespace));

        // Rules 1-4 and 6: what lets this edit join the group in front of it.
        let merge = self.open
            && !large
            && kind != Kind::Replace
            && match self.undo_stack.back() {
                Some(group) => {
                    group.kind == kind
                        && now.saturating_duration_since(group.last) < COALESCE_WINDOW
                        && group.accepts(&edit)
                }
                None => false,
            };

        if merge {
            let group = self
                .undo_stack
                .back_mut()
                .expect("`merge` is false when the stack is empty");
            group.push(edit, now);
        } else {
            if self.undo_stack.len() >= self.max_groups {
                self.undo_stack.pop_front();
            }
            self.undo_stack.push_back(Group::new(edit, kind, now));
        }

        self.open = !closes;
    }

    /// Record several already-applied edits as **one** undo step, without
    /// requiring them to be adjacent.
    ///
    /// This is for the commands that rearrange the document rather than type
    /// into it: moving a note is a removal here and an insertion there, and
    /// replacing every match is one edit per match. Recorded through
    /// [`History::record`] those would either be one step per edit (so ⌘Z takes
    /// back a fifth of a move) or, if described as the single run in which the
    /// old and new documents differ, two copies of everything between them —
    /// eighteen megabytes to promote the oldest note in a twenty-year file.
    ///
    /// The edits must be in the order they were applied, each one's offsets
    /// against the buffer as it stood when it was applied; undo walks them
    /// backwards. The group never merges with anything on either side, and it
    /// reports no span, so the caller rebuilds its derived state rather than
    /// patching it.
    pub fn record_compound(&mut self, edits: Vec<Edit>, now: Instant) {
        let edits: Vec<Edit> = edits
            .into_iter()
            .filter(|e| !(e.removed.is_empty() && e.inserted.is_empty()))
            .collect();
        let Some(first) = edits.first() else {
            return;
        };
        self.redo_stack.clear();
        let kind = Kind::of(first);
        let min_start = edits.iter().map(|e| e.start).min().unwrap_or(0);
        if self.undo_stack.len() >= self.max_groups {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(Group {
            edits,
            kind,
            min_start,
            insert_end: min_start,
            last: now,
            contiguous: false,
        });
        // A rearrangement is its own step: nothing merges into it afterwards.
        self.open = false;
    }

    /// Force the next [`History::record`] to open a fresh group.
    ///
    /// Call this whenever the *next* keystroke is not a continuation of the
    /// last one even though it might land next to it: a caret jump, a click or
    /// drag, a selection change, a structural note move, a save/reload, a
    /// window or note switch.
    pub fn break_group(&mut self) {
        self.open = false;
    }

    /// Undo the newest group by applying its inverse to `text`.
    ///
    /// Returns the selection to restore and the span that changed (see
    /// [`Replay`]), or `None` when there is nothing to undo. `text` MUST be the
    /// buffer the history was recorded against; if it is not, the history is
    /// cleared, `text` is left exactly as it was, and this returns `None`.
    pub fn undo(&mut self, text: &mut String) -> Option<Replay> {
        let group = self.undo_stack.pop_back()?;
        self.open = false;

        // What the group put into the document, read before taking it out.
        let extent = group.undo_span();
        let removed = extent.and_then(|(at, len, _)| read_span(text, at, len));

        // Newest edit first: each inverse restores the buffer state the edit
        // before it was recorded against.
        let mut applied = 0usize;
        let mut ok = true;
        for edit in group.edits.iter().rev() {
            if !apply_inverse(text, edit) {
                ok = false;
                break;
            }
            applied += 1;
        }

        if !ok {
            // Put back exactly what we just took out. These spans were written
            // by us a moment ago, so re-applying them forward cannot fail.
            let rolled_back = &group.edits[group.edits.len() - applied..];
            for edit in rolled_back {
                let restored = apply_forward(text, edit);
                debug_assert!(restored, "rollback of our own inverse must apply");
            }
            self.clear();
            return None;
        }

        let selection = group.edits[0].before;
        let span = span_of(text, extent, removed);
        self.redo_stack.push(group);
        Some(Replay { selection, span })
    }

    /// Redo the group most recently undone.
    ///
    /// Returns the selection to restore and the span that changed, or `None`
    /// when there is nothing to redo or the history has desynchronised from
    /// `text` (in which case `text` is untouched and the history is cleared).
    pub fn redo(&mut self, text: &mut String) -> Option<Replay> {
        let group = self.redo_stack.pop()?;
        self.open = false;

        let extent = group.redo_span();
        let removed = extent.and_then(|(at, len, _)| read_span(text, at, len));

        let mut applied = 0usize;
        let mut ok = true;
        for edit in group.edits.iter() {
            if !apply_forward(text, edit) {
                ok = false;
                break;
            }
            applied += 1;
        }

        if !ok {
            for edit in group.edits[..applied].iter().rev() {
                let restored = apply_inverse(text, edit);
                debug_assert!(restored, "rollback of our own redo must apply");
            }
            self.clear();
            return None;
        }

        let selection = group.edits[group.edits.len() - 1].after;
        let span = span_of(text, extent, removed);
        if self.undo_stack.len() >= self.max_groups {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(group);
        Some(Replay { selection, span })
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Number of undo groups currently retained (for tests/diagnostics).
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.open = false;
    }
}

/// `text[at..at + len]`, when that is a real slice on `char` boundaries.
fn read_span(text: &str, at: usize, len: usize) -> Option<String> {
    let end = at.checked_add(len)?;
    text.get(at..end).map(str::to_string)
}

/// Assemble a [`Replay::span`] once the replay has been applied: `removed` was
/// read from the buffer before, `inserted` is read from it now.
///
/// Either read can come back `None` — the extent is arithmetic over the group's
/// own edits, and a caller that handed us a buffer we were not recorded against
/// could make it name bytes that are not there. Reporting no span then is the
/// safe answer: the caller rebuilds, which is what it did before this existed.
fn span_of(
    text: &str,
    extent: Option<(usize, usize, usize)>,
    removed: Option<String>,
) -> Option<(usize, String, String)> {
    let (at, _, new_len) = extent?;
    let inserted = read_span(text, at, new_len)?;
    Some((at, removed?, inserted))
}

/// Undo one edit: put `removed` back where `inserted` is sitting now.
///
/// Returns `false`, having left `text` untouched, if the span is out of
/// bounds, off a `char` boundary, or does not actually hold `inserted` — i.e.
/// if this history was not recorded against this buffer.
fn apply_inverse(text: &mut String, edit: &Edit) -> bool {
    let Some(end) = edit.start.checked_add(edit.inserted.len()) else {
        return false;
    };
    if end > text.len() || !text.is_char_boundary(edit.start) || !text.is_char_boundary(end) {
        return false;
    }
    if &text[edit.start..end] != edit.inserted.as_str() {
        return false;
    }
    text.replace_range(edit.start..end, &edit.removed);
    true
}

/// Re-apply one edit: put `inserted` back over `removed`. Same guarantees as
/// [`apply_inverse`].
fn apply_forward(text: &mut String, edit: &Edit) -> bool {
    let Some(end) = edit.start.checked_add(edit.removed.len()) else {
        return false;
    };
    if end > text.len() || !text.is_char_boundary(edit.start) || !text.is_char_boundary(end) {
        return false;
    }
    if &text[edit.start..end] != edit.removed.as_str() {
        return false;
    }
    text.replace_range(edit.start..end, &edit.inserted);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------- helpers

    /// The selection out of a [`Replay`] — most tests are about where the caret
    /// lands, not about the span the replay reports.
    fn sel(replay: Replay) -> (usize, usize) {
        replay.selection
    }

    /// Apply `edit` to `text` the way the editor would, then hand it to the
    /// history. Panics if the edit does not match the buffer — the tests are
    /// required to build coherent edits.
    fn apply(text: &mut String, history: &mut History, edit: Edit, now: Instant) {
        assert!(
            apply_forward(text, &edit),
            "test built an edit that does not match the buffer"
        );
        history.record(edit, now);
    }

    /// A pure insert of `s` at `at`, with plausible selections around it.
    fn ins(at: usize, s: &str) -> Edit {
        Edit {
            start: at,
            removed: String::new(),
            inserted: s.to_string(),
            before: (at, at),
            after: (at + s.len(), at + s.len()),
        }
    }

    /// A pure delete of `text[at..at + len]`.
    fn del(text: &str, at: usize, len: usize) -> Edit {
        Edit {
            start: at,
            removed: text[at..at + len].to_string(),
            inserted: String::new(),
            before: (at, at + len),
            after: (at, at),
        }
    }

    /// A backspace over the `len` bytes ending at `caret`.
    fn backspace(text: &str, caret: usize, len: usize) -> Edit {
        del(text, caret - len, len)
    }

    /// A replacement of `text[at..at + len]` with `s`.
    fn repl(text: &str, at: usize, len: usize, s: &str) -> Edit {
        Edit {
            start: at,
            removed: text[at..at + len].to_string(),
            inserted: s.to_string(),
            before: (at, at + len),
            after: (at + s.len(), at + s.len()),
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    fn ms(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    /// Type `s` one `char` at a time at the caret, 10 ms apart.
    fn type_str(text: &mut String, history: &mut History, base: Instant, at: usize, s: &str) {
        let mut caret = at;
        for (i, ch) in s.chars().enumerate() {
            let mut buf = [0u8; 4];
            let piece = ch.encode_utf8(&mut buf).to_string();
            apply(text, history, ins(caret, &piece), ms(base, 10 * i as u64));
            caret += piece.len();
        }
    }

    // ------------------------------------------------------------ basic shape

    #[test]
    fn empty_history_does_nothing() {
        let mut history = History::new();
        let mut text = String::from("hello");

        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo_depth(), 0);
        assert_eq!(history.redo_depth(), 0);
        assert_eq!(history.undo(&mut text), None);
        assert_eq!(history.redo(&mut text), None);
        assert_eq!(text, "hello");
    }

    #[test]
    fn undo_then_redo_restores_exact_bytes() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("hello world");

        apply(&mut text, &mut history, ins(5, ","), base);
        assert_eq!(text, "hello, world");

        assert_eq!(history.undo(&mut text).map(sel), Some((5, 5)));
        assert_eq!(text, "hello world");
        assert!(!history.can_undo());
        assert!(history.can_redo());

        assert_eq!(history.redo(&mut text).map(sel), Some((6, 6)));
        assert_eq!(text, "hello, world");
        assert!(history.can_undo());
        assert!(!history.can_redo());
    }

    #[test]
    fn no_op_edit_is_ignored() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("abc");

        apply(&mut text, &mut history, ins(0, "x"), base);
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(history.redo_depth(), 1);

        history.record(
            Edit {
                start: 1,
                removed: String::new(),
                inserted: String::new(),
                before: (1, 1),
                after: (1, 1),
            },
            ms(base, 10),
        );

        assert_eq!(history.undo_depth(), 0);
        assert_eq!(history.redo_depth(), 1, "a no-op must not drop the redo stack");
    }

    #[test]
    fn clear_drops_both_stacks() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("abc");

        apply(&mut text, &mut history, ins(3, "d"), base);
        apply(&mut text, &mut history, ins(4, "e"), ms(base, 5000));
        history.undo(&mut text);
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 1);

        history.clear();
        assert_eq!(history.undo_depth(), 0);
        assert_eq!(history.redo_depth(), 0);
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }

    // -------------------------------------------------------- coalescing rules

    #[test]
    fn rule1_edits_outside_the_window_start_a_new_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        apply(&mut text, &mut history, ins(0, "a"), base);
        // Just inside the window: merges.
        apply(&mut text, &mut history, ins(1, "b"), ms(base, 899));
        assert_eq!(history.undo_depth(), 1);

        // Exactly at the window: does not merge ("less than" is strict).
        apply(&mut text, &mut history, ins(2, "c"), ms(base, 899 + 900));
        assert_eq!(history.undo_depth(), 2);

        // Well outside: does not merge.
        apply(&mut text, &mut history, ins(3, "d"), ms(base, 10_000));
        assert_eq!(history.undo_depth(), 3);
        assert_eq!(text, "abcd");

        assert_eq!(history.undo(&mut text).map(sel), Some((3, 3)));
        assert_eq!(text, "abc");
        assert_eq!(history.undo(&mut text).map(sel), Some((2, 2)));
        assert_eq!(text, "ab");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");
    }

    #[test]
    fn rule2_break_group_opens_a_new_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        type_str(&mut text, &mut history, base, 0, "hello");
        assert_eq!(history.undo_depth(), 1);

        history.break_group();

        // Contiguous and immediate, but the group was broken.
        apply(&mut text, &mut history, ins(5, "!"), ms(base, 60));
        assert_eq!(history.undo_depth(), 2);
        assert_eq!(text, "hello!");

        assert_eq!(history.undo(&mut text).map(sel), Some((5, 5)));
        assert_eq!(text, "hello");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");
    }

    #[test]
    fn rule3_contiguous_typing_coalesces() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        // Within a word, contiguous keystrokes coalesce into one step. (Word
        // boundaries close the group — see `rule5_whitespace_closes_the_group`.)
        type_str(&mut text, &mut history, base, 0, "hello");
        assert_eq!(text, "hello");
        assert_eq!(history.undo_depth(), 1, "one word is one typing run");

        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");
        assert_eq!(history.redo(&mut text).map(sel), Some((5, 5)));
        assert_eq!(text, "hello");
    }

    #[test]
    fn rule3_non_contiguous_insert_starts_a_new_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("......");

        apply(&mut text, &mut history, ins(0, "a"), base);
        // Caret jumped: not where the last insert ended.
        apply(&mut text, &mut history, ins(4, "b"), ms(base, 10));
        assert_eq!(history.undo_depth(), 2);
        assert_eq!(text, "a...b...");

        assert_eq!(history.undo(&mut text).map(sel), Some((4, 4)));
        assert_eq!(text, "a......");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "......");
    }

    #[test]
    fn rule3_backspace_run_coalesces() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("hello world");

        // Backspace from the end, three times.
        for i in 0..3u64 {
            let caret = text.len();
            let edit = backspace(&text, caret, 1);
            apply(&mut text, &mut history, edit, ms(base, 10 * i));
        }
        assert_eq!(text, "hello wo");
        assert_eq!(history.undo_depth(), 1);

        assert_eq!(history.undo(&mut text).map(sel), Some((10, 11)));
        assert_eq!(text, "hello world");
        assert_eq!(history.redo(&mut text).map(sel), Some((8, 8)));
        assert_eq!(text, "hello wo");
    }

    #[test]
    fn rule3_forward_delete_run_coalesces() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("hello world");

        // The Delete key: the caret stays put and the hole stays put.
        for i in 0..3u64 {
            let edit = del(&text, 5, 1);
            apply(&mut text, &mut history, edit, ms(base, 10 * i));
        }
        assert_eq!(text, "hellorld");
        assert_eq!(history.undo_depth(), 1);

        assert_eq!(history.undo(&mut text).map(sel), Some((5, 6)));
        assert_eq!(text, "hello world");
    }

    #[test]
    fn rule3_non_contiguous_delete_starts_a_new_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("abcdefgh");

        let e1 = del(&text, 7, 1);
        apply(&mut text, &mut history, e1, base);
        // A delete somewhere else entirely.
        let e2 = del(&text, 2, 1);
        apply(&mut text, &mut history, e2, ms(base, 10));

        assert_eq!(text, "abdefg");
        assert_eq!(history.undo_depth(), 2);

        assert_eq!(history.undo(&mut text).map(sel), Some((2, 3)));
        assert_eq!(text, "abcdefg");
        assert_eq!(history.undo(&mut text).map(sel), Some((7, 8)));
        assert_eq!(text, "abcdefgh");
    }

    #[test]
    fn rule4_inserts_and_deletes_never_mix() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        apply(&mut text, &mut history, ins(0, "ab"), base);
        // Backspace right where typing stopped: adjacent, immediate, wrong kind.
        let edit = backspace(&text, 2, 1);
        apply(&mut text, &mut history, edit, ms(base, 10));
        assert_eq!(history.undo_depth(), 2);
        // And typing again after the delete is a third group.
        apply(&mut text, &mut history, ins(1, "c"), ms(base, 20));
        assert_eq!(history.undo_depth(), 3);
        assert_eq!(text, "ac");

        assert_eq!(history.undo(&mut text).map(sel), Some((1, 1)));
        assert_eq!(text, "a");
        assert_eq!(history.undo(&mut text).map(sel), Some((1, 2)));
        assert_eq!(text, "ab");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");
    }

    #[test]
    fn rule4_replacement_never_merges_and_closes_the_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("one two three");

        // Typing first, so there is an open group to refuse.
        apply(&mut text, &mut history, ins(13, "!"), base);
        assert_eq!(history.undo_depth(), 1);

        let edit = repl(&text, 4, 3, "TWO");
        apply(&mut text, &mut history, edit, ms(base, 10));
        assert_eq!(text, "one TWO three!");
        assert_eq!(history.undo_depth(), 2, "a replacement never merges in");

        // And nothing merges into it afterwards, even contiguous typing.
        apply(&mut text, &mut history, ins(7, "x"), ms(base, 20));
        assert_eq!(history.undo_depth(), 3, "a replacement closes the group");

        assert_eq!(history.undo(&mut text).map(sel), Some((7, 7)));
        assert_eq!(text, "one TWO three!");
        assert_eq!(history.undo(&mut text).map(sel), Some((4, 7)));
        assert_eq!(text, "one two three!");
        assert_eq!(history.undo(&mut text).map(sel), Some((13, 13)));
        assert_eq!(text, "one two three");
    }

    #[test]
    fn rule5_newline_joins_the_group_then_closes_it() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        type_str(&mut text, &mut history, base, 0, "one\ntwo\nthree");
        assert_eq!(text, "one\ntwo\nthree");
        assert_eq!(history.undo_depth(), 3, "one undo step per line");

        assert_eq!(history.undo(&mut text).map(sel), Some((8, 8)));
        assert_eq!(text, "one\ntwo\n", "the newline stays with the line it ended");
        assert_eq!(history.undo(&mut text).map(sel), Some((4, 4)));
        assert_eq!(text, "one\n");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");

        assert_eq!(history.redo(&mut text).map(sel), Some((4, 4)));
        assert_eq!(text, "one\n");
        assert_eq!(history.redo(&mut text).map(sel), Some((8, 8)));
        assert_eq!(text, "one\ntwo\n");
        assert_eq!(history.redo(&mut text).map(sel), Some((13, 13)));
        assert_eq!(text, "one\ntwo\nthree");
    }

    #[test]
    fn rule5_whitespace_closes_the_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        // Each word plus its trailing whitespace is its own undo step, so ⌘Z
        // steps back a word at a time the way macOS does.
        type_str(&mut text, &mut history, base, 0, "the quick brown fox");
        assert_eq!(text, "the quick brown fox");
        assert_eq!(history.undo_depth(), 4, "one step per word");

        // The first undo takes back only the last, still-open word.
        assert_eq!(history.undo(&mut text).map(sel), Some((16, 16)));
        assert_eq!(text, "the quick brown ");
        assert_eq!(history.undo(&mut text).map(sel), Some((10, 10)));
        assert_eq!(text, "the quick ");
        assert_eq!(history.undo(&mut text).map(sel), Some((4, 4)));
        assert_eq!(text, "the ");
        assert_eq!(history.undo(&mut text).map(sel), Some((0, 0)));
        assert_eq!(text, "");
    }

    #[test]
    fn rule6_large_edits_are_their_own_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        apply(&mut text, &mut history, ins(0, "a"), base);
        assert_eq!(history.undo_depth(), 1);

        // Exactly at the limit still counts as typing-ish and may merge.
        let at_limit = "x".repeat(LARGE_EDIT);
        apply(&mut text, &mut history, ins(1, &at_limit), ms(base, 10));
        assert_eq!(history.undo_depth(), 1);

        // One byte over is a paste: its own group.
        let paste = "y".repeat(LARGE_EDIT + 1);
        let at = text.len();
        apply(&mut text, &mut history, ins(at, &paste), ms(base, 20));
        assert_eq!(history.undo_depth(), 2);

        // And nothing merges into a paste either.
        let at = text.len();
        apply(&mut text, &mut history, ins(at, "z"), ms(base, 30));
        assert_eq!(history.undo_depth(), 3);

        // A large cut, likewise.
        let cut = del(&text, 1, LARGE_EDIT + 1);
        apply(&mut text, &mut history, cut, ms(base, 40));
        assert_eq!(history.undo_depth(), 4);

        let expected_len = 1 + LARGE_EDIT + (LARGE_EDIT + 1) + 1 - (LARGE_EDIT + 1);
        assert_eq!(text.len(), expected_len);

        while history.can_undo() {
            assert!(history.undo(&mut text).is_some());
        }
        assert_eq!(text, "");
        while history.can_redo() {
            assert!(history.redo(&mut text).is_some());
        }
        assert_eq!(text.len(), expected_len);
    }

    // ---------------------------------------------------------- stack hygiene

    /// What the trimming actually buys: a change in the middle records the
    /// middle, however large the document around it.
    #[test]
    fn a_rewrite_records_only_the_span_that_differs() {
        let before = format!("{}CHANGED{}", "a".repeat(5_000), "b".repeat(5_000));
        let after = format!("{}different{}", "a".repeat(5_000), "b".repeat(5_000));
        let edit = Edit::between(&before, &after, (0, 0), (0, 0)).unwrap();
        assert_eq!(edit.removed, "CHANGED");
        assert_eq!(edit.inserted, "different");
        assert_eq!(edit.start, 5_000);
    }

    /// Moving a note is correct, but not small: promoting one changes byte 0,
    /// so there is no shared prefix to trim and the recorded span reaches back
    /// to the start. Asserted here so the cost is visible rather than assumed
    /// away — see BACKLOG.
    #[test]
    fn a_note_move_round_trips_even_though_it_records_a_lot() {
        let before = "first note\n---\nsecond note";
        let after = "second note\n---\nfirst note";
        let edit = Edit::between(before, after, (0, 0), (0, 0)).unwrap();
        assert!(
            edit.removed.len() > before.len() / 2,
            "a promotion shares no prefix, so it records most of what is above it"
        );

        let mut text = before.to_string();
        assert!(apply_forward(&mut text, &edit));
        assert_eq!(text, after);
        assert!(apply_inverse(&mut text, &edit));
        assert_eq!(text, before);
    }

    #[test]
    fn a_rewrite_that_changed_nothing_records_nothing() {
        assert_eq!(Edit::between("same", "same", (0, 0), (0, 0)), None);
    }

    #[test]
    fn a_rewrite_round_trips_for_every_shape_of_change() {
        let cases = [
            ("", "new"),
            ("old", ""),
            ("abc", "abcd"),
            ("abcd", "abc"),
            ("\n---\nnote", "note\n---\n"),
            ("a\n---\nb\n---\nc", "c\n---\na\n---\nb"),
            ("αβγ", "γβα"),
            ("🎉 party", "party 🎉"),
            ("x", "y"),
        ];
        for (before, after) in cases {
            let Some(edit) = Edit::between(before, after, (0, 0), (1, 1)) else {
                assert_eq!(before, after, "no edit for a real change");
                continue;
            };
            let mut text = before.to_string();
            assert!(apply_forward(&mut text, &edit), "forward {before:?}");
            assert_eq!(text, after, "forward {before:?} -> {after:?}");
            assert!(apply_inverse(&mut text, &edit), "inverse {after:?}");
            assert_eq!(text, before, "inverse {after:?} -> {before:?}");
        }
    }

    /// The trimmed span must never split a character, or the recorded strings
    /// are not valid slices of either version.
    #[test]
    fn the_recorded_span_never_splits_a_character() {
        let before = "aαb🎉c";
        for cut in 0..before.len() {
            if !before.is_char_boundary(cut) {
                continue;
            }
            let after = format!("{}Z{}", &before[..cut], &before[cut..]);
            let edit = Edit::between(before, &after, (0, 0), (0, 0)).unwrap();
            let mut text = before.to_string();
            assert!(apply_forward(&mut text, &edit), "cut {cut}");
            assert_eq!(text, after);
        }
    }

    #[test]
    fn recording_after_an_undo_clears_the_redo_stack() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        type_str(&mut text, &mut history, base, 0, "abc");
        history.undo(&mut text);
        assert_eq!(text, "");
        assert_eq!(history.redo_depth(), 1);

        apply(&mut text, &mut history, ins(0, "z"), ms(base, 100));
        assert_eq!(history.redo_depth(), 0);
        assert!(!history.can_redo());
        assert_eq!(history.redo(&mut text), None);
        assert_eq!(text, "z");
    }

    #[test]
    fn an_undone_group_is_closed_to_further_merging() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        apply(&mut text, &mut history, ins(0, "a"), base);
        apply(&mut text, &mut history, ins(1, "b"), ms(base, 10));
        assert_eq!(history.undo_depth(), 1);

        history.undo(&mut text);
        assert_eq!(text, "");
        history.redo(&mut text);
        assert_eq!(text, "ab");

        // Contiguous and immediate, but the redone group must not reopen.
        apply(&mut text, &mut history, ins(2, "c"), ms(base, 20));
        assert_eq!(history.undo_depth(), 2);
        assert_eq!(text, "abc");
        assert_eq!(history.undo(&mut text).map(sel), Some((2, 2)));
        assert_eq!(text, "ab");
    }

    #[test]
    fn max_groups_evicts_the_oldest_and_saturates() {
        let base = t0();
        let mut history = History::with_limit(4);
        let mut text = String::new();

        // Ten separate groups, each a distinct character appended.
        for i in 0..10u64 {
            let at = text.len();
            let ch = (b'0' + i as u8) as char;
            apply(
                &mut text,
                &mut history,
                ins(at, &ch.to_string()),
                ms(base, 10_000 * i),
            );
            assert_eq!(history.undo_depth(), (i as usize + 1).min(4));
        }
        assert_eq!(text, "0123456789");

        // Only the four newest survive: undo can walk back to "012345" and no
        // further.
        for _ in 0..4 {
            assert!(history.undo(&mut text).is_some());
        }
        assert_eq!(text, "012345");
        assert!(!history.can_undo());
        assert_eq!(history.undo(&mut text), None);
        assert_eq!(text, "012345");

        // The retained groups still redo cleanly.
        for _ in 0..4 {
            assert!(history.redo(&mut text).is_some());
        }
        assert_eq!(text, "0123456789");
    }

    #[test]
    fn with_limit_zero_still_keeps_one_group() {
        let base = t0();
        let mut history = History::with_limit(0);
        let mut text = String::new();

        apply(&mut text, &mut history, ins(0, "a"), base);
        apply(&mut text, &mut history, ins(1, "b"), ms(base, 10_000));
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.undo(&mut text).map(sel), Some((1, 1)));
        assert_eq!(text, "a");
    }

    // --------------------------------------------------------------- offsets

    #[test]
    fn selection_restores_through_a_coalesced_group() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("xyz");

        // A coalesced run of three inserts starting from a selection.
        apply(
            &mut text,
            &mut history,
            Edit {
                start: 1,
                removed: String::new(),
                inserted: "a".into(),
                before: (1, 3),
                after: (2, 2),
            },
            base,
        );
        apply(
            &mut text,
            &mut history,
            Edit {
                start: 2,
                removed: String::new(),
                inserted: "b".into(),
                before: (2, 2),
                after: (3, 3),
            },
            ms(base, 10),
        );
        apply(
            &mut text,
            &mut history,
            Edit {
                start: 3,
                removed: String::new(),
                inserted: "c".into(),
                before: (3, 3),
                after: (4, 4),
            },
            ms(base, 20),
        );
        assert_eq!(text, "xabcyz");
        assert_eq!(history.undo_depth(), 1);

        assert_eq!(
            history.undo(&mut text).map(sel),
            Some((1, 3)),
            "undo restores the `before` of the group's FIRST edit"
        );
        assert_eq!(text, "xyz");
        assert_eq!(
            history.redo(&mut text).map(sel),
            Some((4, 4)),
            "redo restores the `after` of the group's LAST edit"
        );
        assert_eq!(text, "xabcyz");
    }

    // ---------------------------------------------------------------- unicode

    #[test]
    fn multibyte_round_trips_exactly() {
        let base = t0();
        let mut history = History::new();
        // 2-byte, 3-byte, 4-byte, and a combining mark.
        let original = String::from("héllo 中文 😀 e\u{0301}nd");
        let mut text = original.clone();

        let mut now = base;
        let bump = |n: &mut Instant| {
            *n += Duration::from_millis(5_000);
            *n
        };

        // Insert a 4-byte char in the middle of the CJK run.
        let at = text.find('中').unwrap() + '中'.len_utf8();
        apply(&mut text, &mut history, ins(at, "🎈"), bump(&mut now));

        // Delete the emoji that was already there.
        let at = text.find('😀').unwrap();
        let edit = del(&text, at, '😀'.len_utf8());
        apply(&mut text, &mut history, edit, bump(&mut now));

        // Replace a 2-byte char with a 3-byte one.
        let at = text.find('é').unwrap();
        let edit = repl(&text, at, 'é'.len_utf8(), "한");
        apply(&mut text, &mut history, edit, bump(&mut now));

        // Delete a bare combining mark.
        let at = text.find('\u{0301}').unwrap();
        let edit = del(&text, at, '\u{0301}'.len_utf8());
        apply(&mut text, &mut history, edit, bump(&mut now));

        let final_text = text.clone();
        assert_ne!(final_text, original);
        assert_eq!(history.undo_depth(), 4);

        while history.can_undo() {
            assert!(history.undo(&mut text).is_some());
        }
        assert_eq!(text, original, "undo must restore the exact bytes");

        while history.can_redo() {
            assert!(history.redo(&mut text).is_some());
        }
        assert_eq!(text, final_text, "redo must restore the exact bytes");
    }

    // ------------------------------------------------------------ desync guard

    #[test]
    fn desynchronised_history_refuses_to_undo() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("hello");

        apply(&mut text, &mut history, ins(5, " world"), base);
        assert_eq!(text, "hello world");

        // Somebody else replaced the buffer behind our back.
        text = String::from("a completely different note");
        let untouched = text.clone();

        assert_eq!(history.undo(&mut text), None);
        assert_eq!(text, untouched, "the buffer must be left alone");
        assert_eq!(history.undo_depth(), 0, "a desync clears the history");
        assert_eq!(history.redo_depth(), 0);
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }

    #[test]
    fn desync_partway_through_a_group_rolls_back() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::new();

        type_str(&mut text, &mut history, base, 0, "abc");
        assert_eq!(history.undo_depth(), 1);

        // Corrupt the byte the group's FIRST edit wrote, leaving the later two
        // matching — undo gets two inverses in before it notices.
        text.replace_range(0..1, "X");
        let untouched = text.clone();

        assert_eq!(history.undo(&mut text), None);
        assert_eq!(text, untouched, "a partly applied group must roll back");
        assert_eq!(history.undo_depth(), 0);
    }

    #[test]
    fn desync_partway_through_a_redo_rolls_back() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("abc");

        // A coalesced backspace run, so the redo has bytes to check.
        for i in 0..3u64 {
            let caret = text.len();
            let edit = backspace(&text, caret, 1);
            apply(&mut text, &mut history, edit, ms(base, 10 * i));
        }
        assert_eq!(text, "");
        assert_eq!(history.undo_depth(), 1);

        history.undo(&mut text);
        assert_eq!(text, "abc");

        // The redo deletes "c", then "b", then "a". Corrupt the middle one:
        // it gets one delete in before it notices.
        text.replace_range(1..2, "X");
        let untouched = text.clone();

        assert_eq!(history.redo(&mut text), None);
        assert_eq!(text, untouched);
        assert_eq!(history.redo_depth(), 0);
        assert_eq!(history.undo_depth(), 0);
    }

    #[test]
    fn desync_onto_a_char_boundary_does_not_panic() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("ab");

        apply(&mut text, &mut history, ins(1, "x"), base);
        assert_eq!(text, "axb");

        // Same byte length, but offset 1 is now mid-`char`.
        text = String::from("😀");
        let untouched = text.clone();

        assert_eq!(history.undo(&mut text), None);
        assert_eq!(text, untouched);
    }

    #[test]
    fn truncated_buffer_does_not_panic() {
        let base = t0();
        let mut history = History::new();
        let mut text = String::from("hello world");

        apply(&mut text, &mut history, ins(11, " and more"), base);
        text.truncate(3);
        let untouched = text.clone();

        assert_eq!(history.undo(&mut text), None);
        assert_eq!(text, untouched);
    }

    // ------------------------------------------------------- randomized tests

    /// A deterministic LCG (the constants are Knuth's MMIX). Seeded per test so
    /// a failure is reproducible.
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Lcg {
            Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
        }

        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // The high bits of an LCG are the good ones.
            (self.0 >> 33) as u32
        }

        /// Uniform-ish in `0..n`. Returns 0 for `n == 0`.
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                self.next_u32() as usize % n
            }
        }
    }

    /// ASCII, 2-byte, 3-byte and 4-byte chars, a combining mark, and `\n`.
    const ALPHABET: &[char] = &[
        'a', 'b', 'Z', ' ', '\t', '\n', '\n', 'é', 'ß', '中', '한', '😀', '𝄞', '\u{0301}',
    ];

    /// The largest byte offset `<= i` that is a `char` boundary.
    fn floor_boundary(s: &str, i: usize) -> usize {
        let mut i = i.min(s.len());
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn random_run(rng: &mut Lcg, len: usize) -> String {
        (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len())])
            .collect()
    }

    /// Build an edit that is valid against `text` right now.
    fn random_edit(rng: &mut Lcg, text: &str) -> Edit {
        let start = floor_boundary(text, rng.below(text.len() + 1));

        // 1 in 40 is a paste big enough to trip rule 6.
        let insert_len = if rng.below(40) == 0 {
            300 + rng.below(200)
        } else {
            rng.below(4)
        };

        let remove_len = if rng.below(40) == 0 {
            rng.below(600)
        } else {
            rng.below(4)
        };
        let end = floor_boundary(text, start.saturating_add(remove_len));

        let (removed, mut inserted) = (text[start..end].to_string(), random_run(rng, insert_len));
        // Never hand `record` a no-op; it is documented to drop those, and the
        // reference model here does not track them.
        if removed.is_empty() && inserted.is_empty() {
            inserted.push('a');
        }

        let after_caret = start + inserted.len();
        Edit {
            start,
            removed,
            inserted,
            before: (start, end),
            after: (after_caret, after_caret),
        }
    }

    /// Assert that `span` describes exactly how `before` became `after`.
    ///
    /// This is the whole contract the caller patches its line index against, so
    /// it is checked the same way the index's own differential tests are: the
    /// prefix and the suffix must be untouched, and the two middles must be the
    /// strings the span claims.
    fn assert_span_describes(before: &str, after: &str, span: &(usize, String, String)) {
        let (at, removed, inserted) = span;
        let (at, removed, inserted) = (*at, removed.as_str(), inserted.as_str());
        assert!(at <= before.len() && at <= after.len(), "span starts inside both");
        assert_eq!(
            before.get(at..at + removed.len()),
            Some(removed),
            "the removed bytes were where the span says they were"
        );
        assert_eq!(
            after.get(at..at + inserted.len()),
            Some(inserted),
            "the inserted bytes are where the span says they are"
        );
        assert_eq!(before[..at], after[..at], "the prefix did not move");
        assert_eq!(
            before[at + removed.len()..],
            after[at + inserted.len()..],
            "the suffix did not move"
        );
    }

    /// Every span an undo or a redo reports really is the change it made.
    ///
    /// The app patches its line index, fence map and row heights from this
    /// span instead of rebuilding them, and the failure mode of getting it
    /// wrong is silent corruption — so it gets the same differential treatment
    /// the other incremental caches have: thousands of random edits, each
    /// replay checked against a before/after pair.
    #[test]
    fn property_replay_spans_describe_the_change() {
        const STEPS: usize = 2_000;

        let base = t0();
        let mut rng = Lcg::new(0xC0FF_EE01);
        let mut history = History::with_limit(usize::MAX);
        let mut text = String::from("seed — 中文 😀\nsecond line\nthird\n");

        let mut now = base;
        for _ in 0..STEPS {
            let mut edit = random_edit(&mut rng, &text);
            if text.len() > 2_000 && edit.inserted.len() > edit.removed.len() {
                edit.inserted.clear();
                if edit.removed.is_empty() {
                    edit.inserted.push('x');
                }
            }
            assert!(apply_forward(&mut text, &edit), "test edit must apply");
            now += Duration::from_millis(rng.below(1_400) as u64);
            if rng.below(25) == 0 {
                history.break_group();
            }
            history.record(edit, now);
        }

        // Walk all the way back, then all the way forward, checking every step.
        let mut undone = 0usize;
        loop {
            let before = text.clone();
            let Some(replay) = history.undo(&mut text) else {
                break;
            };
            undone += 1;
            let span = replay.span.expect("a recorded group is contiguous");
            assert_span_describes(&before, &text, &span);
        }
        assert!(undone > 100, "the walk has to be long enough to mean something");

        for _ in 0..undone {
            let before = text.clone();
            let replay = history.redo(&mut text).expect("redo must succeed");
            let span = replay.span.expect("a recorded group is contiguous");
            assert_span_describes(&before, &text, &span);
        }
    }

    /// A compound group is one undo step made of edits that are not adjacent —
    /// a note moving from the middle of the document to the top. It reports no
    /// span (the caller rebuilds), and it round-trips exactly.
    #[test]
    fn compound_group_is_one_step_and_reports_no_span() {
        let mut text = String::from("first\n---\nsecond\n---\nthird");
        let original = text.clone();
        let mut history = History::new();

        // Move "third" to the top: cut it with its rule, paste it at 0.
        let cut = Edit {
            start: 16,
            removed: "\n---\nthird".to_string(),
            inserted: String::new(),
            before: (21, 21),
            after: (16, 16),
        };
        assert!(apply_forward(&mut text, &cut));
        let paste = Edit {
            start: 0,
            removed: String::new(),
            inserted: "third\n---\n".to_string(),
            before: (16, 16),
            after: (0, 0),
        };
        assert!(apply_forward(&mut text, &paste));
        assert_eq!(text, "third\n---\nfirst\n---\nsecond");
        history.record_compound(vec![cut, paste], t0());
        assert_eq!(history.undo_depth(), 1, "one move is one undo step");

        let replay = history.undo(&mut text).expect("undo must succeed");
        assert_eq!(text, original, "the move undoes exactly");
        assert_eq!(replay.selection, (21, 21));
        assert!(
            replay.span.is_none(),
            "a compound group's edits are not one run; the caller must rebuild"
        );

        let replay = history.redo(&mut text).expect("redo must succeed");
        assert_eq!(text, "third\n---\nfirst\n---\nsecond");
        assert_eq!(replay.selection, (0, 0));
        assert!(replay.span.is_none());
    }

    /// Nothing merges into or out of a compound group.
    #[test]
    fn compound_group_never_merges() {
        let mut text = String::from("abc");
        let mut history = History::new();
        let now = t0();

        apply(&mut text, &mut history, ins(3, "d"), now);
        let e = ins(4, "e");
        assert!(apply_forward(&mut text, &e));
        history.record_compound(vec![e], now);
        let f = ins(5, "f");
        assert!(apply_forward(&mut text, &f));
        history.record(f, now);

        assert_eq!(
            history.undo_depth(),
            3,
            "the compound group neither swallowed the typing before it nor the typing after"
        );
    }

    #[test]
    fn property_random_edits_round_trip() {
        const STEPS: usize = 5_000;

        let base = t0();
        let mut rng = Lcg::new(0x5EED_1234);
        // No eviction: this test checks the full history, not the limit.
        let mut history = History::with_limit(usize::MAX);

        let original = String::from("seed text — 中文 😀\nsecond line\n");
        let mut text = original.clone();

        // Snapshots are fine *in the test*: `snapshots[i]` is the text before
        // edit `i`. The implementation is what must not keep them.
        let mut snapshots: Vec<String> = Vec::with_capacity(STEPS + 1);
        // The edit index each undo group starts at, and each group's last
        // edit's `after` selection.
        let mut group_first: Vec<usize> = Vec::new();
        let mut group_last_after: Vec<(usize, usize)> = Vec::new();
        let mut befores: Vec<(usize, usize)> = Vec::new();

        let mut now = base;
        for i in 0..STEPS {
            // Bias towards deletes when the buffer has grown, so it stays a
            // sane size across 5k edits.
            let mut edit = random_edit(&mut rng, &text);
            if text.len() > 4_000 && edit.inserted.len() > edit.removed.len() {
                edit.inserted.clear();
                if edit.removed.is_empty() {
                    let end = floor_boundary(&text, (edit.start + 3).min(text.len()));
                    if end > edit.start {
                        edit.removed = text[edit.start..end].to_string();
                        edit.before = (edit.start, end);
                    } else {
                        edit.inserted.push('x');
                    }
                }
                edit.after = (edit.start, edit.start);
            }

            snapshots.push(text.clone());
            befores.push(edit.before);
            let after = edit.after;

            assert!(apply_forward(&mut text, &edit), "test edit must apply");

            // Random time steps: sometimes inside the window, sometimes not.
            now += Duration::from_millis(rng.below(1_400) as u64);
            if rng.below(25) == 0 {
                history.break_group();
            }

            let depth_before = history.undo_depth();
            history.record(edit, now);
            assert_eq!(
                history.undo_depth(),
                depth_before + usize::from(history.undo_depth() != depth_before),
                "a record either merges or adds exactly one group"
            );
            if history.undo_depth() > depth_before {
                group_first.push(i);
                group_last_after.push(after);
            } else {
                *group_last_after
                    .last_mut()
                    .expect("a merge implies an existing group") = after;
            }
        }
        let final_text = text.clone();
        assert_eq!(history.undo_depth(), group_first.len());

        // Undo everything, checking every intermediate state against the
        // snapshot taken before that group's first edit.
        for g in (0..group_first.len()).rev() {
            let first = group_first[g];
            let selection = history.undo(&mut text).expect("undo must succeed").selection;
            assert_eq!(selection, befores[first], "group {g} selection");
            assert_eq!(text, snapshots[first], "group {g} undo");
        }
        assert!(!history.can_undo());
        assert_eq!(text, original, "undoing everything returns the original");

        // Redo everything back.
        for (g, expected_selection) in group_last_after.iter().enumerate() {
            let selection = history.redo(&mut text).expect("redo must succeed").selection;
            assert_eq!(selection, *expected_selection, "group {g} redo selection");
            let expected = group_first
                .get(g + 1)
                .map(|&next| snapshots[next].as_str())
                .unwrap_or(final_text.as_str());
            assert_eq!(text, expected, "group {g} redo");
        }
        assert!(!history.can_redo());
        assert_eq!(text, final_text, "redoing everything returns the final text");
    }

    /// The reference model for the interleaved test: what one undo group looks
    /// like from the outside.
    struct GroupModel {
        before_text: String,
        before_sel: (usize, usize),
        after_sel: (usize, usize),
    }

    #[test]
    fn property_interleaved_undo_redo_matches_a_reference_model() {
        const STEPS: usize = 5_000;

        for seed in [1u64, 7, 99] {
            let base = t0();
            let mut rng = Lcg::new(seed);
            let mut history = History::with_limit(usize::MAX);

            let original = String::from("interleaved 😀 中\nstart\n");
            let mut text = original.clone();

            let mut undo_model: Vec<GroupModel> = Vec::new();
            let mut redo_model: Vec<(GroupModel, String)> = Vec::new();

            let mut now = base;
            for _ in 0..STEPS {
                assert_eq!(history.undo_depth(), undo_model.len());
                assert_eq!(history.redo_depth(), redo_model.len());

                match rng.below(10) {
                    0..=6 => {
                        let edit = random_edit(&mut rng, &text);
                        let before_text = text.clone();
                        let before_sel = edit.before;
                        let after_sel = edit.after;
                        assert!(apply_forward(&mut text, &edit), "test edit must apply");

                        now += Duration::from_millis(rng.below(1_400) as u64);
                        if rng.below(25) == 0 {
                            history.break_group();
                        }

                        let depth_before = history.undo_depth();
                        history.record(edit, now);
                        if history.undo_depth() > depth_before {
                            undo_model.push(GroupModel {
                                before_text,
                                before_sel,
                                after_sel,
                            });
                        } else {
                            undo_model
                                .last_mut()
                                .expect("a merge implies an existing group")
                                .after_sel = after_sel;
                        }
                        redo_model.clear();
                    }
                    7 | 8 => match undo_model.pop() {
                        Some(group) => {
                            let after_text = text.clone();
                            let selection = history.undo(&mut text).expect("undo must succeed").selection;
                            assert_eq!(selection, group.before_sel);
                            assert_eq!(text, group.before_text);
                            redo_model.push((group, after_text));
                        }
                        None => {
                            let untouched = text.clone();
                            assert_eq!(history.undo(&mut text), None);
                            assert_eq!(text, untouched);
                        }
                    },
                    _ => match redo_model.pop() {
                        Some((group, after_text)) => {
                            let selection = history.redo(&mut text).expect("redo must succeed").selection;
                            assert_eq!(selection, group.after_sel);
                            assert_eq!(text, after_text);
                            undo_model.push(group);
                        }
                        None => {
                            let untouched = text.clone();
                            assert_eq!(history.redo(&mut text), None);
                            assert_eq!(text, untouched);
                        }
                    },
                }
            }

            // Unwind to the beginning of time, then wind all the way forward.
            let final_text = text.clone();
            let mut forward: Vec<String> = Vec::new();
            while history.can_undo() {
                forward.push(text.clone());
                assert!(history.undo(&mut text).is_some());
            }
            assert_eq!(text, original, "seed {seed}: full unwind");

            for expected in forward.iter().rev() {
                assert!(history.redo(&mut text).is_some());
                assert_eq!(&text, expected, "seed {seed}: rewind");
            }
            assert_eq!(text, final_text, "seed {seed}: full rewind");
        }
    }

    // ------------------------------------------------------------ performance

    #[test]
    fn perf_100k_edits_then_100k_undos() {
        use std::time::Instant as Clock;

        const EDITS: usize = 100_000;

        // ~1 MB of realistic-ish note text.
        let mut text: String = "lorem ipsum dolor sit amet, consectetur adipiscing elit\n"
            .repeat(1_000_000 / 56 + 1);
        let baseline = text.clone();
        assert!(text.len() >= 1_000_000, "want a ~1 MB buffer");

        // Each edit its own group, so there really are 100k undos to do.
        let mut history = History::with_limit(EDITS + 10);
        let base = t0();

        let start = Clock::now();
        for i in 0..EDITS {
            let at = text.len();
            let edit = ins(at, "x");
            text.push('x');
            history.record(edit, base + Duration::from_millis(i as u64 * 1_000));
        }
        let record_time = start.elapsed();
        assert_eq!(history.undo_depth(), EDITS);

        let start = Clock::now();
        let mut undone = 0usize;
        while history.can_undo() {
            assert!(history.undo(&mut text).is_some());
            undone += 1;
        }
        let undo_time = start.elapsed();

        assert_eq!(undone, EDITS);
        assert_eq!(text, baseline, "100k undos return the exact original bytes");

        println!(
            "history perf: {EDITS} records in {:?} ({:.2} ns/edit), \
             {EDITS} undos in {:?} ({:.2} ns/undo), buffer {} bytes",
            record_time,
            record_time.as_nanos() as f64 / EDITS as f64,
            undo_time,
            undo_time.as_nanos() as f64 / EDITS as f64,
            text.len(),
        );

        let total = record_time + undo_time;
        assert!(
            total < Duration::from_secs(1),
            "100k records + 100k undos took {total:?}, want well under 1s"
        );
    }
}
