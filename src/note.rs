//! Pure single-note document logic — no GPUI dependency.
//!
//! One plain-text buffer. Lines are split on `\n`.
//!
//! # Two levels of structure
//!
//! * **Lines** — the raw `\n`-separated rows. `bring_line_up` moves one line to
//!   the top of the buffer.
//! * **Blocks** — the real unit of "a note". A block is the contiguous run of
//!   lines between two markdown thematic breaks (`---`). A block may span many
//!   lines and may be empty. See [`Note::blocks`].
//!
//! # Line endings
//!
//! Only `\n` is treated as a line terminator. **CRLF (`\r\n`) documents are not
//! supported**: a `\r` stays at the end of the line's content and will be
//! preserved byte-for-byte through every operation here. (One convenience:
//! [`is_separator_line`] trims ASCII whitespace, and `\r` is ASCII whitespace,
//! so `"---\r"` *is* recognised as a separator. Everything else about CRLF —
//! block contents, byte offsets, rebuilt joins — keeps the stray `\r`.)

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::fences::FenceMap;
use crate::index::LineIndex;

/// What [`Note::new_block_at_top`] puts in front of the document: an empty
/// note, then its rule. Public because starting a note is an ordinary insertion
/// at byte 0, and the caller records exactly these bytes in the undo history.
pub const NEW_BLOCK_PREFIX: &str = "\n---\n";

/// True when a line is a markdown thematic break.
///
/// After trimming ASCII whitespace the line must consist of 3 or more of the
/// *same* character drawn from `-`, `*`, `_`, optionally with ASCII whitespace
/// between them (per CommonMark). So `"---"`, `"***"`, `"___"`, `"- - -"` and
/// `"  ---  "` are all separators; `"--"`, `"-*-"`, `"---a"` and a line of only
/// whitespace are not.
///
/// Deliberate deviation from CommonMark: leading indentation is not limited to
/// three spaces (CommonMark would make `"    ---"` an indented code block).
pub fn is_separator_line(line: &str) -> bool {
    let trimmed = line.trim_matches(|c: char| c.is_ascii_whitespace());
    let mut chars = trimmed.chars();
    let marker = match chars.next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    let mut count = 1usize;
    for c in chars {
        if c == marker {
            count += 1;
        } else if c.is_ascii_whitespace() {
            continue;
        } else {
            return false;
        }
    }
    count >= 3
}

/// Line indices that separate one note from the next.
///
/// A boundary is an explicit thematic break (`---`) and nothing else. Blank
/// lines are yours to use: paragraph spacing, breathing room in a long note,
/// a run of them while you think. None of it splits the note.
///
/// A `---` inside a fenced code block is code — someone's YAML front matter or
/// a markdown example — so it is not a boundary. That is the only reason this
/// has to walk the document rather than test one line at a time.
///
/// The fence has to be *closed* to count. A fence you have opened and not yet
/// closed runs to the end of the document, so treating it as code would mean
/// that typing ``` swallowed every note below it until you typed the closing
/// fence — which is most of the time you spend writing a code block.
pub fn boundary_lines(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    // Separators found since the currently open fence began. They are code if
    // it closes, and notes if it never does.
    let mut inside_open_fence = Vec::new();
    let mut in_fence = false;
    // A note file written elsewhere may open with YAML front matter, whose two
    // `---` rules delimit metadata rather than notes — and reading them as
    // boundaries starts the document with two empty notes. Recognising it was
    // tried and taken out again: every test for "this looks like metadata"
    // (`key: value`, `- item`, an indented continuation) is also what an
    // ordinary first note looks like, so a note reading "Groceries: milk and
    // eggs" between two rules silently merged three notes into one. A rule is a
    // rule. Guessing otherwise costs more than it buys.
    for (i, line) in text.split('\n').enumerate() {
        if crate::markdown::is_fence(line) {
            if in_fence {
                inside_open_fence.clear();
            }
            in_fence = !in_fence;
        } else if is_separator_line(line) {
            if in_fence {
                inside_open_fence.push(i);
            } else {
                out.push(i);
            }
        }
    }
    // The fence never closed, so nothing was ever inside a code block.
    out.extend(inside_open_fence);
    out.sort_unstable();
    out
}

/// Is line `line` a note boundary? Answered from the line itself plus the fence
/// state the caller already tracks, so the renderer can ask per visible row
/// without ever scanning the document.
///
/// `in_closed_fence` is [`crate::fences::FenceMap::in_closed_fence`] — a fence
/// that is still being typed does not count. Equivalent to
/// `boundary_lines(text).contains(&line)`, which
/// `boundaries_agree_with_the_per_line_check` holds it to.
pub fn is_boundary_line(
    text: &str,
    index: &LineIndex,
    line: usize,
    in_closed_fence: bool,
) -> bool {
    if in_closed_fence {
        return false;
    }
    let Some(raw) = index.line_range(line).map(|(a, b)| &text[a..b]) else {
        return false;
    };
    is_separator_line(raw)
}

/// The range a backwards delete should take when the caret sits at the very
/// start of a line and a separator is what comes before it.
///
/// The rule is drawn as a hairline and its `---` is invisible, but the three
/// characters are real. Deleting one at a time revealed them glued to the end
/// of the note above — dashes the user never typed. Taking the whole line in
/// one keystroke merges the two notes, which is what a delete there means.
pub fn separator_before(
    text: &str,
    index: &LineIndex,
    fences: &FenceMap,
    offset: usize,
) -> Option<Range<usize>> {
    let line = index.line_at(offset);
    if line == 0 || offset != index.line_start(line) {
        return None;
    }
    let above = line - 1;
    (is_rule(text, index, fences, above))
        .then(|| index.line_start(above)..index.line_start(line))
}

/// Whether `line` is a rule the document is actually divided by — the same
/// question [`is_boundary_line`] answers, so a `---` inside a closed fence is
/// code here too rather than something a delete may swallow whole.
fn is_rule(text: &str, index: &LineIndex, fences: &FenceMap, line: usize) -> bool {
    index
        .line_range(line)
        .is_some_and(|(a, b)| is_separator_line(&text[a..b]))
        && !fences.in_closed_fence(line)
}

/// The same, forwards: the caret at the end of a line with a separator below.
pub fn separator_after(
    text: &str,
    index: &LineIndex,
    fences: &FenceMap,
    offset: usize,
) -> Option<Range<usize>> {
    let line = index.line_at(offset);
    if offset != index.line_end(line) {
        return None;
    }
    let below = line + 1;
    (below < index.line_count() && is_rule(text, index, fences, below))
        .then(|| index.line_end(line)..index.line_end(below))
}

/// Pull `start` forward so a deletion ending at `caret` cannot reach back
/// through a separator into the note above.
///
/// Word-wise deletion scans by word, and a rule is not a word — so ⌥⌫ at the
/// head of a note used to swallow the separator and take the last word of the
/// previous note with it.
pub fn clamp_within_note(
    text: &str,
    index: &LineIndex,
    fences: &FenceMap,
    start: usize,
    caret: usize,
) -> usize {
    let first = index.line_at(start);
    let last = index.line_at(caret);
    for line in (first..last).rev() {
        if is_rule(text, index, fences, line) {
            // Stop at the head of the line below the rule.
            return index.line_start(line + 1);
        }
    }
    start
}

/// Pull `end` back so a deletion starting at `caret` cannot reach *forward*
/// through a separator into the note below.
///
/// The mirror of [`clamp_within_note`]: forward word-deletion (⌥⌦) scans by
/// word, and a `---` rule is not a word, so at the end of a note it used to
/// swallow the separator and the first word of the note below, merging the two.
pub fn clamp_within_note_forward(
    text: &str,
    index: &LineIndex,
    fences: &FenceMap,
    caret: usize,
    end: usize,
) -> usize {
    let first = index.line_at(caret);
    let last = index.line_at(end);
    for line in (first + 1)..=last {
        if is_rule(text, index, fences, line) {
            // Stop at the end of the line above the rule.
            return index.line_end(line - 1);
        }
    }
    end
}

/// One note: the contiguous run of lines between two separator lines.
///
/// A block has **no content** exactly when `start == end`. Only then may
/// `first_line`/`last_line` be degenerate: for a block with zero content lines
/// (two adjacent separators, or a separator at the very start/end of the
/// document) `last_line` is `first_line - 1` (saturating, so both are `0` when
/// the block sits before line 0) and `first_line` may equal the line count.
/// Treat `first_line..=last_line` as meaningful only when `start < end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// 0-based index of this block among blocks().
    pub index: usize,
    /// Byte offset of the first byte of block content.
    pub start: usize,
    /// Byte offset one past the last byte of block content (no trailing newline).
    pub end: usize,
    /// Line index of the separator line immediately preceding this block, if any.
    /// `None` for the first block.
    pub sep_line: Option<usize>,
    /// First content line index of this block.
    pub first_line: usize,
    /// Last content line index of this block, inclusive.
    pub last_line: usize,
}

/// One note changing places, as the two edits it really is.
///
/// A move is a cut and a paste, and saying so is what keeps it cheap: the
/// caller replays these two spans into its line index, its fence map and its
/// undo history instead of rebuilding all three against a document that was
/// taken apart and put back together.
///
/// The removal is described against the buffer as it stood *before* the move,
/// the insertion against the buffer with the removal already applied — the
/// order they were performed in, and the order [`crate::history::History`]
/// wants them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMove {
    /// Where the note (with the rule above it) was lifted from.
    pub cut_at: usize,
    /// The bytes lifted out: the newline before the rule, the rule, and the
    /// note itself.
    pub cut_text: String,
    /// Where it was put back down, in the buffer after the cut.
    pub insert_at: usize,
    /// The bytes put back: the note, then the rule, so the note lands above it.
    pub insert_text: String,
    /// Byte offset of the moved note's first character in the new text.
    pub moved_to: usize,
}

/// In-memory single note with efficient line operations for a personal note size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Note {
    text: String,
}

impl Note {
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Mutable access to the raw buffer, for callers that apply their own edits
    /// (undo/redo replays spans directly). Anything derived from the text —
    /// line index, fence map, block list — must be re-derived afterwards.
    pub fn text_mut(&mut self) -> &mut String {
        &mut self.text
    }



    /// Delete the half-open byte range `[start, end)` (clamped, char-aligned).
    pub fn delete_range(&mut self, start: usize, end: usize) {
        let (start, end) = self.clamp_range(start, end);
        if start < end {
            self.text.replace_range(start..end, "");
        }
    }

    /// Replace `[start, end)` with `replacement` (clamped to char boundaries).
    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        let (start, end) = self.clamp_range(start, end);
        self.text.replace_range(start..end, replacement);
    }

    /// Clamp a byte offset to a valid UTF-8 char boundary within the buffer.
    pub fn clamp_offset(&self, offset: usize) -> usize {
        floor_char_boundary(&self.text, offset.min(self.text.len()))
    }

    /// Clamp a half-open range to char boundaries.
    pub fn clamp_range(&self, start: usize, end: usize) -> (usize, usize) {
        let start = floor_char_boundary(&self.text, start.min(self.text.len()));
        let end = ceil_char_boundary(&self.text, end.min(self.text.len()));
        if start <= end {
            (start, end)
        } else {
            (end, start)
        }
    }

    /// Logical lines (split on `\n`). A trailing empty line after a final `\n`
    /// is preserved so typing Enter feels natural.
    pub fn lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            return vec![""];
        }
        self.text.split('\n').collect()
    }

    pub fn line_count(&self) -> usize {
        self.lines().len()
    }

    /// Byte range of line `line_idx` content **without** the trailing newline.
    /// Returns `None` if the index is out of range.
    pub fn line_byte_range(&self, line_idx: usize) -> Option<(usize, usize)> {
        let lines = self.lines();
        if line_idx >= lines.len() {
            return None;
        }
        let mut start = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let end = start + line.len();
            if i == line_idx {
                return Some((start, end));
            }
            start = end + 1;
        }
        None
    }

    /// Which line contains byte offset `offset`?
    ///
    /// For `"aa\\nbb\\ncc"`: bytes `0..2` and the `\\n` at `2` are line 0;
    /// `3..5` and `\\n` at `5` are line 1; `6..` is line 2.
    pub fn line_index_at(&self, offset: usize) -> usize {
        let offset = offset.min(self.text.len());
        let lines = self.lines();
        let last = lines.len().saturating_sub(1);
        let mut start = 0usize;
        for (i, line) in lines.into_iter().enumerate() {
            let content_end = start + line.len();
            let span_end = if i < last {
                content_end // `\n` is at content_end; include via `<=`
            } else {
                self.text.len()
            };
            if offset <= span_end {
                return i;
            }
            start = content_end + 1;
        }
        last
    }

    /// All blocks, in document order. ALWAYS returns at least one block
    /// (an empty document is one empty block: start == end == 0).
    /// A block's content excludes the separator lines that delimit it.
    /// A block may be empty (two adjacent separators, or a separator at the
    /// very start or very end of the document).
    ///
    /// `blocks().len() == separator_line_indices().len() + 1`.
    pub fn blocks(&self) -> Vec<Block> {
        let lines = self.lines();
        // Byte range (content only, no `\n`) of every line.
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(lines.len());
        let mut cursor = 0usize;
        for line in &lines {
            let end = cursor + line.len();
            ranges.push((cursor, end));
            cursor = end + 1;
        }
        let text_len = self.text.len();

        // Build one block for the half-open line span `[from, to)`.
        let make = |index: usize, sep_line: Option<usize>, from: usize, to: usize| -> Block {
            if from < to {
                let last_line = to - 1;
                Block {
                    index,
                    start: ranges[from].0,
                    end: ranges[last_line].1,
                    sep_line,
                    first_line: from,
                    last_line,
                }
            } else {
                // Empty block: it sits at the start of line `from`, or at the
                // very end of the document when `from` is past the last line.
                let at = ranges.get(from).map_or(text_len, |r| r.0);
                Block {
                    index,
                    start: at,
                    end: at,
                    sep_line,
                    first_line: from,
                    last_line: from.saturating_sub(1),
                }
            }
        };

        let boundaries = boundary_lines(&self.text);
        let is_boundary = |i: usize| boundaries.binary_search(&i).is_ok();

        let mut blocks = Vec::new();
        let mut sep_line: Option<usize> = None;
        let mut from = 0usize;
        for (i, _line) in lines.iter().enumerate() {
            if is_boundary(i) {
                blocks.push(make(blocks.len(), sep_line, from, i));
                sep_line = Some(i);
                from = i + 1;
            }
        }
        blocks.push(make(blocks.len(), sep_line, from, lines.len()));
        blocks
    }


    /// Index of the block containing byte `offset`. If `offset` falls on a
    /// separator line, return the index of the block that FOLLOWS that
    /// separator (the separator visually belongs to the note under it).
    pub fn block_index_at(&self, offset: usize) -> usize {
        let line_idx = self.line_index_at(offset);
        // Number of boundary lines at or before `line_idx`:
        //  * `line_idx` is content    → equals the count strictly before it,
        //                               which is this block's index.
        //  * `line_idx` is a boundary → one more, i.e. the block *below* it.
        boundary_lines(&self.text)
            .iter()
            .take_while(|&&b| b <= line_idx)
            .count()
    }

    /// Line indices of every separator line, ascending.
    pub fn separator_line_indices(&self) -> Vec<usize> {
        boundary_lines(&self.text)
    }

    /// Move block `idx` to the top of the document, preserving the relative
    /// order of the other blocks. Returns the move that was made, or `None`
    /// when it is a no-op (idx == 0 or out of range).
    ///
    /// See [`BlockMove`]: this is a cut and a paste of one note's bytes, not a
    /// rebuild of the document.
    /// What moving block `idx` to the top would do, without doing it.
    ///
    /// The app applies the two halves one at a time so it can patch its line
    /// index between them — each half is an ordinary edit against the buffer as
    /// it stands, which is the only description those patches accept.
    pub fn plan_bring_block_up(&self, index: &LineIndex, idx: usize) -> Option<BlockMove> {
        self.plan_move(index, idx, 0)
    }

    /// The same for swapping block `idx` with the one above it.
    pub fn plan_move_block_up_one(&self, index: &LineIndex, idx: usize) -> Option<BlockMove> {
        self.plan_move(index, idx, idx.checked_sub(1)?)
    }

    /// Perform a planned move: the removal, then the insertion.
    pub fn apply_move(&mut self, planned: &BlockMove) {
        let BlockMove {
            cut_at,
            cut_text,
            insert_at,
            insert_text,
            ..
        } = planned;
        self.text.replace_range(*cut_at..cut_at + cut_text.len(), "");
        self.text.insert_str(*insert_at, insert_text);
    }

    /// Lift block `idx` out of the document — with the rule above it — and put
    /// it back down in front of block `before`.
    ///
    /// The whole operation is one removal and one insertion on the buffer:
    /// every byte outside the moved note is untouched, which is what lets the
    /// caller patch its line index instead of rebuilding it, and what keeps the
    /// undo entry the size of one note rather than the size of everything above
    /// it. Promoting the oldest note in a twenty-year file used to copy the
    /// document three times and record eighteen megabytes of history.
    ///
    /// The rule that travels with the note is the one that was above it, taken
    /// byte-for-byte: a `***` you typed stays `***`. (Rebuilding joined the
    /// blocks back together with a canonical `---`, so moving one note quietly
    /// rewrote every separator in the document.)
    fn plan_move(&self, index: &LineIndex, idx: usize, before: usize) -> Option<BlockMove> {
        if idx == 0 || before >= idx {
            return None;
        }
        let boundaries = boundary_lines(&self.text);
        let line_count = index.line_count();
        // Block `i` starts on the line after boundary `i - 1`, and the first
        // block starts at the top.
        let block_first_line = |i: usize| -> usize {
            match i.checked_sub(1) {
                None => 0,
                Some(previous) => boundaries[previous] + 1,
            }
        };
        let line_start = |line: usize| -> usize {
            if line < line_count {
                index.line_start(line)
            } else {
                self.text.len()
            }
        };

        // The lines the move lifts out: the rule, then the block's own lines.
        // A block with no content of its own is just the rule.
        let sep_line = *boundaries.get(idx.checked_sub(1)?)?;
        let last_line = match boundaries.get(idx) {
            Some(&next) => next.saturating_sub(1),
            None => line_count.saturating_sub(1),
        };
        if last_line < sep_line {
            return None;
        }

        let rule = &self.text[index.line_start(sep_line)..index.line_end(sep_line)];
        let content = if last_line > sep_line {
            &self.text[line_start(sep_line + 1)..index.line_end(last_line)]
        } else {
            ""
        };
        // Take the run's trailing newline with it, or — at the very end of the
        // document, where there is none — the one in front of it.
        let (cut_at, cut_end) = if last_line + 1 < line_count {
            (index.line_start(sep_line), line_start(last_line + 1))
        } else {
            (index.line_start(sep_line).saturating_sub(1), self.text.len())
        };
        let cut_text = self.text.get(cut_at..cut_end)?.to_string();
        // `before < idx`, so the paste point sits above the cut and the cut
        // does not move it — except between two adjacent rules, where the empty
        // block between them "starts" on the very line being cut. Pasting back
        // exactly where the cut was made is the honest answer there: two empty
        // notes swapping places is a no-op.
        let insert_at = line_start(block_first_line(before)).min(cut_at);

        // Put the note back with the rule *under* it, so it lands above the
        // block it was moved in front of: the same lines in a different order.
        let body = if last_line > sep_line {
            format!("{content}\n{rule}")
        } else {
            rule.to_string()
        };
        // A run needs a newline on whichever side it has a neighbour, and the
        // answer is a question about *lines*, not offsets: the last line of a
        // document starts at its very end, and a run pasted in front of that
        // line still has something below it.
        let remaining = self.text.len() - cut_text.len();
        let lines_left = line_count - (last_line - sep_line + 1);
        // Whether the run needed a newline in *front* of it, which is the only
        // thing that puts the note itself past the paste point. Reading that
        // back off the assembled string instead was wrong for any note whose
        // own first line is blank, and left the caret a line into it.
        let leading_newline;
        let insert_text = if block_first_line(before) < lines_left {
            leading_newline = false;
            format!("{body}\n")
        } else if remaining == 0 {
            leading_newline = false;
            body
        } else {
            leading_newline = true;
            format!("\n{body}")
        };

        // Where the note itself starts, which is past the newline when the
        // paste needed one in front of it. This is what the caret is put on.
        let moved_to = insert_at + usize::from(leading_newline);
        Some(BlockMove {
            cut_at,
            cut_text,
            insert_at,
            insert_text,
            moved_to,
        })
    }

    /// Insert a new empty block at the very top. Returns the byte offset where
    /// the cursor should be placed (inside the new empty block, i.e. 0).
    /// If the document is entirely empty, this is a no-op returning 0 —
    /// do not create a stray separator in an empty document.
    pub fn new_block_at_top(&mut self) -> usize {
        if self.text.is_empty() {
            return 0;
        }
        // Prepend the new note and its rule, rather than taking the document
        // apart and putting it back. Rebuilding normalises every separator it
        // passes — a `***` you typed becomes `---` — which is a document-wide
        // rewrite nobody asked for, and it lands in undo as one enormous step.
        self.text.insert_str(0, NEW_BLOCK_PREFIX);
        0
    }

    /// Byte offset at the start of the block containing `offset`.
    pub fn block_start_offset(&self, offset: usize) -> usize {
        let idx = self.block_index_at(offset);
        self.blocks().get(idx).map_or(0, |b| b.start)
    }

    /// `(block index, block start offset)` for `offset`, without building the
    /// block list.
    ///
    /// [`Note::block_index_at`] and [`Note::block_start_offset`] each rebuild
    /// every block — about 23 ms on a 9 MB note — so asking for both costs two
    /// full scans. This answers both from a [`LineIndex`]: one backward scan to
    /// the enclosing separator (bounded by the note's own length), then one
    /// allocation-free separator count over the prefix.
    ///
    /// Semantics are identical to `(block_index_at(offset),
    /// block_start_offset(offset))`, including the tie-break that a separator
    /// line belongs to the block *beneath* it.
    pub fn block_at(&self, index: &LineIndex, offset: usize) -> (usize, usize) {
        let line = index.line_at(offset);
        let boundaries = boundary_lines(&self.text);
        let block_index = boundaries.iter().take_while(|&&b| b <= line).count();
        let first_line = match block_index.checked_sub(1) {
            Some(previous) => boundaries[previous] + 1,
            None => 0,
        };
        let start = index.line_start(first_line).min(self.text.len());
        (block_index, start)
    }

}

pub fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn ceil_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Everything the app persists — the note, the settings, the backups — hangs
/// off this one directory, which is the whole reason the development build can
/// be kept away from the real note by changing a single string.
fn app_support_dir() -> PathBuf {
    let home = crate::sandbox::home();
    let name = if crate::dev::is_dev() {
        "gravitynote-gpui-dev"
    } else {
        "gravitynote-gpui"
    };
    home.join("Library/Application Support").join(name)
}

/// Local persistence path: `~/Library/Application Support/gravitynote-gpui/note.md`
pub fn default_note_path() -> PathBuf {
    app_support_dir().join("note.md")
}

/// Outcome of loading a note from disk.
#[derive(Debug)]
pub enum LoadOutcome {
    /// File missing — caller may show welcome text.
    Missing,
    /// Loaded successfully.
    Loaded(Note),
    /// File present but not valid UTF-8; content is lossily decoded.
    Lossy(Note),
    /// Other I/O error; do not overwrite the file until the user edits.
    IoError { message: String },
}

pub fn load_note(path: &Path) -> LoadOutcome {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => LoadOutcome::Loaded(Note::from_text(text)),
            Err(err) => {
                let lossy = String::from_utf8_lossy(err.as_bytes()).into_owned();
                LoadOutcome::Lossy(Note::from_text(lossy))
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoadOutcome::Missing,
        Err(err) => LoadOutcome::IoError {
            message: err.to_string(),
        },
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_note_has_one_empty_line() {
        let n = Note::new();
        assert_eq!(n.lines(), vec![""]);
        assert_eq!(n.line_count(), 1);
    }


    #[test]
    fn delete_range_removes_bytes() {
        let mut n = Note::from_text("hello world");
        n.delete_range(5, 11);
        assert_eq!(n.text(), "hello");
    }

    #[test]
    fn replace_range_edits_middle() {
        let mut n = Note::from_text("hello world");
        n.replace_range(6, 11, "rust");
        assert_eq!(n.text(), "hello rust");
    }






    #[test]
    fn line_index_at_boundaries() {
        let n = Note::from_text("aa\nbb\ncc");
        assert_eq!(n.line_index_at(0), 0);
        assert_eq!(n.line_index_at(1), 0);
        assert_eq!(n.line_index_at(2), 0);
        assert_eq!(n.line_index_at(3), 1);
        assert_eq!(n.line_index_at(5), 1);
        assert_eq!(n.line_index_at(6), 2);
        assert_eq!(n.line_index_at(n.len()), 2);
    }



    #[test]
    fn unicode_line_index_and_ranges() {
        let n = Note::from_text("α\nββ\nγ");
        // α is 2 bytes, then \n, then ββ (4 bytes), \n, γ (2 bytes)
        let (s0, e0) = n.line_byte_range(0).unwrap();
        assert_eq!(&n.text()[s0..e0], "α");
        let (s1, e1) = n.line_byte_range(1).unwrap();
        assert_eq!(&n.text()[s1..e1], "ββ");
        assert_eq!(n.line_index_at(s1 + 1), 1); // mid first β → still line 1 after clamp conceptually
        assert_eq!(n.line_index_at(e1), 1); // on newline after ββ
    }

    #[test]
    fn clamp_offset_and_replace_mid_char() {
        let mut n = Note::from_text("αβ");
        // mid-byte of α (offset 1) should clamp for replace
        n.replace_range(1, 1, "X");
        // floor start, ceil end → start=0 end=0 after clamp of empty?
        // start floor(1)=0, end ceil(1)=2 (end of α), so replaces α with X
        assert_eq!(n.text(), "Xβ");
    }

    #[test]
    fn clamp_offset_mid_multibyte() {
        let n = Note::from_text("αβ");
        assert_eq!(n.clamp_offset(1), 0); // inside α
        assert_eq!(n.clamp_offset(2), 2); // boundary between α and β
        assert_eq!(n.clamp_offset(3), 2); // inside β → floor to 2
        assert_eq!(n.clamp_offset(4), 4);
    }

    #[test]
    fn load_missing_is_missing() {
        let path = std::env::temp_dir().join(format!(
            "gravitynote-gpui-missing-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(load_note(&path), LoadOutcome::Missing));
    }


    // ---- separator detection --------------------------------------------

    #[test]
    fn separator_lines_recognised() {
        for line in [
            "---",
            "----",
            "***",
            "___",
            "- - -",
            "* * * *",
            "_ _ _",
            "  ---  ",
            "\t---\t",
            "-- -",
            "---\r", // CRLF leftover: trailing \r is ASCII whitespace
            "          ---",
        ] {
            assert!(is_separator_line(line), "expected separator: {line:?}");
        }
    }

    #[test]
    fn non_separator_lines_rejected() {
        for line in [
            "", "   ", "\t", "--", "**", "__", "-", "-*-", "---a", "a---", "- - ", "===", " -_-",
            "text", "—-—", "###",
        ] {
            assert!(!is_separator_line(line), "expected non-separator: {line:?}");
        }
    }

    // ---- block invariants -------------------------------------------------

    /// Every structural guarantee `blocks()` must uphold.
    fn assert_block_invariants(text: &str) {
        let n = Note::from_text(text);
        let blocks = n.blocks();
        assert!(!blocks.is_empty(), "blocks() empty for {text:?}");
        assert_eq!(blocks.len(), n.blocks().len(), "block_count for {text:?}");
        assert_eq!(
            blocks.len(),
            n.separator_line_indices().len() + 1,
            "blocks == separators + 1 for {text:?}"
        );
        let mut prev_end = 0usize;
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.index, i, "index for {text:?}");
            assert!(b.start <= b.end, "start<=end for {text:?} block {i}");
            assert!(b.end <= n.len(), "end<=len for {text:?} block {i}");
            assert!(
                n.text().is_char_boundary(b.start) && n.text().is_char_boundary(b.end),
                "char boundaries for {text:?} block {i}"
            );
            assert!(
                b.start >= prev_end,
                "ascending/non-overlapping for {text:?} block {i}"
            );
            prev_end = b.end;
            // Slicing must not panic and must equal the joined content lines.
            let slice = &n.text()[b.start..b.end];
            if b.start < b.end {
                let lines = n.lines();
                let expected = lines[b.first_line..=b.last_line].join("\n");
                assert_eq!(slice, expected, "content for {text:?} block {i}");
                for line in &lines[b.first_line..=b.last_line] {
                    assert!(
                        !is_separator_line(line),
                        "block content contains a separator for {text:?}"
                    );
                }
            }
            assert_eq!(
                b.sep_line.is_none(),
                i == 0,
                "sep_line presence for {text:?} block {i}"
            );
            if let Some(s) = b.sep_line {
                assert!(
                    is_separator_line(n.lines()[s]),
                    "sep_line points at a separator"
                );
                assert_eq!(n.separator_line_indices()[i - 1], s);
            }
        }
        // Every byte offset maps to an in-range block index.
        for off in 0..=n.len() {
            let idx = n.block_index_at(off);
            assert!(idx < blocks.len(), "block_index_at({off}) for {text:?}");
        }
    }

    const FIXTURES: &[&str] = &[
        "",
        "one line",
        "a\nb\nc",
        "---",
        "---\n---",
        "---\nafter",
        "before\n---",
        "a\n---\nb",
        "a\n---\n---\nb",
        "a\n---\n\n---\nb",
        // A note whose own first line is blank: `moved_to` used to point at that
        // blank line's newline rather than at the note.
        "a\n---\n\nhello\n---\nb",
        "a\n---\nb\n---\n\nc",
        "  ---  \nbody",
        "α\n---\nβγ\n---\nδ",
        "a\n***\nb\n___\nc",
        "\n",
        "\n---\n",
        "多行\n内容\n---\n第二块",
    ];

    #[test]
    fn block_invariants_hold_for_fixtures() {
        for t in FIXTURES {
            assert_block_invariants(t);
        }
    }

    #[test]
    fn empty_document_is_one_empty_block() {
        let n = Note::new();
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start, 0);
        assert_eq!(blocks[0].end, 0);
        assert_eq!(blocks[0].sep_line, None);
        assert_eq!(blocks[0].first_line, 0);
        assert_eq!(blocks[0].last_line, 0);
        assert_eq!(n.blocks().len(), 1);
        assert_eq!(n.block_index_at(0), 0);
        assert_eq!(n.block_start_offset(0), 0);
    }

    #[test]
    fn document_without_separators_is_one_block() {
        let text = "title\nbody line\n\nmore";
        let n = Note::from_text(text);
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!((blocks[0].start, blocks[0].end), (0, text.len()));
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], text);
        assert_eq!(blocks[0].first_line, 0);
        assert_eq!(blocks[0].last_line, n.line_count() - 1);
    }

    #[test]
    fn multi_block_offsets_and_contents() {
        let n = Note::from_text("alpha\nline2\n---\nbeta\n---\ngamma");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "alpha\nline2");
        assert_eq!(&n.text()[blocks[1].start..blocks[1].end], "beta");
        assert_eq!(&n.text()[blocks[2].start..blocks[2].end], "gamma");
        assert_eq!(blocks[0].sep_line, None);
        assert_eq!(blocks[1].sep_line, Some(2));
        assert_eq!(blocks[2].sep_line, Some(4));
        assert_eq!((blocks[1].first_line, blocks[1].last_line), (3, 3));
        assert_eq!(n.separator_line_indices(), vec![2, 4]);
    }

    #[test]
    fn document_that_is_only_a_separator() {
        let n = Note::from_text("---");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!((blocks[0].start, blocks[0].end), (0, 0));
        assert_eq!((blocks[1].start, blocks[1].end), (3, 3));
        assert_eq!(blocks[0].sep_line, None);
        assert_eq!(blocks[1].sep_line, Some(0));
        // The separator belongs to the block under it.
        assert_eq!(n.block_index_at(0), 1);
        assert_eq!(n.block_index_at(3), 1);
    }

    #[test]
    fn leading_separator_makes_empty_first_block() {
        let n = Note::from_text("---\nbody");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!((blocks[0].start, blocks[0].end), (0, 0));
        assert_eq!(&n.text()[blocks[1].start..blocks[1].end], "body");
        assert_eq!(blocks[1].sep_line, Some(0));
        assert_eq!(blocks[1].first_line, 1);
    }

    #[test]
    fn trailing_separator_makes_empty_last_block() {
        let n = Note::from_text("body\n---");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "body");
        assert_eq!((blocks[1].start, blocks[1].end), (n.len(), n.len()));
        assert_eq!(blocks[1].sep_line, Some(1));
        // Empty trailing block: first_line is one past the last line.
        assert_eq!(blocks[1].first_line, n.line_count());
    }

    #[test]
    fn adjacent_separators_make_an_empty_middle_block() {
        let n = Note::from_text("a\n---\n---\nb");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "a");
        assert_eq!(blocks[1].start, blocks[1].end);
        assert_eq!(blocks[1].sep_line, Some(1));
        assert_eq!(&n.text()[blocks[2].start..blocks[2].end], "b");
        assert_eq!(blocks[2].sep_line, Some(2));
        assert_eq!(n.separator_line_indices(), vec![1, 2]);
    }

    #[test]
    fn separator_with_surrounding_whitespace_splits() {
        let n = Note::from_text("a\n   ---   \nb");
        assert_eq!(n.blocks().len(), 2);
        let blocks = n.blocks();
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "a");
        assert_eq!(&n.text()[blocks[1].start..blocks[1].end], "b");
    }

    #[test]
    fn whitespace_only_line_is_not_a_separator() {
        let n = Note::from_text("a\n   \nb");
        assert_eq!(n.blocks().len(), 1);
        assert_eq!(n.blocks()[0].end, n.len());
    }

    #[test]
    fn alternate_separator_glyphs_split_blocks() {
        let n = Note::from_text("a\n***\nb\n___\nc\n- - -\nd");
        assert_eq!(n.blocks().len(), 4);
        assert_eq!(n.separator_line_indices(), vec![1, 3, 5]);
    }

    #[test]
    fn unicode_block_offsets() {
        let n = Note::from_text("αβ\n---\nγδε\n---\nζ");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "αβ");
        assert_eq!(&n.text()[blocks[1].start..blocks[1].end], "γδε");
        assert_eq!(&n.text()[blocks[2].start..blocks[2].end], "ζ");
        assert_eq!(blocks[0].start, 0);
        assert_eq!(blocks[0].end, 4); // 2 chars × 2 bytes
        assert_eq!(blocks[1].start, 4 + 1 + 3 + 1); // "\n" + "---" + "\n"
    }

    #[test]
    fn block_index_at_separator_returns_following_block() {
        let n = Note::from_text("a\n---\nb");
        // line 0 = "a" (0..1), line 1 = "---" (2..5), line 2 = "b" (6..7)
        assert_eq!(n.block_index_at(0), 0);
        assert_eq!(n.block_index_at(1), 0); // newline of line 0
        assert_eq!(n.block_index_at(2), 1); // start of separator
        assert_eq!(n.block_index_at(4), 1);
        assert_eq!(n.block_index_at(5), 1); // newline of the separator
        assert_eq!(n.block_index_at(6), 1);
        assert_eq!(n.block_index_at(7), 1);
        assert_eq!(n.block_index_at(9999), 1); // clamped
    }

    #[test]
    fn block_index_at_every_offset_of_fixture() {
        let n = Note::from_text("one\n---\ntwo\nlines\n---\nthree");
        let count = n.blocks().len();
        let expected_at_end = count - 1;
        for off in 0..=n.len() {
            assert!(n.block_index_at(off) < count);
        }
        assert_eq!(n.block_index_at(n.len()), expected_at_end);
        assert_eq!(n.block_index_at(0), 0);
    }

    #[test]
    fn block_start_offset_returns_containing_block_start() {
        let n = Note::from_text("aaa\n---\nbbb\nccc");
        let blocks = n.blocks();
        assert_eq!(n.block_start_offset(1), blocks[0].start);
        assert_eq!(n.block_start_offset(4), blocks[1].start); // on the separator
        assert_eq!(n.block_start_offset(n.len()), blocks[1].start);
    }

    // ---- block moves ------------------------------------------------------

    /// The moves take the caller's line index, which every caller already has.
    fn promote(n: &mut Note, idx: usize) -> Option<usize> {
        let index = LineIndex::new(n.text());
        let planned = n.plan_bring_block_up(&index, idx)?;
        n.apply_move(&planned);
        Some(planned.moved_to)
    }

    fn nudge(n: &mut Note, idx: usize) -> Option<usize> {
        let index = LineIndex::new(n.text());
        let planned = n.plan_move_block_up_one(&index, idx)?;
        n.apply_move(&planned);
        Some(planned.moved_to)
    }

    /// Every block's content, in document order.
    fn contents(n: &Note) -> Vec<String> {
        n.blocks()
            .iter()
            .map(|b| n.text()[b.start..b.end].to_string())
            .collect()
    }

    #[test]
    fn bring_block_up_moves_block_to_top() {
        let mut n = Note::from_text("one\n---\ntwo\n---\nthree");
        assert_eq!(promote(&mut n, 2), Some(0));
        assert_eq!(n.text(), "three\n---\none\n---\ntwo");
        assert_eq!(n.blocks().len(), 3);
    }

    #[test]
    fn bring_block_up_preserves_multiline_content() {
        let mut n = Note::from_text("a1\na2\n---\nb1\nb2\nb3\n---\nc");
        assert_eq!(promote(&mut n, 1), Some(0));
        assert_eq!(n.text(), "b1\nb2\nb3\n---\na1\na2\n---\nc");
        let blocks = n.blocks();
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "b1\nb2\nb3");
    }

    #[test]
    fn bring_block_up_noop_on_first_or_oob() {
        let mut n = Note::from_text("a\n---\nb");
        assert_eq!(promote(&mut n, 0), None);
        assert_eq!(n.text(), "a\n---\nb");
        assert_eq!(promote(&mut n, 2), None);
        assert_eq!(promote(&mut n, 99), None);
        assert_eq!(n.text(), "a\n---\nb");
    }

    /// Moving one note must not rewrite the separators of the notes it passes.
    /// Rebuilding the document did: a `***` you typed came back as `---`.
    #[test]
    fn bring_block_up_leaves_separator_glyphs_alone() {
        let mut n = Note::from_text("a\n***\nb\n___\nc");
        assert_eq!(promote(&mut n, 2), Some(0));
        assert_eq!(n.text(), "c\n___\na\n***\nb");
    }

    #[test]
    fn bring_block_up_with_unicode() {
        let mut n = Note::from_text("α\n---\nββ\n---\nγ");
        assert_eq!(promote(&mut n, 2), Some(0));
        assert_eq!(n.text(), "γ\n---\nα\n---\nββ");
        let blocks = n.blocks();
        assert_eq!(&n.text()[blocks[0].start..blocks[0].end], "γ");
        assert_eq!(&n.text()[blocks[2].start..blocks[2].end], "ββ");
    }

    #[test]
    fn bring_block_up_keeps_empty_blocks() {
        let mut n = Note::from_text("a\n---\n---\nb");
        // Move the empty middle block to the top; it stays an empty block, and
        // the rule it travelled with is now the document's first line.
        assert_eq!(promote(&mut n, 1), Some(0));
        assert_eq!(n.text(), "---\na\n---\nb");
        assert_eq!(n.blocks().len(), 3);
        assert_eq!(n.blocks()[0].start, n.blocks()[0].end);

        // Moving the last block up keeps the empty block, now trailing.
        let mut n = Note::from_text("a\n---\n---\nb");
        assert_eq!(promote(&mut n, 2), Some(0));
        assert_eq!(n.text(), "b\n---\na\n---");
        assert_eq!(n.blocks().len(), 3);
        let blocks = n.blocks();
        assert_eq!((blocks[2].start, blocks[2].end), (n.len(), n.len()));
    }

    #[test]
    fn move_block_up_one_swaps_with_previous() {
        let mut n = Note::from_text("A\n---\nB\n---\nC");
        let off = nudge(&mut n, 2).unwrap();
        assert_eq!(n.text(), "A\n---\nC\n---\nB");
        assert_eq!(off, 6); // "A\n---\n"
        assert_eq!(&n.text()[off..off + 1], "C");
        assert_eq!(n.blocks()[1].start, off);
    }

    #[test]
    fn move_block_up_one_to_the_top() {
        let mut n = Note::from_text("A\n---\nB");
        assert_eq!(nudge(&mut n, 1), Some(0));
        assert_eq!(n.text(), "B\n---\nA");
    }

    #[test]
    fn move_block_up_one_noop_on_first_or_oob() {
        let mut n = Note::from_text("A\n---\nB");
        assert_eq!(nudge(&mut n, 0), None);
        assert_eq!(nudge(&mut n, 2), None);
        assert_eq!(nudge(&mut n, 42), None);
        assert_eq!(n.text(), "A\n---\nB");
    }

    #[test]
    fn move_block_up_one_offset_with_unicode() {
        let mut n = Note::from_text("αα\n---\nβ\n---\nγ");
        let off = nudge(&mut n, 2).unwrap();
        assert_eq!(n.text(), "αα\n---\nγ\n---\nβ");
        assert_eq!(&n.text()[off..off + "γ".len()], "γ");
    }

    /// A move has two contracts, and both are checked here on every fixture.
    ///
    /// 1. **It reports the two edits it made.** Applying the cut and then the
    ///    insertion to the old text reproduces the new text exactly — the app
    ///    replays them into its line index, its fence map and its undo history,
    ///    and the failure mode of getting that wrong is silent corruption.
    /// 2. **It only reorders notes.** The blocks afterwards are the blocks
    ///    before, permuted — no note's content is altered, gained or lost.
    #[test]
    fn a_move_reports_the_two_edits_it_made() {
        for text in FIXTURES {
            let base = Note::from_text(*text);
            let count = base.blocks().len();
            for idx in 0..count + 1 {
                for to_top in [true, false] {
                    let mut n = base.clone();
                    let index = LineIndex::new(n.text());
                    let before = n.text().to_string();
                    let was = contents(&n);
                    let Some(moved) = (if to_top {
                        n.plan_bring_block_up(&index, idx)
                    } else {
                        n.plan_move_block_up_one(&index, idx)
                    }) else {
                        assert_eq!(n.text(), before, "a refused move changed the text");
                        continue;
                    };
                    n.apply_move(&moved);
                    let mut replayed = before.clone();
                    assert_eq!(
                        replayed.get(moved.cut_at..moved.cut_at + moved.cut_text.len()),
                        Some(moved.cut_text.as_str()),
                        "the cut says it took bytes that were not there ({text:?}, block {idx})"
                    );
                    replayed.replace_range(moved.cut_at..moved.cut_at + moved.cut_text.len(), "");
                    replayed.insert_str(moved.insert_at, &moved.insert_text);
                    assert_eq!(
                        replayed,
                        n.text(),
                        "replaying the reported edits did not reproduce the move \
                         ({text:?}, block {idx}, to_top {to_top})"
                    );

                    // The caret is put on `moved_to`, so it has to be the first
                    // byte of the note that moved — not the newline in front of
                    // it, which is a rule line the caret would land on instead.
                    assert!(
                        n.text().is_char_boundary(moved.moved_to),
                        "moved_to split a character ({text:?}, block {idx})"
                    );
                    assert!(
                        n.text()[moved.moved_to..].starts_with(was[idx].as_str()),
                        "moved_to points at {:?}, not at the note that moved ({:?}) \
                         ({text:?}, block {idx}, to_top {to_top})",
                        &n.text()[moved.moved_to..],
                        was[idx]
                    );

                    let (mut sorted_was, mut sorted_now) = (was.clone(), contents(&n));
                    sorted_was.sort();
                    sorted_now.sort();
                    assert_eq!(
                        sorted_was, sorted_now,
                        "a move changed what the notes say ({text:?}, block {idx}, \
                         to_top {to_top}): {was:?} became {:?}",
                        contents(&n)
                    );
                }
            }
        }
    }

    #[test]
    fn new_block_at_top_on_empty_doc_is_noop() {
        let mut n = Note::new();
        assert_eq!(n.new_block_at_top(), 0);
        assert_eq!(n.text(), "");
        assert_eq!(n.blocks().len(), 1);
    }

    #[test]
    fn new_block_at_top_prepends_empty_block() {
        let mut n = Note::from_text("existing\nnote");
        assert_eq!(n.new_block_at_top(), 0);
        assert_eq!(n.text(), "\n---\nexisting\nnote");
        let blocks = n.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!((blocks[0].start, blocks[0].end), (0, 0));
        assert_eq!(&n.text()[blocks[1].start..blocks[1].end], "existing\nnote");
        assert_eq!(n.block_index_at(0), 0);
    }


    #[test]
    fn new_block_at_top_on_multi_block_doc() {
        let mut n = Note::from_text("a\n---\nb");
        assert_eq!(n.new_block_at_top(), 0);
        assert_eq!(n.text(), "\n---\na\n---\nb");
        assert_eq!(n.blocks().len(), 3);
    }

    /// Starting a new note must not rewrite the rest of the document. A `***`
    /// is a separator the user typed, and it stays one.
    #[test]
    fn new_block_at_top_leaves_the_separators_below_it_alone() {
        for text in ["a\n***\nb", "a\n___\nb", "a\n- - -\nb", "a\n  ---  \nb"] {
            let mut n = Note::from_text(text);
            n.new_block_at_top();
            assert_eq!(
                n.text(),
                format!("\n---\n{text}"),
                "starting a note rewrote {text:?}"
            );
            assert_eq!(n.blocks().len(), 3);
        }
    }

    /// The app applies a move as two edits and *splices* its line index and
    /// fence map between them rather than rebuilding either. That is the same
    /// arrangement `index` and `fences` have their own differential tests for,
    /// so the move's two halves get one too: patch, then compare against a
    /// rebuild. Getting this wrong corrupts a document silently.
    #[test]
    fn a_move_can_be_spliced_into_the_derived_state() {
        for text in FIXTURES {
            let base = Note::from_text(*text);
            for idx in 0..base.blocks().len() + 1 {
                for to_top in [true, false] {
                    let mut n = base.clone();
                    let mut index = LineIndex::new(n.text());
                    let mut fences = FenceMap::new(n.text(), &index);
                    let Some(planned) = (if to_top {
                        n.plan_bring_block_up(&index, idx)
                    } else {
                        n.plan_move_block_up_one(&index, idx)
                    }) else {
                        continue;
                    };

                    // Exactly what `NoteApp::commit_block_move` does.
                    let splice = |n: &Note,
                                  index: &mut LineIndex,
                                  fences: &mut FenceMap,
                                  at: usize,
                                  removed: &str,
                                  inserted: &str| {
                        let first_line = index.line_at(at);
                        let removed_lines = removed.matches('\n').count();
                        let inserted_lines = inserted.matches('\n').count();
                        index.splice(n.text(), at, removed, inserted);
                        fences.splice(n.text(), index, first_line, removed_lines, inserted_lines);
                    };

                    let cut_end = planned.cut_at + planned.cut_text.len();
                    n.replace_range(planned.cut_at, cut_end, "");
                    splice(
                        &n,
                        &mut index,
                        &mut fences,
                        planned.cut_at,
                        &planned.cut_text,
                        "",
                    );
                    n.replace_range(planned.insert_at, planned.insert_at, &planned.insert_text);
                    splice(
                        &n,
                        &mut index,
                        &mut fences,
                        planned.insert_at,
                        "",
                        &planned.insert_text,
                    );

                    let rebuilt = LineIndex::new(n.text());
                    assert_eq!(
                        index.line_count(),
                        rebuilt.line_count(),
                        "spliced line count is wrong after moving block {idx} of {text:?}"
                    );
                    for line in 0..rebuilt.line_count() {
                        assert_eq!(
                            index.line_range(line),
                            rebuilt.line_range(line),
                            "spliced line {line} is wrong after moving block {idx} of {text:?}"
                        );
                    }
                    let rebuilt_fences = FenceMap::new(n.text(), &rebuilt);
                    for line in 0..rebuilt.line_count() {
                        assert_eq!(
                            fences.is_open(line),
                            rebuilt_fences.is_open(line),
                            "spliced fence state for line {line} is wrong after moving \
                             block {idx} of {text:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn moves_keep_block_invariants() {
        for t in FIXTURES {
            let base = Note::from_text(*t);
            for idx in 0..base.blocks().len() + 1 {
                let mut a = base.clone();
                let ai = LineIndex::new(a.text());
                if let Some(m) = a.plan_bring_block_up(&ai, idx) {
                    a.apply_move(&m);
                }
                assert_block_invariants(a.text());
                let mut b = base.clone();
                let bi = LineIndex::new(b.text());
                if let Some(m) = b.plan_move_block_up_one(&bi, idx) {
                    b.apply_move(&m);
                }
                assert_block_invariants(b.text());
                let mut c = base.clone();
                c.new_block_at_top();
                assert_block_invariants(c.text());
            }
        }
    }

    #[test]
    fn round_trip_rebuild_is_stable_for_canonical_documents() {
        let n = Note::from_text("a\n---\nb\n---\nc");
        let mut m = n.clone();
        // Moving a block up and back down again returns the original text.
        assert_eq!(nudge(&mut m, 1), Some(0));
        assert_eq!(nudge(&mut m, 1), Some(0));
        assert_eq!(m.text(), n.text());
    }

    // ---- persistence paths / migration -----------------------------------





}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn the_note_is_a_markdown_file_in_application_support() {
        let path = default_note_path();
        assert_eq!(path.file_name().unwrap(), "note.md");
        assert!(
            path.to_string_lossy()
                .contains("Application Support/gravitynote-gpui"),
            "unexpected location: {path:?}"
        );
    }

    #[test]
    fn typing_after_a_new_block_lands_inside_it() {
        let mut note = Note::from_text("existing");
        let caret = note.new_block_at_top();
        note.replace_range(caret, caret, "fresh");
        assert_eq!(note.text(), "fresh\n---\nexisting");
        assert_eq!(note.blocks().len(), 2);
        assert_eq!(note.block_index_at(caret), 0);
    }
}

#[cfg(test)]
mod block_at_tests {
    use super::*;

    /// `block_at` is an optimisation of two existing calls; it must agree with
    /// them at every byte offset, not just the easy ones.
    fn assert_agrees(text: &str) {
        let note = Note::from_text(text);
        let index = LineIndex::new(note.text());
        for offset in 0..=note.len() {
            if !note.text().is_char_boundary(offset) {
                continue;
            }
            let (fast_idx, fast_start) = note.block_at(&index, offset);
            assert_eq!(
                fast_idx,
                note.block_index_at(offset),
                "block index at {offset} in {text:?}"
            );
            assert_eq!(
                fast_start,
                note.block_start_offset(offset),
                "block start at {offset} in {text:?}"
            );
        }
    }

    #[test]
    fn agrees_with_the_reference_on_fixtures() {
        for text in [
            "",
            "a",
            "one\ntwo\nthree",
            "---",
            "---\n",
            "\n---\n",
            "a\n---\nb",
            "a\n---\n---\nb",
            "---\na",
            "a\n---",
            "a\n---\nb\n---\nc",
            "# h\n\nbody\n---\n# h2\n\nbody2\n",
            "α\n---\nββ\n---\nγ",
            "🎉 party\n---\n日本語\n---\ncafé",
            "a\n***\nb\n___\nc",
            "  ---  \nindented separator",
            "a\n\n\n---\n\n\nb",
        ] {
            assert_agrees(text);
        }
    }

    #[test]
    fn agrees_with_the_reference_on_a_generated_corpus() {
        let text = crate::corpus::generate(crate::corpus::CorpusSpec::with_notes(400));
        let note = Note::from_text(text);
        let index = LineIndex::new(note.text());
        // Every offset would be ~100k reference scans; sample densely instead,
        // including every block boundary and both sides of every separator.
        let mut offsets: Vec<usize> = note
            .blocks()
            .iter()
            .flat_map(|b| [b.start, b.end, b.start + 1])
            .collect();
        // Both sides of every separator line — the tie-break lives there.
        for line in note.separator_line_indices() {
            offsets.push(index.line_start(line));
            offsets.push(index.line_end(line));
            offsets.push(index.line_end(line) + 1);
        }
        offsets.retain(|&o| o <= note.len() && note.text().is_char_boundary(o));
        offsets.sort_unstable();
        offsets.dedup();

        for offset in offsets {
            assert_eq!(
                note.block_at(&index, offset).0,
                note.block_index_at(offset),
                "block index at {offset}"
            );
            assert_eq!(
                note.block_at(&index, offset).1,
                note.block_start_offset(offset),
                "block start at {offset}"
            );
        }
    }
}

#[cfg(test)]
mod separator_delete_tests {
    use super::*;
    use crate::index::LineIndex;

    fn at(text: &str, offset: usize) -> (LineIndex, usize) {
        (LineIndex::new(text), offset)
    }

    fn fences_for(text: &str, index: &LineIndex) -> FenceMap {
        FenceMap::new(text, index)
    }

    #[test]
    fn backspace_at_a_notes_start_takes_the_whole_rule() {
        let text = "first note\n---\nsecond note";
        let (index, caret) = at(text, 15); // start of "second note"
        let fences = fences_for(text, &index);
        let rule = separator_before(text, &index, &fences, caret).expect("a rule above");
        let mut merged = text.to_string();
        merged.replace_range(rule, "");
        assert_eq!(merged, "first note\nsecond note", "no stray dashes");
    }

    #[test]
    fn forward_delete_at_a_notes_end_takes_the_whole_rule() {
        let text = "first note\n---\nsecond note";
        let (index, caret) = at(text, 10); // end of "first note"
        let fences = fences_for(text, &index);
        let rule = separator_after(text, &index, &fences, caret).expect("a rule below");
        let mut merged = text.to_string();
        merged.replace_range(rule, "");
        assert_eq!(merged, "first note\nsecond note");
    }

    #[test]
    fn there_is_no_rule_to_take_in_the_middle_of_a_line() {
        let text = "first note\n---\nsecond note";
        let index = LineIndex::new(text);
        let fences = fences_for(text, &index);
        assert_eq!(separator_before(text, &index, &fences, 18), None);
        assert_eq!(separator_after(text, &index, &fences, 5), None);
        // Nor at the very start or end of the document.
        assert_eq!(separator_before(text, &index, &fences, 0), None);
        assert_eq!(separator_after(text, &index, &fences, text.len()), None);
    }

    /// ⌥⌫ at the head of a note used to scan past the rule and eat the last
    /// word of the note above.
    #[test]
    fn a_word_delete_cannot_reach_back_through_a_rule() {
        let text = "first note\n---\nsecond note";
        let index = LineIndex::new(text);
        let fences = fences_for(text, &index);
        let caret = 22; // inside "second note", after "second"
        // A scan that reached back to offset 6 must stop at the note's start.
        assert_eq!(clamp_within_note(text, &index, &fences, 6, caret), 15);
        // Within one note it changes nothing.
        assert_eq!(clamp_within_note(text, &index, &fences, 15, caret), 15);
    }

    /// ⌥⌦ at the end of a note used to scan *forward* past the rule and eat the
    /// first word of the note below, merging the two.
    #[test]
    fn a_word_delete_cannot_reach_forward_through_a_rule() {
        let text = "first note\n---\nsecond note";
        let index = LineIndex::new(text);
        let fences = fences_for(text, &index);
        let caret = 4; // inside "first note", before " note"
        // A forward scan reaching to 22 (into "second note") must stop at the
        // end of the first note's content, offset 10.
        assert_eq!(clamp_within_note_forward(text, &index, &fences, caret, 22), 10);
        // Within one note it changes nothing.
        assert_eq!(clamp_within_note_forward(text, &index, &fences, caret, 10), 10);
    }

    /// A `---` inside a code fence is code. Deleting next to it must not take
    /// the whole line as though it were a rule dividing two notes.
    #[test]
    fn a_rule_inside_a_fence_is_not_a_note_edge() {
        let text = "note\n\n```\n---\nbody\n```\n";
        let index = LineIndex::new(text);
        let fences = fences_for(text, &index);
        assert_eq!(boundary_lines(text), Vec::<usize>::new(), "no rules here");
        let body = text.find("body").unwrap();
        assert_eq!(separator_before(text, &index, &fences, body), None);
        let fence_open_end = text.find("\n---").unwrap();
        assert_eq!(separator_after(text, &index, &fences, fence_open_end), None);
        // And a word delete inside the block is not clamped by it.
        assert_eq!(clamp_within_note(text, &index, &fences, 6, body + 4), 6);
    }
}

#[cfg(test)]
mod blank_line_tests {
    use super::*;

    /// Blank lines are punctuation inside a note, never a split. A note is
    /// allowed to breathe.
    #[test]
    fn no_number_of_blank_lines_separates_notes() {
        for text in [
            "a\n\nb",
            "a\n\n\nb",
            "a\n\n\n\nb",
            "a\n\n\n\n\n\n\n\nb",
            "  \n  \n  \n  \nx",
            "a\n\n\n\n",
            "\n\n\n\na",
        ] {
            assert_eq!(boundary_lines(text), Vec::<usize>::new(), "text {text:?}");
            assert_eq!(Note::from_text(text).blocks().len(), 1, "text {text:?}");
        }
    }

    #[test]
    fn only_a_rule_splits_a_note() {
        let note = Note::from_text("one\n\n\n\ntwo\n---\nthree");
        let blocks = note.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            &note.text()[blocks[0].start..blocks[0].end],
            "one\n\n\n\ntwo"
        );
        assert_eq!(&note.text()[blocks[1].start..blocks[1].end], "three");
    }

    #[test]
    fn blank_lines_travel_with_the_note_they_belong_to() {
        let mut note = Note::from_text("first\n\n\n\nstill first\n---\nsecond");
        assert_eq!(note.blocks().len(), 2);
        let ni = LineIndex::new(note.text());
        let planned = note.plan_bring_block_up(&ni, 1).expect("a move to make");
        note.apply_move(&planned);
        assert_eq!(note.text(), "second\n---\nfirst\n\n\n\nstill first");
    }

    #[test]
    fn block_at_agrees_with_the_reference_around_blank_lines() {
        for text in [
            "a\n\n\n\nb",
            "a\n\n\n\n\n\nb\n---\nc",
            "\n\n\n\na",
            "a\n\n\n\n",
            "  \n  \n  \n  \nx",
            "a\n\nb\n\nc",
        ] {
            let note = Note::from_text(text);
            let index = crate::index::LineIndex::new(note.text());
            for offset in 0..=note.len() {
                if !note.text().is_char_boundary(offset) {
                    continue;
                }
                assert_eq!(
                    note.block_at(&index, offset).0,
                    note.block_index_at(offset),
                    "index at {offset} in {text:?}"
                );
                assert_eq!(
                    note.block_at(&index, offset).1,
                    note.block_start_offset(offset),
                    "start at {offset} in {text:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::index::LineIndex;
    use crate::fences::FenceMap;

    /// The renderer answers "is this a boundary?" per visible row, from the line
    /// and the fence state it already has. That has to agree with the walk over
    /// the whole document — it is the only thing keeping segmentation off the
    /// frame path.
    #[test]
    fn boundaries_agree_with_the_per_line_check() {
        for text in [
            "",
            "a",
            "a\nb",
            "a\n\n\n\nb",
            "---",
            "a\n---\nb",
            "a\n***\nb",
            "  ---  ",
            "a\n---\n\n\n---\nb",
            "```\n---\n```",
            "a\n```\n---\n---\n```\nb",
            "a\n---\n```\n---\n```\n---\nb",
            "```rust\nlet x = 1;\n---\n```",
            "~~~\n---\n~~~",
            "```\n---",
            "```\n---\nunclosed",
            "a\n---\n```\nstill open",
            "```\n```\n---\n```\nopen again",
            "α\n---\nβ",
        ] {
            let index = LineIndex::new(text);
            let fences = FenceMap::new(text, &index);
            let expected = boundary_lines(text);
            let got: Vec<usize> = (0..index.line_count())
                .filter(|&i| is_boundary_line(text, &index, i, fences.in_closed_fence(i)))
                .collect();
            assert_eq!(got, expected, "text {text:?}");
        }
    }

    /// A fence that was opened and never closed is a code block being written,
    /// not one that exists. Treating it as code makes every note below merge
    /// into it the moment you type ``` — and un-merge when you close it.
    #[test]
    fn an_unclosed_fence_does_not_swallow_the_notes_below_it() {
        assert_eq!(boundary_lines("```\ncode\n---\nnote"), vec![2]);
        assert_eq!(Note::from_text("```\ncode\n---\nnote").blocks().len(), 2);
        // Closing it makes the same `---` code again.
        assert_eq!(boundary_lines("```\ncode\n---\n```"), Vec::<usize>::new());
    }

    /// A `---` inside a fenced block is code. Splitting there would cut the
    /// block in half the next time a note was moved.
    #[test]
    fn a_rule_inside_a_code_fence_is_not_a_boundary() {
        assert_eq!(boundary_lines("```\n---\n```"), Vec::<usize>::new());
        assert_eq!(Note::from_text("```\n---\n```").blocks().len(), 1);
        // ...but the same rule after the block closes still splits.
        assert_eq!(boundary_lines("```\n---\n```\n---\nx"), vec![3]);
    }

    #[test]
    fn out_of_range_lines_are_not_boundaries() {
        let text = "a\nb";
        let index = LineIndex::new(text);
        assert!(!is_boundary_line(text, &index, 99, false));
    }
}
