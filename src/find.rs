//! Finding text in the note.
//!
//! At 36,500 notes in one file, scrolling is not a search strategy. This is the
//! whole feature: a query, every match in the document, and a cursor over them.
//!
//! Matching is **case-insensitive for ASCII and exact for everything else**.
//! That is a deliberate limit: a full Unicode case fold would mean lowercasing
//! the entire 9 MB buffer on every keystroke in the query field. Comparing
//! bytes, with ASCII folded, costs one linear scan and no allocation, and gets
//! the behaviour people actually expect from a note search. Two toggles narrow
//! it further — [`MatchOptions::match_case`] compares bytes exactly, and
//! [`MatchOptions::whole_word`] keeps only matches bounded by non-word
//! characters.
//!
//! The query field is a real one-line text field: the caret sits at an offset
//! inside the query and drags a selection behind it, so typing, deleting, word
//! motion and select-all behave the way they do in any Mac search box. That
//! logic is pure and lives here; `main.rs` only routes keystrokes to it.

use std::ops::Range;

/// How the query is compared against the note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchOptions {
    /// Compare bytes exactly instead of folding ASCII case.
    pub match_case: bool,
    /// Keep only matches bounded by non-word characters on both sides.
    pub whole_word: bool,
    /// Read the query as a regular expression rather than as literal text.
    pub regex: bool,
}

/// The largest offset `<= i` that is a character boundary in `s`.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The most matches a search will collect.
///
/// A one-character query on a twenty-year note has millions of them: `.` as the
/// first character of a pattern found 9.4 million and took 183 ms and 150 MB,
/// on the main thread, on every keystroke of typing `.*todo`. Nothing on screen
/// can use more than a screenful and nine hundred rail ticks, and the count says
/// "5000+" rather than a number nobody reads.
pub const MATCH_CAP: usize = 5_000;

/// A "word" character for whole-word matching: alphanumeric or underscore.
/// Everything else — spaces, punctuation, the ends of the text — is a boundary.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `start..end` is bounded by a non-word char (or the text ends) on
/// each side. The neighbours are read one char outward, so the check is O(1).
fn word_bounded(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
    let after_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

/// Two characters equal ignoring case. ASCII takes the cheap path; everything
/// else compares simple (1:1) Unicode lowercase, so "é" ⇄ "É", "π" ⇄ "Π" and
/// "и" ⇄ "И" all match. A full case fold (ß → ss) is deliberately not done — it
/// changes length, and the note search wants the accented/script folding people
/// expect, not dictionary equivalence.
fn ci_eq(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    if a.is_ascii() || b.is_ascii() {
        return a.eq_ignore_ascii_case(&b);
    }
    a.to_lowercase().eq(b.to_lowercase())
}

/// Every non-overlapping match of `query` in `text`, in document order.
///
/// Returns nothing for an empty query. Never returns a range that starts or
/// ends inside a multi-byte character.
pub fn find_all(text: &str, query: &str, opts: MatchOptions) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if opts.regex {
        return match compile(query, opts) {
            Ok(re) => re
                .find_iter(text)
                // A pattern can match nothing at all (`a*`), and a zero-width
                // hit is not something you can step to or replace.
                .filter(|m| m.start() < m.end())
                .map(|m| m.start()..m.end())
                .take(MATCH_CAP)
                .collect(),
            Err(_) => Vec::new(),
        };
    }
    if opts.match_case {
        // The exact path indexes bytes and would underflow computing the last
        // start, so a query longer than the text is empty here. The folded path
        // walks by character and needs no such guard — a case fold can change
        // length, so a shorter text may still match a longer query.
        if query.len() > text.len() {
            return Vec::new();
        }
        find_all_exact(text, query, opts.whole_word)
    } else {
        find_all_folded(text, query, opts.whole_word)
    }
}

/// Build the pattern for a regex search, with the other two toggles folded in:
/// case-insensitive unless Match Case is on, and wrapped in word boundaries when
/// Whole Word is.
///
/// Returns the compile error so the field can go red on a half-typed pattern
/// rather than silently finding nothing.
pub fn compile(query: &str, opts: MatchOptions) -> Result<regex::Regex, regex::Error> {
    let pattern = if opts.whole_word {
        format!(r"\b(?:{query})\b")
    } else {
        query.to_string()
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!opts.match_case)
        // A note is one document; `.` should not run past the end of a line.
        // `^` and `$` mean the line, not the document — a note is one buffer,
        // and anchoring to its ends would make them almost useless. (`.` never
        // crosses a newline either way.)
        .multi_line(true)
        .build()
}

/// Byte-exact substring search — the case-sensitive path. One linear scan, no
/// allocation, guarded onto char boundaries.
fn find_all_exact(text: &str, query: &str, whole_word: bool) -> Vec<Range<usize>> {
    let mut matches = Vec::new();
    let needle = query.as_bytes();
    let haystack = text.as_bytes();
    let last_start = haystack.len() - needle.len();
    let mut at = 0usize;
    while at <= last_start && matches.len() < MATCH_CAP {
        let end = at + needle.len();
        if haystack[at] == needle[0]
            && &haystack[at..end] == needle
            && text.is_char_boundary(at)
            && text.is_char_boundary(end)
            && (!whole_word || word_bounded(text, at, end))
        {
            matches.push(at..end);
            at = end;
        } else {
            at += 1;
        }
    }
    matches
}

/// Case-insensitive substring search folding simple Unicode case — the default
/// path. A match is compared char-for-char, so the matched byte range is always
/// a real slice even when the query and the text differ in case-changed length.
///
/// Finding where to *start* comparing is the whole cost on a big document, and
/// it is done on bytes. An ASCII first character — which is nearly every search
/// anyone types — matches only at a byte equal to it in one of its two cases,
/// and an ASCII byte can only appear at a character boundary in UTF-8, so the
/// scan skips over everything else without decoding it. Decoding all nine
/// megabytes just to find the few hundred places a match could begin was most
/// of the time a search took.
fn find_all_folded(text: &str, query: &str, whole_word: bool) -> Vec<Range<usize>> {
    let needle: Vec<char> = query.chars().collect();
    let first = needle[0];
    let bytes = text.as_bytes();
    let mut matches = Vec::new();

    /// How far a match starting here reaches, if there is one.
    fn match_end(text: &str, start: usize, needle: &[char]) -> Option<usize> {
        let mut ni = 0usize;
        let mut end = start;
        for (off, hc) in text[start..].char_indices() {
            if ni == needle.len() {
                break;
            }
            if !ci_eq(hc, needle[ni]) {
                return None;
            }
            ni += 1;
            end = start + off + hc.len_utf8();
        }
        (ni == needle.len()).then_some(end)
    }

    let take = |start: usize, matches: &mut Vec<Range<usize>>| -> usize {
        if matches.len() >= MATCH_CAP {
            return usize::MAX;
        }
        match match_end(text, start, &needle) {
            // Non-overlapping: the next candidate starts where this one ended.
            Some(end) if !whole_word || word_bounded(text, start, end) => {
                matches.push(start..end);
                end
            }
            _ => start + 1,
        }
    };

    if first.is_ascii() {
        let (lower, upper) = (
            first.to_ascii_lowercase() as u8,
            first.to_ascii_uppercase() as u8,
        );
        // `memchr2` scans a register at a time rather than a byte at a time.
        let mut at = 0usize;
        while let Some(hit) = memchr::memchr2(lower, upper, &bytes[at..]) {
            let start = at + hit;
            at = take(start, &mut matches).max(start + 1);
            if at == usize::MAX {
                break;
            }
        }
    } else {
        let mut next_allowed = 0usize;
        for (start, c) in text.char_indices() {
            if start < next_allowed || !ci_eq(c, first) {
                continue;
            }
            next_allowed = take(start, &mut matches);
            if next_allowed == usize::MAX {
                break;
            }
        }
    }
    matches
}

/// One editable one-line text field. The search query and (in replace mode) the
/// replacement are each a `Field`, so caret motion, selection, deletion, IME
/// composition and per-field undo are written once and shared. Offsets are byte
/// offsets into `text`, always on char boundaries.
#[derive(Clone, Debug, Default)]
struct Field {
    text: String,
    caret: usize,
    anchor: usize,
    /// The composing (IME preedit) run, if a dead key, accent, or CJK input
    /// method is mid-composition — replaced as it updates and consumed on commit.
    marked: Option<Range<usize>>,
    /// `(text, caret, anchor)` snapshots. The field is one short line, so a
    /// snapshot per step is cheap; consecutive inserts coalesce (`inserting`) and
    /// any delete or caret move starts a fresh group.
    undo_stack: Vec<(String, usize, usize)>,
    redo_stack: Vec<(String, usize, usize)>,
    inserting: bool,
}

impl Field {
    /// Replace the whole field and reset its history; the caret lands at the end.
    fn set(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.marked = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.inserting = false;
    }

    fn selection(&self) -> Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    fn offer(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    fn snapshot(&mut self) {
        self.undo_stack.push((self.text.clone(), self.caret, self.anchor));
        self.redo_stack.clear();
    }

    fn undo(&mut self) -> bool {
        self.marked = None;
        self.inserting = false;
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push((self.text.clone(), self.caret, self.anchor));
            (self.text, self.caret, self.anchor) = prev;
            true
        } else {
            false
        }
    }

    fn redo(&mut self) -> bool {
        self.marked = None;
        self.inserting = false;
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push((self.text.clone(), self.caret, self.anchor));
            (self.text, self.caret, self.anchor) = next;
            true
        } else {
            false
        }
    }

    fn insert(&mut self, addition: &str) {
        if !self.inserting {
            self.snapshot();
            self.inserting = true;
        }
        let target = self.marked.take().unwrap_or_else(|| self.selection());
        self.text.replace_range(target.clone(), addition);
        self.caret = target.start + addition.len();
        self.anchor = self.caret;
    }

    fn compose(&mut self, preedit: &str) {
        let target = self.marked.clone().unwrap_or_else(|| self.selection());
        self.text.replace_range(target.clone(), preedit);
        let end = target.start + preedit.len();
        self.caret = end;
        self.anchor = end;
        self.marked = (!preedit.is_empty()).then_some(target.start..end);
    }

    fn unmark(&mut self) {
        self.marked = None;
    }

    fn delete_backward(&mut self) {
        self.marked = None;
        if self.has_selection() {
            self.delete_range(self.selection());
        } else if self.caret > 0 {
            self.delete_range(prev_boundary(&self.text, self.caret)..self.caret);
        }
    }

    fn delete_forward(&mut self) {
        self.marked = None;
        if self.has_selection() {
            self.delete_range(self.selection());
        } else if self.caret < self.text.len() {
            self.delete_range(self.caret..next_boundary(&self.text, self.caret));
        }
    }

    fn delete_word_backward(&mut self) {
        self.marked = None;
        if self.has_selection() {
            self.delete_range(self.selection());
        } else {
            let start = crate::selection::prev_word_start(&self.text, self.caret);
            self.delete_range(start..self.caret);
        }
    }

    fn delete_word_forward(&mut self) {
        self.marked = None;
        if self.has_selection() {
            self.delete_range(self.selection());
        } else {
            let end = crate::selection::next_word_end(&self.text, self.caret);
            self.delete_range(self.caret..end);
        }
    }

    fn delete_to_start(&mut self) {
        self.marked = None;
        if self.has_selection() {
            self.delete_range(self.selection());
        } else {
            self.delete_range(0..self.caret);
        }
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.snapshot();
        self.inserting = false;
        self.text.replace_range(range.clone(), "");
        self.caret = range.start;
        self.anchor = range.start;
    }

    fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    fn move_char(&mut self, forward: bool, extend: bool) {
        if !extend && self.has_selection() {
            let sel = self.selection();
            self.place(if forward { sel.end } else { sel.start }, false);
        } else {
            let to = if forward {
                next_boundary(&self.text, self.caret)
            } else {
                prev_boundary(&self.text, self.caret)
            };
            self.place(to, extend);
        }
    }

    fn move_word(&mut self, forward: bool, extend: bool) {
        let from = if !extend && self.has_selection() {
            let sel = self.selection();
            if forward { sel.end } else { sel.start }
        } else {
            self.caret
        };
        let to = if forward {
            crate::selection::next_word_end(&self.text, from)
        } else {
            crate::selection::prev_word_start(&self.text, from)
        };
        self.place(to, extend);
    }

    fn move_to_edge(&mut self, forward: bool, extend: bool) {
        let to = if forward { self.text.len() } else { 0 };
        self.place(to, extend);
    }

    fn place(&mut self, pos: usize, extend: bool) {
        self.marked = None;
        self.inserting = false;
        self.caret = pos;
        if !extend {
            self.anchor = pos;
        }
    }
}

/// Which field the query bar's keyboard is editing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Which {
    #[default]
    Query,
    Replacement,
}

/// A query, its matches, and which one is current — plus, in replace mode, a
/// replacement field.
///
/// Each field is an editable one-line [`Field`]; `active` is the one the
/// keyboard drives, so the field-editing methods below all act on it.
#[derive(Clone, Debug, Default)]
pub struct Find {
    query: Field,
    replacement: Field,
    active: Which,
    /// Whether the replace row is showing. When off, `active` is always `Query`.
    replacing: bool,
    matches: Vec<Range<usize>>,
    current: Option<usize>,
    opts: MatchOptions,
    /// When set, only this byte range of the document is searched — the note the
    /// caret is in. In a file holding twenty years of notes, "replace all" is a
    /// different and much more frightening command than "replace all *here*".
    scope: Option<Range<usize>>,
    /// Whether the query is a pattern that will not compile. Recomputed by
    /// [`Self::rerun`], so the render path can ask for free.
    broken_pattern: bool,
    /// Whether the last step cycled past an end of the document (⏎ at the last
    /// match, or ⇧⏎ at the first). Surfaced in `status` as a "wrapped" cue and
    /// cleared by anything that is not a wrapping step.
    wrapped: bool,
}

impl Find {
    pub fn new() -> Self {
        Self::default()
    }

    /// The active field — the one the keyboard is editing.
    fn field(&mut self) -> &mut Field {
        match self.active {
            Which::Query => &mut self.query,
            Which::Replacement => &mut self.replacement,
        }
    }

    fn active_field(&self) -> &Field {
        match self.active {
            Which::Query => &self.query,
            Which::Replacement => &self.replacement,
        }
    }

    /// The search text.
    pub fn query(&self) -> &str {
        &self.query.text
    }

    /// The replacement text (empty until typed in replace mode).
    pub fn replacement(&self) -> &str {
        &self.replacement.text
    }

    pub fn replacing(&self) -> bool {
        self.replacing
    }

    /// Whether the keyboard is currently editing the replacement field.
    pub fn active_is_replacement(&self) -> bool {
        self.active == Which::Replacement
    }

    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    /// 1-based position of the current match, for display.
    pub fn position(&self) -> Option<usize> {
        self.current.map(|i| i + 1)
    }

    pub fn current(&self) -> Option<Range<usize>> {
        self.current.and_then(|i| self.matches.get(i).cloned())
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn match_case(&self) -> bool {
        self.opts.match_case
    }

    pub fn whole_word(&self) -> bool {
        self.opts.whole_word
    }

    pub fn is_regex(&self) -> bool {
        self.opts.regex
    }

    pub fn set_regex(&mut self, on: bool) {
        self.opts.regex = on;
    }

    /// Whether the pattern currently in the query field can even be compiled.
    /// A half-typed `[a-` is not "no matches", and the field says so. Decided by
    /// the last [`Self::rerun`], not on the frame that asks.
    pub fn pattern_is_broken(&self) -> bool {
        self.broken_pattern
    }

    /// Whether the search stopped at [`MATCH_CAP`] rather than at the end of the
    /// document.
    pub fn capped(&self) -> bool {
        self.matches.len() >= MATCH_CAP
    }

    /// Limit the search to one range of the document, or lift the limit.
    pub fn set_scope(&mut self, scope: Option<Range<usize>>) {
        self.scope = scope;
    }

    pub fn scope(&self) -> Option<Range<usize>> {
        self.scope.clone()
    }

    // ── The active field's caret and selection ─────────────────────────────

    /// Byte offset of the caret within the active field.
    pub fn caret(&self) -> usize {
        self.active_field().caret
    }

    /// The selected span of the active field, `start <= end`.
    pub fn selection(&self) -> Range<usize> {
        self.active_field().selection()
    }

    pub fn has_selection(&self) -> bool {
        self.active_field().has_selection()
    }

    /// The selected text of the active field, for ⌘C.
    pub fn selected_text(&self) -> &str {
        self.active_field().selected_text()
    }

    // ── What the find bar says ─────────────────────────────────────────────

    /// What the find bar says about the state of the search, to the right of
    /// the query. Empty while there is nothing to report.
    pub fn status(&self) -> String {
        if self.query.text.is_empty() {
            String::new()
        } else if self.broken_pattern {
            "bad pattern".to_string()
        } else if self.matches.is_empty() {
            "no matches".to_string()
        } else {
            let total = if self.capped() {
                // Stopped counting rather than counted them all.
                format!("{}+", self.matches.len())
            } else {
                self.matches.len().to_string()
            };
            let base = format!("{} of {total}", self.position().unwrap_or(1));
            if self.wrapped {
                format!("{base} · wrapped")
            } else {
                base
            }
        }
    }

    // ── Searching ──────────────────────────────────────────────────────────

    /// Re-run the current query against `text`, keeping the match nearest the
    /// caret current. The one full-document scan; everything else defers to it.
    pub fn rerun(&mut self, text: &str, caret: usize) {
        // The pattern is compiled once here rather than in the render path,
        // where asking "is this pattern broken?" cost a full rebuild of the
        // regex on every frame of the caret's fade.
        self.broken_pattern = self.opts.regex
            && !self.query.text.is_empty()
            && compile(&self.query.text, self.opts).is_err();
        self.matches = match self.scope.clone() {
            // Searching a slice and shifting the answers keeps one search
            // routine rather than two. The range is clamped onto character
            // boundaries: it was derived from the document *before* the edit
            // that prompted this search, and slicing a moved boundary panics.
            Some(range) => {
                let start = floor_boundary(text, range.start);
                let end = floor_boundary(text, range.end.max(start));
                find_all(&text[start..end], &self.query.text, self.opts)
                    .into_iter()
                    .map(|hit| hit.start + start..hit.end + start)
                    .collect()
            }
            None => find_all(text, &self.query.text, self.opts),
        };
        self.current = self.nearest_at_or_after(caret);
        self.wrapped = false;
    }

    /// Move the known matches along after an edit to the document, without
    /// searching it again.
    ///
    /// A full scan is 11 ms on a twenty-year note, and running one per keystroke
    /// meant that typing with the find bar open stuttered. The matches an edit
    /// cannot have changed — the ones that sit entirely after it — only need
    /// their offsets shifted; the ones it overlapped are dropped, because the
    /// text under them is not what was matched any more. The caller re-scans
    /// once the typing settles, which is what puts back any match the edit
    /// created.
    ///
    /// `start`, `removed` and `inserted` describe the edit the way
    /// [`crate::history::Edit`] does: `removed` bytes at `start` became
    /// `inserted` bytes.
    pub fn shift_after_edit(&mut self, start: usize, removed: usize, inserted: usize) {
        if self.matches.is_empty() {
            return;
        }
        let old_end = start + removed;
        let current_start = self.current().map(|hit| hit.start);
        self.matches.retain(|hit| hit.end <= start || hit.start >= old_end);
        for hit in &mut self.matches {
            if hit.start >= old_end {
                hit.start = hit.start + inserted - removed;
                hit.end = hit.end + inserted - removed;
            }
        }
        // Keep pointing at the same hit where it survived; otherwise at the one
        // that took its place in document order, so the count and the rail's
        // "you are here" stay sensible until the re-scan lands. Editing the last
        // match leaves nothing after it, and *some* hit has to be current while
        // matches remain — with none, the bar reads "1 of 1" over a document
        // with twelve hits and ⌘G jumps back to the top.
        self.current = current_start.and_then(|was| {
            let was = if was >= old_end {
                was + inserted - removed
            } else {
                was
            };
            let at = self.matches.partition_point(|hit| hit.start < was);
            (!self.matches.is_empty()).then(|| at.min(self.matches.len() - 1))
        });
        // The document moved under the search; whatever the last step wrapped
        // past is no longer the story.
        self.wrapped = false;
    }

    /// Replace the query wholesale and recompute. The caret lands at the end
    /// with nothing selected, and the keyboard returns to the query field. The
    /// match nearest `caret` becomes current, so typing more of a word keeps you
    /// where you were looking instead of jumping back to the top of the document.
    pub fn set_query(&mut self, text: &str, query: String, caret: usize) {
        self.query.set(query);
        self.active = Which::Query;
        self.rerun(text, caret);
    }

    /// Offer the whole active field as a selection, the way a Mac find field
    /// opens: the first keystroke replaces it, ⏎ keeps it.
    pub fn offer(&mut self) {
        self.field().offer();
    }

    /// Show or hide the replace row. Hiding it returns the keyboard to the query.
    pub fn set_replacing(&mut self, on: bool) {
        self.replacing = on;
        if !on {
            self.active = Which::Query;
        }
    }

    /// Move the keyboard between the query and replacement fields (Tab), only
    /// while the replace row is showing.
    pub fn focus_next_field(&mut self) {
        if self.replacing {
            self.active = match self.active {
                Which::Query => Which::Replacement,
                Which::Replacement => Which::Query,
            };
        }
    }

    /// Flip a toggle. The caller re-runs the search; the query text is untouched.
    pub fn set_match_case(&mut self, on: bool) {
        self.opts.match_case = on;
    }

    pub fn set_whole_word(&mut self, on: bool) {
        self.opts.whole_word = on;
    }

    // ── Editing the active field (no search — the caller schedules that) ─────

    pub fn insert(&mut self, addition: &str) {
        self.field().insert(addition);
    }

    pub fn compose(&mut self, preedit: &str) {
        self.field().compose(preedit);
    }

    pub fn unmark(&mut self) {
        self.field().unmark();
    }

    pub fn delete_backward(&mut self) {
        self.field().delete_backward();
    }

    pub fn delete_forward(&mut self) {
        self.field().delete_forward();
    }

    pub fn delete_word_backward(&mut self) {
        self.field().delete_word_backward();
    }

    pub fn delete_word_forward(&mut self) {
        self.field().delete_word_forward();
    }

    pub fn delete_to_start(&mut self) {
        self.field().delete_to_start();
    }

    /// Undo/redo the active field. Returns whether anything changed.
    pub fn undo_query(&mut self) -> bool {
        self.field().undo()
    }

    pub fn redo_query(&mut self) -> bool {
        self.field().redo()
    }

    // ── Moving the caret in the active field (no search) ────────────────────

    pub fn select_all(&mut self) {
        self.field().select_all();
    }

    pub fn move_char(&mut self, forward: bool, extend: bool) {
        self.field().move_char(forward, extend);
    }

    pub fn move_word(&mut self, forward: bool, extend: bool) {
        self.field().move_word(forward, extend);
    }

    pub fn move_to_edge(&mut self, forward: bool, extend: bool) {
        self.field().move_to_edge(forward, extend);
    }

    // ── Cursor over the matches ────────────────────────────────────────────

    /// Advance to the next match, wrapping. `None` when there are none.
    ///
    /// Not an iterator: it moves the *current* match, which is a cursor over a
    /// fixed set rather than a stream being consumed.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        let len = self.matches.len();
        let idx = match self.current {
            Some(i) => (i + 1) % len,
            None => 0,
        };
        // Wrapped when the step returned to or before where it was — i.e. the
        // current match was the last (a single match wraps onto itself).
        self.wrapped = self.current.is_some_and(|i| idx <= i);
        self.current = Some(idx);
        self.current()
    }

    /// Step back to the previous match, wrapping.
    pub fn previous(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            return None;
        }
        let len = self.matches.len();
        let idx = match self.current {
            Some(0) | None => len - 1,
            Some(i) => i - 1,
        };
        self.wrapped = self.current.is_some_and(|i| idx >= i);
        self.current = Some(idx);
        self.current()
    }

    /// Make match `index` the current one — what clicking a rail tick does.
    pub fn set_current(&mut self, index: usize) -> Option<Range<usize>> {
        if index < self.matches.len() {
            self.current = Some(index);
            self.wrapped = false;
            self.current()
        } else {
            None
        }
    }

    /// Matches that overlap `range`, for highlighting one visible line.
    pub fn overlapping(&self, range: Range<usize>) -> impl Iterator<Item = Range<usize>> + '_ {
        // Matches are sorted and non-overlapping, so the run that touches this
        // line is contiguous — find its start, then walk while it overlaps.
        let start = self.matches.partition_point(|m| m.end <= range.start);
        self.matches[start..]
            .iter()
            .take_while(move |m| m.start < range.end)
            .cloned()
    }

    fn nearest_at_or_after(&self, caret: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let at = self.matches.partition_point(|m| m.start < caret);
        Some(if at == self.matches.len() { 0 } else { at })
    }
}

/// The char boundary just before `pos` in `s` (or `0` at the start).
fn prev_boundary(s: &str, pos: usize) -> usize {
    s[..pos].chars().next_back().map_or(0, |c| pos - c.len_utf8())
}

/// The char boundary just after `pos` in `s` (or `pos` at the end).
fn next_boundary(s: &str, pos: usize) -> usize {
    s[pos..].chars().next().map_or(pos, |c| pos + c.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Case-insensitive substring, the default the field opens with.
    fn plain(text: &str, query: &str) -> Vec<Range<usize>> {
        find_all(text, query, MatchOptions::default())
    }

    #[test]
    fn finds_nothing_for_an_empty_query() {
        assert!(plain("anything", "").is_empty());
        assert!(plain("", "x").is_empty());
        assert!(plain("ab", "abc").is_empty(), "query longer than text");
    }

    #[test]
    fn ime_composition_replaces_its_preedit_then_commits() {
        let mut find = Find::new();
        find.insert("caf");
        // A dead-key accent composes over an empty selection, updating live…
        find.compose("´");
        assert_eq!(find.query(), "caf´");
        find.compose("é"); // …the IME replaces the preedit, not appends…
        assert_eq!(find.query(), "café");
        // …and the commit swaps the marked run for the final grapheme once.
        find.insert("é");
        assert_eq!(find.query(), "café");
        assert_eq!(find.caret(), "café".len());
        // A fresh keystroke after the commit is ordinary, not a re-compose.
        find.insert("s");
        assert_eq!(find.query(), "cafés");
    }

    #[test]
    fn finds_every_occurrence() {
        assert_eq!(plain("a b a b a", "a"), vec![0..1, 4..5, 8..9]);
        assert_eq!(plain("banana", "an"), vec![1..3, 3..5]);
    }

    #[test]
    fn matches_are_non_overlapping() {
        // "aaa" contains "aa" twice if you allow overlap; we do not.
        assert_eq!(plain("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn ascii_case_is_ignored_by_default() {
        assert_eq!(plain("Hello hello HELLO", "hello").len(), 3);
        assert_eq!(plain("Hello", "HeLLo"), vec![0..5]);
    }

    #[test]
    fn match_case_compares_exactly() {
        let opts = MatchOptions { match_case: true, whole_word: false, regex: false };
        assert_eq!(find_all("Hello hello HELLO", "hello", opts), vec![6..11]);
        assert!(find_all("Hello", "hello", opts).is_empty());
    }

    #[test]
    fn whole_word_needs_non_word_boundaries() {
        let opts = MatchOptions { match_case: false, whole_word: true, regex: false };
        // "cat" is whole in "a cat." but not in "cats" or "scatter".
        assert_eq!(find_all("a cat, cats, scatter", "cat", opts), vec![2..5]);
        // Underscores and digits are word characters, so they block a boundary.
        assert!(find_all("cat_", "cat", opts).is_empty());
        assert!(find_all("cat9", "cat", opts).is_empty());
        // Bounded by the ends of the text and by punctuation.
        assert_eq!(find_all("cat", "cat", opts), vec![0..3]);
        assert_eq!(find_all("(cat)", "cat", opts), vec![1..4]);
    }

    #[test]
    fn whole_word_and_match_case_combine() {
        let opts = MatchOptions { match_case: true, whole_word: true, regex: false };
        assert_eq!(find_all("Cat cat CAT", "cat", opts), vec![4..7]);
    }

    #[test]
    fn non_ascii_folds_case_and_never_splits_a_character() {
        let text = "café CAFÉ naïve 日本語 🎉";
        // Accented and other simple-cased text folds: "café" finds "CAFÉ" too.
        assert_eq!(plain(text, "café").len(), 2);
        assert_eq!(plain(text, "CAFÉ").len(), 2);
        assert_eq!(plain(text, "日本").len(), 1);
        assert_eq!(plain(text, "🎉").len(), 1);
        for found in plain(text, "a") {
            assert!(text.is_char_boundary(found.start));
            assert!(text.is_char_boundary(found.end));
        }
    }

    #[test]
    fn unicode_case_folds_across_scripts() {
        // Greek and Cyrillic fold the way accented Latin does.
        assert_eq!(plain("ΑΘΗΝΑ αθηνα", "αθηνα").len(), 2);
        assert_eq!(plain("Привет привет", "ПРИВЕТ").len(), 2);
        // A folded match is still a real, boundary-safe slice of the text.
        for found in plain("STRAßE straße", "stra") {
            assert!("STRAßE straße".is_char_boundary(found.start));
            assert_eq!(found.end - found.start, "stra".len());
        }
        // Case-sensitive still means byte-exact.
        let cs = MatchOptions { match_case: true, whole_word: false, regex: false };
        assert_eq!(find_all("café CAFÉ", "café", cs), vec![0..5]);
        // A fold can make the query longer in bytes than the matching text
        // ("ẞ" is 3 bytes, "ß" is 2): the folded path must not be length-gated.
        assert_eq!(plain("ß", "ẞ"), vec![0..2]);
    }

    #[test]
    fn every_match_is_a_real_slice_of_the_text() {
        let text = "one two three two one 日本語 two";
        for found in plain(text, "two") {
            assert!(text[found.clone()].eq_ignore_ascii_case("two"), "{found:?}");
        }
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let text = "x x x";
        let mut find = Find::new();
        find.set_query(text, "x".into(), 0);
        assert_eq!(find.matches().len(), 3);
        assert_eq!(find.current(), Some(0..1));
        assert_eq!(find.next(), Some(2..3));
        assert_eq!(find.next(), Some(4..5));
        assert_eq!(find.next(), Some(0..1), "wraps forward");
        assert_eq!(find.previous(), Some(4..5), "wraps backward");
    }

    #[test]
    fn set_current_selects_a_match_by_index() {
        let text = "x x x";
        let mut find = Find::new();
        find.set_query(text, "x".into(), 0);
        assert_eq!(find.set_current(2), Some(4..5));
        assert_eq!(find.position(), Some(3));
        assert_eq!(find.set_current(9), None, "out of range leaves it be");
        assert_eq!(find.current(), Some(4..5));
    }

    #[test]
    fn the_match_nearest_the_caret_becomes_current() {
        let text = "alpha beta alpha beta alpha";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), 12);
        assert_eq!(find.current(), Some(22..27), "the match at or after 12");
        assert_eq!(find.position(), Some(3));
    }

    #[test]
    fn the_status_says_what_the_search_found() {
        let text = "cat cart cattle";
        let mut find = Find::new();
        assert_eq!(find.status(), "", "nothing typed yet, nothing to report");
        find.set_query(text, "ca".into(), 0);
        assert_eq!(find.status(), "1 of 3");
        find.next();
        assert_eq!(find.status(), "2 of 3");
        find.set_query(text, "zebra".into(), 0);
        assert_eq!(find.status(), "no matches");
    }

    #[test]
    fn rerunning_keeps_the_query_and_follows_the_caret() {
        let mut find = Find::new();
        find.set_query("cat cart", "ca".into(), 0);
        assert_eq!(find.position(), Some(1));
        // The text grew a match ahead of the caret; the query is untouched and
        // the hit nearest the caret is still the current one.
        find.rerun("ca cat cart", 3);
        assert_eq!(find.query(), "ca");
        assert_eq!(find.matches().len(), 3);
        assert_eq!(find.position(), Some(2));
    }

    #[test]
    fn a_caret_past_the_last_match_wraps_to_the_first() {
        let text = "alpha .... ";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), text.len());
        assert_eq!(find.current(), Some(0..5));
    }

    // ── The query field ────────────────────────────────────────────────────

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut find = Find::new();
        find.set_query("", "ac".into(), 0);
        // Caret is at the end after set_query; walk it left one char.
        find.move_char(false, false);
        assert_eq!(find.caret(), 1);
        find.insert("b");
        assert_eq!(find.query(), "abc");
        assert_eq!(find.caret(), 2, "caret sits after the inserted text");
        assert!(!find.has_selection());
    }

    #[test]
    fn inserting_replaces_the_selection() {
        let mut find = Find::new();
        find.set_query("", "hello".into(), 0);
        find.select_all();
        assert_eq!(find.selected_text(), "hello");
        find.insert("x");
        assert_eq!(find.query(), "x", "first keystroke replaces the offered query");
        assert_eq!(find.caret(), 1);
    }

    #[test]
    fn backspace_deletes_the_char_before_the_caret_not_the_tail() {
        let mut find = Find::new();
        find.set_query("", "abcd".into(), 0);
        find.move_char(false, false); // caret after "abc", before "d"
        find.delete_backward();
        assert_eq!(find.query(), "abd", "deletes at the caret, not the end");
        assert_eq!(find.caret(), 2);
    }

    #[test]
    fn backspace_deletes_a_selection_whole() {
        let mut find = Find::new();
        find.set_query("", "hello".into(), 0);
        find.select_all();
        find.delete_backward();
        assert!(find.query().is_empty());
        assert_eq!(find.caret(), 0);
        find.delete_backward(); // empty query must not panic
    }

    #[test]
    fn backspace_over_a_multibyte_char_stays_on_a_boundary() {
        let mut find = Find::new();
        find.set_query("", "café".into(), 0);
        find.delete_backward();
        assert_eq!(find.query(), "caf");
        assert_eq!(find.caret(), 3);
    }

    #[test]
    fn forward_and_word_deletes_edit_the_query() {
        let mut find = Find::new();
        find.set_query("", "the quick fox".into(), 0);
        find.move_to_edge(false, false); // caret 0
        find.delete_forward();
        assert_eq!(find.query(), "he quick fox", "⌦ removes the char after");
        find.delete_word_forward();
        assert_eq!(find.query(), " quick fox", "⌥⌦ removes 'he'");
        find.move_to_edge(true, false); // caret at end
        find.delete_word_backward();
        assert_eq!(find.query(), " quick ", "⌥⌫ removes 'fox'");
        find.delete_to_start();
        assert_eq!(find.query(), "", "⌘⌫ clears to the start");
    }

    #[test]
    fn the_query_field_undoes_and_redoes() {
        let mut find = Find::new();
        find.set_query("", String::new(), 0);
        find.insert("cat"); // one typing run
        find.move_char(false, false); // break the run
        find.insert("X");
        assert_eq!(find.query(), "caXt");
        assert!(find.undo_query());
        assert_eq!(find.query(), "cat", "undo peels off the second edit");
        assert!(find.undo_query());
        assert_eq!(find.query(), "", "undo peels off the typing run");
        assert!(!find.undo_query(), "nothing left to undo");
        assert!(find.redo_query());
        assert_eq!(find.query(), "cat", "redo restores the run");
    }

    #[test]
    fn char_motion_collapses_a_selection_to_its_near_edge() {
        let mut find = Find::new();
        find.set_query("", "hello".into(), 0);
        find.select_all(); // anchor 0, caret 5
        find.move_char(false, false);
        assert_eq!(find.caret(), 0, "plain ← lands on the near edge");
        assert!(!find.has_selection());
        find.select_all();
        find.move_char(true, false);
        assert_eq!(find.caret(), 5, "plain → lands on the far edge");
    }

    #[test]
    fn shift_arrows_extend_the_selection() {
        let mut find = Find::new();
        find.set_query("", "hello".into(), 0);
        find.move_to_edge(false, false); // caret at 0
        find.move_char(true, true);
        find.move_char(true, true);
        assert_eq!(find.selection(), 0..2);
        assert_eq!(find.selected_text(), "he");
    }

    #[test]
    fn word_motion_walks_the_query_by_word() {
        let mut find = Find::new();
        find.set_query("", "the quick fox".into(), 0);
        find.move_to_edge(false, false); // caret at 0
        find.move_word(true, false);
        assert_eq!(find.caret(), 3, "end of 'the'");
        find.move_word(true, false);
        assert_eq!(find.caret(), 9, "end of 'quick'");
        find.move_word(false, false);
        assert_eq!(find.caret(), 4, "back to the start of 'quick'");
    }

    #[test]
    fn editing_the_query_recomputes_when_rerun() {
        // Editing no longer searches on its own — the view debounces the scan —
        // so the caller runs it after an edit. This mirrors that pairing.
        let text = "cat cart cattle";
        let mut find = Find::new();
        find.insert("cat");
        find.rerun(text, 0);
        assert_eq!(find.matches().len(), 2, "cat, cattle");
        find.insert("t");
        find.rerun(text, 0);
        assert_eq!(find.matches().len(), 1, "catt in cattle");
        find.delete_backward();
        find.rerun(text, 0);
        assert_eq!(find.query(), "cat");
        find.delete_backward();
        find.rerun(text, 0);
        assert_eq!(find.query(), "ca");
        assert_eq!(find.matches().len(), 3, "cat, cart, cattle");
    }

    #[test]
    fn toggling_match_case_changes_the_result_on_rerun() {
        let text = "Cat cat";
        let mut find = Find::new();
        find.set_query(text, "cat".into(), 0);
        assert_eq!(find.matches().len(), 2);
        find.set_match_case(true);
        find.rerun(text, 0);
        assert_eq!(find.matches().len(), 1, "only the lowercase cat now");
    }

    #[test]
    fn overlapping_returns_only_the_matches_on_that_line() {
        let text = "aa\nbb aa\ncc aa aa";
        let mut find = Find::new();
        find.set_query(text, "aa".into(), 0);
        assert_eq!(find.matches().len(), 4);
        // Line 1 is bytes 3..8 ("bb aa")
        let on_line = find.overlapping(3..8).collect::<Vec<_>>();
        assert_eq!(on_line, vec![6..8]);
        // Line 2 is bytes 9..17 ("cc aa aa")
        let on_line = find.overlapping(9..17).collect::<Vec<_>>();
        assert_eq!(on_line, vec![12..14, 15..17]);
        // A line with no matches
        assert_eq!(find.overlapping(0..0).count(), 0);
    }

    #[test]
    fn searching_a_large_document_is_fast_enough_to_type_into() {
        let text = crate::corpus::generate(crate::corpus::CorpusSpec::with_notes(20_000));
        let started = std::time::Instant::now();
        let matches = find_all(&text, "the deadline", MatchOptions::default());
        let elapsed = started.elapsed();
        eprintln!(
            "[find] {} bytes, {} matches in {elapsed:?}",
            text.len(),
            matches.len()
        );
        let budget = if cfg!(debug_assertions) { 2_000 } else { 100 };
        assert!(
            elapsed.as_millis() < budget,
            "search took {elapsed:?}, over the {budget} ms budget"
        );
        for found in matches {
            assert!(text[found.clone()].eq_ignore_ascii_case("the deadline"));
        }
    }
    // ── Patterns, and searching one note ─────────────────────────────────────

    /// A pattern search finds what a literal one cannot.
    #[test]
    fn regex_finds_a_pattern() {
        let text = "call 555-1234 or 555-9876 today";
        let opts = MatchOptions {
            regex: true,
            ..Default::default()
        };
        let found = find_all(text, r"\d{3}-\d{4}", opts);
        assert_eq!(found.len(), 2);
        assert_eq!(&text[found[0].clone()], "555-1234");
    }

    /// A pattern that cannot compile finds nothing and says so, rather than
    /// throwing or matching everything.
    #[test]
    fn a_broken_pattern_finds_nothing() {
        let opts = MatchOptions {
            regex: true,
            ..Default::default()
        };
        assert!(find_all("anything", "[a-", opts).is_empty());

        let mut find = Find::new();
        find.set_regex(true);
        find.set_query("anything", "[a-".into(), 0);
        assert!(find.pattern_is_broken());
        find.set_query("anything", "[a-z]".into(), 0);
        assert!(!find.pattern_is_broken());
    }

    /// A zero-width match is not somewhere you can stand.
    #[test]
    fn empty_matches_are_dropped() {
        let opts = MatchOptions {
            regex: true,
            ..Default::default()
        };
        assert!(find_all("aaa", "b*", opts).is_empty());
    }

    /// Scoped to one note, a search ignores the rest of the document — which is
    /// what makes Replace All a command you can press.
    #[test]
    fn a_scope_limits_the_search() {
        let text = "alpha here\n---\nalpha there\n---\nalpha everywhere";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), 0);
        assert_eq!(find.matches().len(), 3);

        // The middle note only.
        find.set_scope(Some(15..27));
        find.rerun(text, 15);
        assert_eq!(find.matches(), &[15..20], "one match, at its document offset");
        assert_eq!(&text[15..20], "alpha");

        find.set_scope(None);
        find.rerun(text, 0);
        assert_eq!(find.matches().len(), 3);
    }

    // ── Shifting matches after a document edit ───────────────────────────────

    /// The matches after an edit have to be where the text is, or the
    /// highlights sit on the wrong words until the re-scan lands.
    #[test]
    fn an_edit_before_the_matches_shifts_them() {
        let text = "xx alpha yy alpha";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), 0);
        assert_eq!(find.matches(), &[3..8, 12..17]);

        // "zz" typed at the very start: two bytes in, nothing removed.
        let after = "zzxx alpha yy alpha";
        find.shift_after_edit(0, 0, 2);
        assert_eq!(find.matches(), &[5..10, 14..19]);
        for hit in find.matches() {
            assert_eq!(&after[hit.clone()], "alpha");
        }
    }

    #[test]
    fn an_edit_inside_a_match_drops_it() {
        let text = "alpha beta alpha";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), 0);
        assert_eq!(find.matches().len(), 2);

        // A character deleted inside the first match.
        find.shift_after_edit(2, 1, 0);
        assert_eq!(
            find.matches(),
            &[10..15],
            "the broken match is gone and the one after it moved"
        );
        assert_eq!(find.current(), Some(10..15), "the current hit follows");
    }

    /// Editing the *last* match leaves nothing after it, and some hit still has
    /// to be current: with none, the bar reads "1 of 1" over a document full of
    /// hits, and ⌘G jumps back to the top instead of stepping on.
    #[test]
    fn editing_the_last_match_keeps_a_current_one() {
        let text = "alpha beta alpha";
        let mut find = Find::new();
        find.set_query(text, "alpha".into(), 11);
        assert_eq!(find.current(), Some(11..16));

        find.shift_after_edit(12, 1, 0);
        assert_eq!(find.matches(), &[0..5]);
        assert_eq!(find.current(), Some(0..5), "the surviving match is current");
        assert_eq!(find.status(), "1 of 1");
    }

    #[test]
    fn a_shift_matches_a_rescan_when_the_edit_touches_nothing() {
        let text = "one alpha two alpha three alpha";
        for (at, removed, inserted, edited) in [
            (0usize, 0usize, "ZZ", "ZZone alpha two alpha three alpha"),
            (4, 0, "", "one alpha two alpha three alpha"),
            (25, 0, " and", "one alpha two alpha three and alpha"),
        ] {
            let mut shifted = Find::new();
            shifted.set_query(text, "alpha".into(), 0);
            shifted.shift_after_edit(at, removed, inserted.len());

            let mut rescanned = Find::new();
            rescanned.set_query(edited, "alpha".into(), 0);
            assert_eq!(
                shifted.matches(),
                rescanned.matches(),
                "shifting disagreed with a re-scan for {edited:?}"
            );
        }
    }
}
