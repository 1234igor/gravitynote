//! Selection arithmetic: what a double-click, a triple-click, an Option+Arrow
//! or an Option+Delete actually covers.
//!
//! Pure byte-range logic over a `&str`. No GPUI, no document type, no state —
//! every function takes the whole buffer plus a byte offset and hands back a
//! `Range<usize>` (or a single offset) that the caller applies to its own
//! selection. Word boundaries come from UAX #29 via `unicode-segmentation`, so
//! `café`, `日本語`, `👍🏽` and `naïve` behave like text rather than like bytes.
//!
//! # Cost: local, never O(document)
//!
//! The buffer is one `String` and may be 10 MB. Nothing here ever splits,
//! scans or allocates over the whole document. Every function starts at
//! `offset` and walks *outward* to the nearest boundary it needs:
//!
//! | Function | Work |
//! |---|---|
//! | [`word_range`] | the whitespace-delimited chunk around `offset` |
//! | [`line_range`] | outward to the nearest `\n` in each direction |
//! | [`paragraph_range`] | the lines of the surrounding paragraph only |
//! | [`click_range`] | whichever of the above it delegates to |
//! | [`prev_word_start`] / [`next_word_end`] | the distance actually travelled |
//! | the three `delete_*_range` fns | same as the motion they wrap |
//!
//! Concretely: line bounds are found with a backwards byte scan that stops at
//! the first `\n` (`rposition`), not with `text[..offset].rfind` over a fresh
//! slice of the prefix; paragraph bounds walk line by line and stop at the
//! first blank or separator line. A click in the middle of a 10 MB document
//! costs a few hundred bytes of scanning, and `tests::scoping_is_local` pins
//! that down with a timed batch of 100 000 calls against a 5 MB buffer.
//!
//! # Robustness
//!
//! No function panics on any input. `offset` may be past the end of `text` or
//! in the middle of a multi-byte character; it is clamped to `text.len()` and
//! floored to the nearest char boundary first. Every returned offset is a char
//! boundary of `text`, is `<= text.len()`, and every returned range has
//! `start <= end`.

use std::ops::Range;

use unicode_segmentation::{UWordBoundIndices, UnicodeSegmentation};

// ---------------------------------------------------------------------------
// Word / click selection
// ---------------------------------------------------------------------------

/// Byte range of the word at `offset`.
///
/// Mac semantics: when `offset` is inside or at the edge of a word, that whole
/// word is returned. When it sits in a run of whitespace, the whitespace run is
/// returned. When it sits on punctuation, that punctuation run is returned.
/// Never panics; always returns a range on char boundaries within `text`.
///
/// # What counts as one word
///
/// Segments come from UAX #29 word boundaries, with two adjustments. Adjacent
/// segments of the same kind are merged, which matters only for scripts that
/// UAX #29 breaks per character — so a run of Han or Hiragana such as `日本語`
/// selects whole rather than one ideograph at a time — and for runs of ASCII
/// punctuation such as `://`. Emoji are never merged, so `👍🏽😀` selects one
/// emoji at a time.
///
/// The three ambiguous connectors, decided deliberately and pinned by tests:
///
/// - `_` **joins**: `snake_case` is one word. (UAX #29 ExtendNumLet; also what
///   Xcode, TextEdit and most Mac apps select.)
/// - `-` **splits**: `kebab-case` is three selections — `kebab`, `-`, `case`.
///   Double-clicking `kebab` gives you just `kebab`.
/// - `.` **joins between word characters**: `example.com` and `3.14` are each
///   one word, so double-clicking inside `https://example.com` selects `https`,
///   `://` or `example.com` depending on where you hit. A trailing `.` with no
///   word after it splits off, so `end.` at the end of a sentence selects
///   `end`.
///
/// Whitespace runs stop at a line break: `\n` and `\r` are never part of a
/// returned whitespace run, so a double-click never swallows a newline. A
/// double-click on an empty line returns an empty range at that offset, which
/// the caller applies as a plain caret move.
pub fn word_range(text: &str, offset: usize) -> Range<usize> {
    let o = clamp_boundary(text, offset);

    let after_is_space = next_char(text, o).is_none_or(char::is_whitespace);
    let before_is_space = prev_char(text, o).is_none_or(char::is_whitespace);

    // Whitespace on both sides (or the ends of the document): the run of
    // horizontal whitespace is the selection. Empty text lands here too.
    if after_is_space && before_is_space {
        return hspace_run(text, o);
    }

    // Otherwise `o` touches a non-whitespace chunk on at least one side.
    // `chunk_bounds` expands in whichever direction is non-whitespace, so a
    // caret at the trailing edge of a word gets that word rather than the
    // whitespace behind it.
    let (cs, ce) = chunk_bounds(text, o);
    pick_segment(text, cs, ce, o)
}

/// Range a mouse click selects. `click_count` 1 → `None` (a plain caret move,
/// the caller collapses the selection), 2 → word, 3 or more → paragraph.
///
/// `click_count` 0 is treated like 1 and yields `None`.
pub fn click_range(text: &str, offset: usize, click_count: usize) -> Option<Range<usize>> {
    match click_count {
        0 | 1 => None,
        2 => Some(word_range(text, offset)),
        _ => Some(paragraph_range(text, offset)),
    }
}

/// Grow `anchor` to also cover `unit`, for a drag that extends a selection by
/// whole words or paragraphs.
///
/// A double- or triple-click selects a word or paragraph and then, if you keep
/// the button down and drag, macOS keeps *that* first unit selected and swallows
/// whole units toward the pointer — never cutting one in half. `anchor` is the
/// unit first selected; `unit` is the word or paragraph under the pointer now.
/// The merged range is returned along with whether the moving end (the caret) is
/// at the start, so the caller can point the active edge the way the drag went.
pub fn union_toward(anchor: Range<usize>, unit: Range<usize>) -> (Range<usize>, bool) {
    let start = anchor.start.min(unit.start);
    let end = anchor.end.max(unit.end);
    // Dragged before the anchor: the caret is the low end and runs left.
    let reversed = unit.start < anchor.start;
    (start..end, reversed)
}

// ---------------------------------------------------------------------------
// Line / paragraph
// ---------------------------------------------------------------------------

/// Byte range of the logical line at `offset`, excluding the trailing `\n`.
///
/// Found by scanning outward to the nearest `\n` in each direction, so the cost
/// is the length of that one line and not of the document. Only `\n`
/// terminates a line; a `\r` in a CRLF document stays inside the returned range
/// (the blank-line and separator tests below trim ASCII whitespace, so it never
/// changes a classification).
pub fn line_range(text: &str, offset: usize) -> Range<usize> {
    let o = clamp_boundary(text, offset);
    line_start_at(text, o)..line_end_at(text, o)
}

/// Byte range of the paragraph at `offset`: the run of consecutive non-blank
/// lines around it, bounded by blank lines (empty or all-whitespace), by a
/// markdown thematic break line (`---`, `***`, `___` — 3 or more of the same
/// character after trimming), or by the document ends. The trailing `\n` is
/// excluded. When `offset` is on a blank or separator line, that single line is
/// the range.
///
/// Walks line by line outward from `offset` and stops at the first boundary
/// line in each direction, so the cost is the size of the paragraph.
pub fn paragraph_range(text: &str, offset: usize) -> Range<usize> {
    let line = line_range(text, offset);
    if is_boundary_line(&text[line.start..line.end]) {
        return line;
    }

    let mut start = line.start;
    while start > 0 {
        // `start - 1` is the `\n` that ended the previous line.
        let prev_end = start - 1;
        let prev_start = line_start_at(text, prev_end);
        if is_boundary_line(&text[prev_start..prev_end]) {
            break;
        }
        start = prev_start;
    }

    let mut end = line.end;
    while end < text.len() {
        // `end` is a `\n`, so the next line starts right after it.
        let next_start = end + 1;
        let next_end = line_end_at(text, next_start);
        if is_boundary_line(&text[next_start..next_end]) {
            break;
        }
        end = next_end;
    }

    start..end
}

// ---------------------------------------------------------------------------
// Word-wise motion
// ---------------------------------------------------------------------------

/// Start of the previous word from `offset` — Option+Left. Skips any whitespace
/// immediately behind the caret, then moves to the start of the word before it.
/// Crosses line boundaries like macOS does. Returns 0 at the start.
///
/// Punctuation is skipped along with whitespace, so from `foo, bar|` the caret
/// lands before `bar`, and from `foo, |bar` before `foo`. Strictly decreasing:
/// the result is always `< offset` unless `offset` is already 0, so a caller
/// looping on it cannot hang.
pub fn prev_word_start(text: &str, offset: usize) -> usize {
    let o = clamp_boundary(text, offset);
    if o == 0 {
        return 0;
    }

    let (mut cs, mut ce) = chunk_bounds(text, o);
    loop {
        if cs < ce {
            let mut best = None;
            for seg in segments(text, cs, ce) {
                if seg.start >= o {
                    break;
                }
                if seg.kind.is_stop() {
                    best = Some(seg.start);
                }
            }
            if let Some(start) = best {
                return start;
            }
        }

        // Nothing word-like in this chunk: step back over the whitespace in
        // front of it and try the chunk before that.
        let mut p = cs;
        while let Some(c) = prev_char(text, p) {
            if !c.is_whitespace() {
                break;
            }
            p -= c.len_utf8();
        }
        if p == 0 {
            return 0;
        }
        ce = p;
        cs = chunk_start_at(text, p);
    }
}

/// End of the next word from `offset` — Option+Right. Skips whitespace ahead,
/// then moves to the end of the next word. Returns `text.len()` at the end.
///
/// Punctuation is skipped along with whitespace, mirroring [`prev_word_start`].
/// Strictly increasing: the result is always `> offset` unless `offset` is
/// already at the end of the text.
pub fn next_word_end(text: &str, offset: usize) -> usize {
    let o = clamp_boundary(text, offset);
    if o == text.len() {
        return o;
    }

    let (mut cs, mut ce) = chunk_bounds(text, o);
    loop {
        if cs < ce {
            for seg in segments(text, cs, ce) {
                if seg.kind.is_stop() && seg.end > o {
                    return seg.end;
                }
            }
        }

        let mut p = ce;
        while let Some(c) = next_char(text, p) {
            if !c.is_whitespace() {
                break;
            }
            p += c.len_utf8();
        }
        if p == text.len() {
            return text.len();
        }
        cs = p;
        ce = chunk_end_at(text, p);
    }
}

// ---------------------------------------------------------------------------
// Deletion ranges
// ---------------------------------------------------------------------------

/// Range Option+Delete removes: from `prev_word_start(text, offset)` to
/// `offset`. Empty range when already at 0.
pub fn delete_word_back_range(text: &str, offset: usize) -> Range<usize> {
    let o = clamp_boundary(text, offset);
    prev_word_start(text, o)..o
}

/// Range Option+Fn+Delete (forward word delete) removes.
///
/// From `offset` to `next_word_end(text, offset)`. Empty range at the end of
/// the text.
pub fn delete_word_forward_range(text: &str, offset: usize) -> Range<usize> {
    let o = clamp_boundary(text, offset);
    o..next_word_end(text, o)
}

/// Range Cmd+Delete removes: from the start of the line to `offset`.
///
/// Empty range when the caret is already at the start of its line.
pub fn delete_to_line_start_range(text: &str, offset: usize) -> Range<usize> {
    let o = clamp_boundary(text, offset);
    line_start_at(text, o)..o
}

// ---------------------------------------------------------------------------
// Transpose
// ---------------------------------------------------------------------------

/// The edit ⌃T (transpose) makes to `line` with the caret at line-local byte
/// offset `caret`: `(range, replacement)`, where replacing `range` with
/// `replacement` swaps the two graphemes around the caret. `None` when there is
/// nothing to swap.
///
/// `line` is one line, without its trailing newline, so the swap can never
/// cross a line break. Grapheme-based, so `é`, `👍🏽` and CJK move as whole
/// characters rather than bytes.
///
/// The two cases match every Cocoa text field:
/// * a grapheme on each side of the caret — swap them and step past (the caret
///   lands at `range.end`, which is where replacing leaves it);
/// * at the end of the line — swap the two graphemes that precede the caret, the
///   way a trailing ⌃T fixes the last two letters of a word.
///
/// The replacement is always exactly as long as the range it replaces (the same
/// two graphemes, reordered), so the caller's natural "caret at end of inserted
/// text" lands correctly in both cases.
pub fn transpose(line: &str, caret: usize) -> Option<(Range<usize>, String)> {
    let caret = clamp_boundary(line, caret);
    let before = line[..caret].grapheme_indices(true).next_back();
    let after = line[caret..].graphemes(true).next();
    match (before, after) {
        (Some((b0, before)), Some(after)) => {
            let end = caret + after.len();
            Some((b0..end, format!("{after}{before}")))
        }
        (Some((b0, before)), None) => {
            // At the line end: swap the two that come before the caret.
            let (a0, first) = line[..b0].grapheme_indices(true).next_back()?;
            Some((a0..caret, format!("{before}{first}")))
        }
        // Fewer than two graphemes to work with, or a caret at the very start.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Offset helpers
// ---------------------------------------------------------------------------

/// Clamp `offset` into `text` and floor it to the nearest char boundary.
///
/// O(1): a char boundary is at most 3 bytes behind any byte index.
fn clamp_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut o = offset;
    while !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

/// The char starting at `pos`, or `None` at the end. O(1).
fn next_char(text: &str, pos: usize) -> Option<char> {
    text[pos..].chars().next()
}

/// The char ending at `pos`, or `None` at the start. O(1).
fn prev_char(text: &str, pos: usize) -> Option<char> {
    text[..pos].chars().next_back()
}

/// Start of the line containing `pos`: one past the nearest `\n` behind it.
///
/// `rposition` walks backwards and stops at the first hit, so this is O(line),
/// not O(pos). `\n` is ASCII, so a byte scan cannot land mid-character.
fn line_start_at(text: &str, pos: usize) -> usize {
    match text.as_bytes()[..pos].iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => 0,
    }
}

/// End of the line containing `pos`: the nearest `\n` at or after it, or
/// `text.len()`. O(line).
fn line_end_at(text: &str, pos: usize) -> usize {
    match text.as_bytes()[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i,
        None => text.len(),
    }
}

/// True for a line that terminates a paragraph: blank, all-whitespace, or a
/// markdown thematic break.
fn is_boundary_line(line: &str) -> bool {
    line.chars().all(char::is_whitespace) || is_separator_line(line)
}

/// True when a line is a markdown thematic break.
///
/// After trimming ASCII whitespace the line must be 3 or more of the *same*
/// character drawn from `-`, `*`, `_`, optionally with ASCII whitespace between
/// them: `---`, `***`, `___`, `- - -`, `  ---  `. Not `--`, `-*-`, `---a`.
///
/// Mirrors `crate::note::is_separator_line` rather than importing it, so this
/// module stays self-contained and independently testable.
fn is_separator_line(line: &str) -> bool {
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

// ---------------------------------------------------------------------------
// Whitespace-delimited chunks
// ---------------------------------------------------------------------------

/// Whitespace that stays inside one line — everything `char::is_whitespace`
/// accepts except the line terminators.
fn is_hspace(c: char) -> bool {
    c.is_whitespace() && c != '\n' && c != '\r'
}

/// The maximal run of horizontal whitespace around `pos`. May be empty (when
/// `pos` sits against a line break or the ends of the document).
fn hspace_run(text: &str, pos: usize) -> Range<usize> {
    let mut start = pos;
    while let Some(c) = prev_char(text, start) {
        if !is_hspace(c) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = pos;
    while let Some(c) = next_char(text, end) {
        if !is_hspace(c) {
            break;
        }
        end += c.len_utf8();
    }
    start..end
}

/// Start of the maximal run of non-whitespace ending at `pos`.
fn chunk_start_at(text: &str, pos: usize) -> usize {
    let mut start = pos;
    while let Some(c) = prev_char(text, start) {
        if c.is_whitespace() {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

/// End of the maximal run of non-whitespace starting at `pos`.
fn chunk_end_at(text: &str, pos: usize) -> usize {
    let mut end = pos;
    while let Some(c) = next_char(text, end) {
        if c.is_whitespace() {
            break;
        }
        end += c.len_utf8();
    }
    end
}

/// The maximal run of non-whitespace containing `pos`. Empty when `pos` is
/// surrounded by whitespace.
///
/// A whitespace character is always a UAX #29 word boundary and never joins to
/// the text around it, so segmenting one of these chunks in isolation gives
/// exactly the same answer as segmenting the whole document — which is what
/// makes the local scan legitimate rather than merely cheap.
fn chunk_bounds(text: &str, pos: usize) -> (usize, usize) {
    (chunk_start_at(text, pos), chunk_end_at(text, pos))
}

// ---------------------------------------------------------------------------
// Word segments
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Contains at least one alphanumeric character.
    Word,
    /// Entirely ASCII punctuation.
    Punct,
    /// Whitespace, which word motion crosses rather than stops on.
    Space,
    /// Anything else: emoji, symbols, private-use characters.
    Symbol,
}

impl Kind {
    /// Whether word motion stops here.
    ///
    /// A word does, and so does an emoji or a symbol: a double-click already
    /// selects one on its own, so ⌥⌫ treating it as invisible — and eating the
    /// word before it, or the end of the note above — was the same module
    /// disagreeing with itself.
    fn is_stop(self) -> bool {
        matches!(self, Kind::Word | Kind::Symbol)
    }
}

fn kind_of(s: &str) -> Kind {
    if s.chars().any(char::is_alphanumeric) {
        Kind::Word
    } else if s.is_empty() {
        Kind::Symbol
    } else if s.chars().all(char::is_whitespace) {
        Kind::Space
    } else if s.chars().all(|c| c.is_ascii_punctuation()) {
        Kind::Punct
    } else {
        Kind::Symbol
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Segment {
    start: usize,
    end: usize,
    kind: Kind,
}

/// UAX #29 word segments of `text[start..end]`, in document offsets, with
/// adjacent same-kind `Word` and `Punct` segments merged.
///
/// The merge is what makes `日本語` one selection (UAX #29 breaks Han per
/// character) and `://` one selection. `Symbol` never merges, so emoji stay
/// individually selectable.
fn segments(text: &str, start: usize, end: usize) -> Segments<'_> {
    Segments {
        inner: text[start..end].split_word_bound_indices().peekable(),
        base: start,
    }
}

struct Segments<'a> {
    inner: std::iter::Peekable<UWordBoundIndices<'a>>,
    base: usize,
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        let (i, s) = self.inner.next()?;
        let mut seg = Segment {
            start: self.base + i,
            end: self.base + i + s.len(),
            kind: kind_of(s),
        };
        // Symbols and whitespace stay one segment per character, which is what
        // makes a double-click select a single emoji.
        if !matches!(seg.kind, Kind::Symbol | Kind::Space) {
            while let Some(&(j, t)) = self.inner.peek() {
                if self.base + j != seg.end || kind_of(t) != seg.kind {
                    break;
                }
                seg.end = self.base + j + t.len();
                self.inner.next();
            }
        }
        Some(seg)
    }
}

/// The segment of `text[cs..ce]` that a click at `o` selects.
///
/// A segment that strictly contains `o` wins outright. On a boundary between
/// two segments the word wins over punctuation, which is what makes a caret at
/// either edge of a word select that word.
fn pick_segment(text: &str, cs: usize, ce: usize, o: usize) -> Range<usize> {
    let mut before: Option<Segment> = None;
    for seg in segments(text, cs, ce) {
        if seg.start < o && o < seg.end {
            return seg.start..seg.end;
        }
        if seg.start == o {
            if seg.kind != Kind::Word {
                if let Some(b) = before {
                    if b.kind == Kind::Word {
                        return b.start..b.end;
                    }
                }
            }
            return seg.start..seg.end;
        }
        if seg.end <= o {
            before = Some(seg);
        } else {
            break;
        }
    }
    match before {
        Some(b) => b.start..b.end,
        None => o..o,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Assert a range is usable as a slice of `text`.
    #[track_caller]
    fn valid(text: &str, r: &Range<usize>) {
        assert!(r.start <= r.end, "inverted range {r:?}");
        assert!(r.end <= text.len(), "range {r:?} past len {}", text.len());
        assert!(text.is_char_boundary(r.start), "start {} not a boundary", r.start);
        assert!(text.is_char_boundary(r.end), "end {} not a boundary", r.end);
    }

    #[track_caller]
    fn sel(text: &str, offset: usize) -> &str {
        let r = word_range(text, offset);
        valid(text, &r);
        &text[r]
    }

    /// Every string the invariant sweep and several unit tests run over.
    const CORPUS: &[&str] = &[
        "",
        "\n",
        "\n\n\n\n",
        " ",
        "   \n   \n   ",
        "a",
        "hello world",
        "hello  world",
        "  leading and trailing  ",
        "café naïve résumé",
        "日本語のテキストです",
        "👍🏽 emoji 😀😀 run",
        "snake_case kebab-case https://example.com",
        "one\ntwo\n\nthree\n---\nfour",
        "\n\nmiddle\n\n",
        "a\r\nb\r\n",
        "3.14 and 1,000 and end.",
        "don't — em dash — ok",
        "---\n***\n___\n- - -",
        "tabs\tand\tspaces  here",
        "trailing newline\n",
        "ends with space ",
        "Ω≈ç√∫˜µ≤≥÷",
        "a👍b",
    ];

    // -- 1 & 2: never panics, always valid ---------------------------------

    #[test]
    fn every_function_returns_valid_ranges_at_every_offset() {
        for text in CORPUS {
            // Every byte index, plus a few past the end.
            for offset in 0..text.len() + 4 {
                valid(text, &word_range(text, offset));
                valid(text, &line_range(text, offset));
                valid(text, &paragraph_range(text, offset));
                valid(text, &delete_word_back_range(text, offset));
                valid(text, &delete_word_forward_range(text, offset));
                valid(text, &delete_to_line_start_range(text, offset));
                for clicks in 0..5 {
                    if let Some(r) = click_range(text, offset, clicks) {
                        valid(text, &r);
                    }
                }
                for o in [prev_word_start(text, offset), next_word_end(text, offset)] {
                    assert!(o <= text.len());
                    assert!(text.is_char_boundary(o), "{o} not a boundary in {text:?}");
                }
            }
        }
    }

    #[test]
    fn offsets_inside_multibyte_characters_are_floored() {
        let text = "aé日👍🏽b"; // 1 + 2 + 3 + 4 + 4 + 1
        // Byte 2 is the middle of `é`; asking there is the same as asking at 1.
        assert_eq!(word_range(text, 2), word_range(text, 1));
        // Byte 5 is the middle of `日`.
        assert_eq!(word_range(text, 5), word_range(text, 4));
        for offset in 0..text.len() + 4 {
            valid(text, &word_range(text, offset));
        }
    }

    #[test]
    fn offsets_past_the_end_clamp() {
        let text = "hello";
        assert_eq!(word_range(text, 99), 0..5);
        assert_eq!(line_range(text, 99), 0..5);
        assert_eq!(paragraph_range(text, 99), 0..5);
        assert_eq!(next_word_end(text, 99), 5);
        assert_eq!(prev_word_start(text, 99), 0);
        assert_eq!(delete_word_forward_range(text, 99), 5..5);
        assert_eq!(delete_to_line_start_range(text, 99), 0..5);
    }

    /// ⌥⌫ after an emoji used to delete the word before it — and, at the start
    /// of a note, the end of the note above. A double-click already treats an
    /// emoji as its own thing; word motion has to agree.
    #[test]
    fn word_motion_stops_at_an_emoji_rather_than_stepping_over_it() {
        let text = "hello 😀";
        assert_eq!(prev_word_start(text, text.len()), 6, "just the emoji");
        let text = "done ✅";
        assert_eq!(prev_word_start(text, text.len()), 5);
        // Across a note boundary: it must not reach into the note above.
        let text = "- item one\n- 😀";
        assert_eq!(prev_word_start(text, text.len()), 13, "the emoji alone");
    }

    #[test]
    fn word_motion_steps_through_a_line_of_words_and_emoji_one_at_a_time() {
        let text = "a 😀 b";
        let mut at = 0;
        let mut stops = vec![at];
        while at < text.len() {
            let next = next_word_end(text, at);
            assert!(next > at, "stuck at {at}");
            at = next;
            stops.push(at);
        }
        assert_eq!(stops, vec![0, 1, 6, 8], "a | 😀 | b");
    }

    #[test]
    fn empty_text() {
        assert_eq!(word_range("", 0), 0..0);
        assert_eq!(line_range("", 0), 0..0);
        assert_eq!(paragraph_range("", 0), 0..0);
        assert_eq!(prev_word_start("", 0), 0);
        assert_eq!(next_word_end("", 0), 0);
        assert_eq!(delete_word_back_range("", 5), 0..0);
        assert_eq!(delete_word_forward_range("", 5), 0..0);
        assert_eq!(delete_to_line_start_range("", 5), 0..0);
    }

    #[test]
    fn only_newlines() {
        let text = "\n\n\n";
        assert_eq!(line_range(text, 0), 0..0);
        assert_eq!(line_range(text, 1), 1..1);
        assert_eq!(line_range(text, 3), 3..3);
        assert_eq!(paragraph_range(text, 1), 1..1);
        assert_eq!(word_range(text, 1), 1..1);
        assert_eq!(prev_word_start(text, 3), 0);
        assert_eq!(next_word_end(text, 0), 3);
    }

    #[test]
    fn five_megabyte_single_line_does_not_panic() {
        let text = "x".repeat(5 * 1024 * 1024);
        let mid = text.len() / 2;
        assert_eq!(line_range(&text, mid), 0..text.len());
        assert_eq!(paragraph_range(&text, mid), 0..text.len());
        assert_eq!(word_range(&text, mid), 0..text.len());
        assert_eq!(prev_word_start(&text, mid), 0);
        assert_eq!(next_word_end(&text, mid), text.len());
        assert_eq!(delete_to_line_start_range(&text, mid), 0..mid);
    }

    // -- 3: word_range semantics -------------------------------------------

    #[test]
    fn word_range_picks_the_word_under_the_caret() {
        let text = "hello world";
        assert_eq!(sel(text, 0), "hello");
        assert_eq!(sel(text, 2), "hello");
        assert_eq!(sel(text, 5), "hello", "trailing edge stays on the word");
        assert_eq!(sel(text, 6), "world", "leading edge stays on the word");
        assert_eq!(sel(text, 8), "world");
        assert_eq!(sel(text, 11), "world", "end of text");
    }

    #[test]
    fn word_range_selects_whitespace_runs() {
        let text = "a    b";
        assert_eq!(sel(text, 2), "    ");
        assert_eq!(sel(text, 3), "    ");
        // At either edge the adjoining word wins.
        assert_eq!(sel(text, 1), "a");
        assert_eq!(sel(text, 5), "b");
        assert_eq!(sel(text, 4), "    ");
    }

    #[test]
    fn word_range_never_crosses_a_newline() {
        let text = "a  \n  b";
        assert_eq!(sel(text, 2), "  ");
        assert_eq!(sel(text, 3), "  ", "stops before the newline");
        assert_eq!(sel(text, 4), "  ", "starts after the newline");
        for offset in 0..text.len() + 1 {
            assert!(!sel(text, offset).contains('\n'));
        }
    }

    #[test]
    fn word_range_on_a_blank_line_is_a_caret() {
        let text = "a\n\nb";
        assert_eq!(word_range(text, 2), 2..2);
    }

    #[test]
    fn word_range_handles_accents_and_scripts() {
        assert_eq!(sel("café au lait", 2), "café");
        assert_eq!(sel("café au lait", 5), "café", "byte 5 is the end of café");
        assert_eq!(sel("naïve idea", 3), "naïve");
        assert_eq!(sel("résumé", 0), "résumé");
    }

    #[test]
    fn word_range_selects_a_whole_cjk_run() {
        // UAX #29 breaks Han per character; the same-kind merge puts the run
        // back together so a double-click grabs the phrase, not one glyph.
        let text = "日本語 テキスト";
        assert_eq!(sel(text, 0), "日本語");
        assert_eq!(sel(text, 3), "日本語");
        assert_eq!(sel(text, 10), "テキスト");
    }

    #[test]
    fn word_range_selects_one_emoji_at_a_time() {
        // `👍🏽` is base + skin-tone modifier: one segment, selected whole.
        let text = "👍🏽😀 ok";
        assert_eq!(sel(text, 0), "👍🏽");
        assert_eq!(sel(text, 4), "👍🏽", "inside the modifier sequence");
        assert_eq!(sel(text, 8), "😀");
        assert_eq!(sel(text, 13), "ok");
    }

    /// The documented choices for `_`, `-` and `.`.
    #[test]
    fn word_range_connector_policy() {
        // `_` joins: one word.
        assert_eq!(sel("snake_case here", 3), "snake_case");
        assert_eq!(sel("snake_case here", 6), "snake_case");
        assert_eq!(sel("__dunder__", 4), "__dunder__");

        // `-` splits: three selections.
        assert_eq!(sel("kebab-case here", 2), "kebab");
        assert_eq!(sel("kebab-case here", 5), "kebab", "edge prefers the word");
        assert_eq!(sel("kebab-case here", 8), "case");
        // Landing strictly inside the hyphen run is the only way to get `-`.
        assert_eq!(sel("a--b", 2), "--");

        // `.` joins between word characters, splits when trailing.
        assert_eq!(sel("example.com", 3), "example.com");
        assert_eq!(sel("3.14 rest", 1), "3.14");
        assert_eq!(sel("the end.", 5), "end");
        assert_eq!(sel("the end.", 8), ".", "past the word, on the period");
    }

    #[test]
    fn word_range_in_a_url() {
        let text = "https://example.com/a";
        assert_eq!(sel(text, 2), "https");
        assert_eq!(sel(text, 6), "://", "ASCII punctuation merges into a run");
        assert_eq!(sel(text, 10), "example.com");
        assert_eq!(sel(text, 20), "a");
    }

    #[test]
    fn word_range_keeps_apostrophes_inside_words() {
        assert_eq!(sel("don't stop", 2), "don't");
    }

    // -- click_range --------------------------------------------------------

    #[test]
    fn click_counts_map_to_the_right_ranges() {
        let text = "one two\nthree\n\nfour";
        assert_eq!(click_range(text, 4, 0), None);
        assert_eq!(click_range(text, 4, 1), None);
        assert_eq!(click_range(text, 4, 2), Some(word_range(text, 4)));
        assert_eq!(click_range(text, 4, 3), Some(paragraph_range(text, 4)));
        assert_eq!(click_range(text, 4, 9), Some(paragraph_range(text, 4)));
        assert_eq!(click_range(text, 4, 2).unwrap(), 4..7);
        assert_eq!(click_range(text, 4, 3).unwrap(), 0..13);
    }

    // -- union_toward -------------------------------------------------------

    #[test]
    fn union_toward_keeps_the_anchor_and_swallows_whole_units() {
        let text = "one two three four";
        // Anchor on "two", drag right onto "four": the whole span, caret forward.
        let anchor = word_range(text, 5); // "two" 4..7
        let onto = word_range(text, 15); // "four" 14..18
        let (range, reversed) = union_toward(anchor.clone(), onto);
        assert_eq!(&text[range.clone()], "two three four");
        assert!(!reversed, "dragging right leaves the caret at the end");

        // Drag left of the anchor onto "one": the caret is now the low end.
        let onto = word_range(text, 0); // "one" 0..3
        let (range, reversed) = union_toward(anchor, onto);
        assert_eq!(&text[range], "one two");
        assert!(reversed, "dragging left points the caret backwards");
    }

    #[test]
    fn union_toward_on_the_anchor_itself_is_the_anchor() {
        let text = "alpha beta";
        let anchor = word_range(text, 0); // "alpha" 0..5
        let (range, reversed) = union_toward(anchor.clone(), anchor.clone());
        assert_eq!(range, anchor);
        assert!(!reversed);
    }

    // -- line_range ---------------------------------------------------------

    #[test]
    fn line_range_excludes_the_newline() {
        let text = "one\ntwo\nthree";
        assert_eq!(line_range(text, 0), 0..3);
        assert_eq!(line_range(text, 3), 0..3, "at the newline, still line 0");
        assert_eq!(line_range(text, 4), 4..7);
        assert_eq!(line_range(text, 7), 4..7);
        assert_eq!(line_range(text, 8), 8..13);
        assert_eq!(line_range(text, 13), 8..13);
    }

    #[test]
    fn line_range_on_empty_lines() {
        let text = "a\n\nb\n";
        assert_eq!(line_range(text, 2), 2..2);
        assert_eq!(line_range(text, 5), 5..5, "after a trailing newline");
    }

    // -- 4: paragraph_range -------------------------------------------------

    /// Hand-checked fixture. Byte offsets are counted in the assertions below.
    ///
    /// ```text
    ///  0 "first para line one"      0..19
    /// 20 "first para line two"     20..39
    /// 40 ""                        40..40
    /// 41 "   "                     41..44   (all-whitespace, also a boundary)
    /// 45 "second para"             45..56
    /// 57 "---"                     57..60   (separator)
    /// 61 "third para line one"     61..80
    /// 81 "third para line two"     81..100
    /// ```
    const FIXTURE: &str = "first para line one\nfirst para line two\n\n   \nsecond para\n---\nthird para line one\nthird para line two";

    #[test]
    fn paragraph_fixture_offsets_are_what_the_comment_claims() {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(FIXTURE.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        assert_eq!(starts, vec![0, 20, 40, 41, 45, 57, 61, 81]);
        assert_eq!(FIXTURE.len(), 100);
    }

    #[test]
    fn paragraph_at_the_very_start() {
        for offset in [0, 5, 19, 20, 30, 39] {
            assert_eq!(paragraph_range(FIXTURE, offset), 0..39, "offset {offset}");
        }
    }

    #[test]
    fn paragraph_stops_at_blank_and_whitespace_lines() {
        // The empty line and the all-whitespace line are each their own range.
        assert_eq!(paragraph_range(FIXTURE, 40), 40..40);
        assert_eq!(paragraph_range(FIXTURE, 42), 41..44);
        // The one-line paragraph between them and the separator.
        for offset in [45, 50, 56] {
            assert_eq!(paragraph_range(FIXTURE, offset), 45..56, "offset {offset}");
        }
    }

    #[test]
    fn paragraph_stops_at_a_separator() {
        assert_eq!(paragraph_range(FIXTURE, 58), 57..60, "on the separator");
        for offset in [61, 70, 80, 81, 95, 100] {
            assert_eq!(paragraph_range(FIXTURE, offset), 61..100, "offset {offset}");
        }
    }

    #[test]
    fn paragraph_at_the_very_end_without_trailing_newline() {
        assert_eq!(paragraph_range(FIXTURE, FIXTURE.len()), 61..100);
    }

    #[test]
    fn paragraph_with_leading_and_trailing_blank_lines() {
        let text = "\n\n  body one\n  body two\n\n\n";
        //          0 1 2           12          24 25
        assert_eq!(paragraph_range(text, 0), 0..0);
        assert_eq!(paragraph_range(text, 1), 1..1);
        assert_eq!(paragraph_range(text, 5), 2..23);
        assert_eq!(paragraph_range(text, 23), 2..23);
        assert_eq!(paragraph_range(text, 24), 24..24);
        assert_eq!(paragraph_range(text, 25), 25..25, "after the last newline");
    }

    #[test]
    fn paragraph_of_a_whole_document_without_blank_lines() {
        let text = "a\nb\nc";
        assert_eq!(paragraph_range(text, 3), 0..5);
    }

    #[test]
    fn all_separator_spellings_bound_a_paragraph() {
        for sep in ["---", "***", "___", "- - -", "  ---  ", "-----"] {
            let text = format!("above\n{sep}\nbelow");
            assert_eq!(paragraph_range(&text, 0), 0..5, "sep {sep:?}");
            let below = text.len() - 5;
            assert_eq!(
                paragraph_range(&text, below),
                below..text.len(),
                "sep {sep:?}"
            );
        }
        // Near misses stay inside the paragraph.
        for not_sep in ["--", "-*-", "---a"] {
            let text = format!("above\n{not_sep}\nbelow");
            assert_eq!(paragraph_range(&text, 0), 0..text.len(), "{not_sep:?}");
        }
    }

    // -- 5: motion ----------------------------------------------------------

    #[test]
    fn word_motion_matches_mac_expectations() {
        let text = "one two three";
        assert_eq!(next_word_end(text, 0), 3);
        assert_eq!(next_word_end(text, 3), 7);
        assert_eq!(next_word_end(text, 1), 3, "from mid-word, to that word's end");
        assert_eq!(next_word_end(text, 7), 13);
        assert_eq!(next_word_end(text, 13), 13);

        assert_eq!(prev_word_start(text, 13), 8);
        assert_eq!(prev_word_start(text, 8), 4);
        assert_eq!(prev_word_start(text, 9), 8, "from mid-word, to that word's start");
        assert_eq!(prev_word_start(text, 4), 0);
        assert_eq!(prev_word_start(text, 0), 0);
    }

    #[test]
    fn word_motion_skips_punctuation() {
        let text = "foo, bar";
        assert_eq!(prev_word_start(text, 8), 5, "past `, ` to the start of bar");
        assert_eq!(prev_word_start(text, 5), 0, "past `, ` to the start of foo");
        assert_eq!(next_word_end(text, 3), 8, "past `, ` to the end of bar");
        assert_eq!(next_word_end(text, 0), 3);
    }

    #[test]
    fn word_motion_crosses_lines() {
        let text = "alpha\n\n  beta";
        assert_eq!(next_word_end(text, 5), 13);
        assert_eq!(prev_word_start(text, 9), 0);
        assert_eq!(next_word_end(text, 0), 5);
    }

    #[test]
    fn word_motion_over_text_with_no_words_reaches_the_ends() {
        let text = "!!! ... ???";
        assert_eq!(next_word_end(text, 0), text.len());
        assert_eq!(prev_word_start(text, text.len()), 0);
    }

    /// Requirement: the editor must not be able to loop forever on these.
    #[test]
    fn motion_always_makes_progress() {
        for text in CORPUS {
            // Forward from 0 to len.
            let mut o = 0;
            let mut steps = 0;
            while o < text.len() {
                let next = next_word_end(text, o);
                assert!(next > o, "no forward progress at {o} in {text:?}");
                assert!(next <= text.len());
                assert!(text.is_char_boundary(next));
                o = next;
                steps += 1;
                assert!(steps <= text.len() + 1, "runaway loop in {text:?}");
            }
            assert_eq!(next_word_end(text, text.len()), text.len(), "fixpoint at end");

            // Backward from len to 0.
            let mut o = text.len();
            let mut steps = 0;
            while o > 0 {
                let prev = prev_word_start(text, o);
                assert!(prev < o, "no backward progress at {o} in {text:?}");
                assert!(text.is_char_boundary(prev));
                o = prev;
                steps += 1;
                assert!(steps <= text.len() + 1, "runaway loop in {text:?}");
            }
            assert_eq!(prev_word_start(text, 0), 0, "fixpoint at start");
        }
    }

    #[test]
    fn forward_walk_visits_every_word_end_in_order() {
        let text = "one two, three\n\nfour-five six_seven";
        let mut ends = Vec::new();
        let mut o = 0;
        while o < text.len() {
            o = next_word_end(text, o);
            ends.push(&text[..o]);
        }
        let words: Vec<&str> = ends
            .iter()
            .map(|prefix| {
                let start = prev_word_start(text, prefix.len());
                &text[start..prefix.len()]
            })
            .collect();
        assert_eq!(
            words,
            vec!["one", "two", "three", "four", "five", "six_seven"]
        );
    }

    // -- deletion ranges ----------------------------------------------------

    #[test]
    fn delete_word_back() {
        let text = "hello world";
        assert_eq!(delete_word_back_range(text, 11), 6..11);
        assert_eq!(delete_word_back_range(text, 8), 6..8);
        assert_eq!(delete_word_back_range(text, 6), 0..6);
        assert_eq!(delete_word_back_range(text, 0), 0..0);
    }

    #[test]
    fn delete_word_forward() {
        let text = "hello world";
        assert_eq!(delete_word_forward_range(text, 0), 0..5);
        assert_eq!(delete_word_forward_range(text, 5), 5..11);
        assert_eq!(delete_word_forward_range(text, 11), 11..11);
    }

    #[test]
    fn delete_to_line_start() {
        let text = "one\ntwo three";
        assert_eq!(delete_to_line_start_range(text, 11), 4..11);
        assert_eq!(delete_to_line_start_range(text, 4), 4..4);
        assert_eq!(delete_to_line_start_range(text, 2), 0..2);
    }

    /// Apply `transpose` to `line` at `caret` and return the resulting line and
    /// where the caret lands (the replacement is always as long as its range).
    #[track_caller]
    fn transposed(line: &str, caret: usize) -> Option<(String, usize)> {
        transpose(line, caret).map(|(range, replacement)| {
            let mut out = line.to_string();
            out.replace_range(range.clone(), &replacement);
            (out, range.start + replacement.len())
        })
    }

    #[test]
    fn transpose_swaps_the_characters_around_the_caret() {
        // Mid-line: swap the pair and step past it.
        assert_eq!(transposed("ab", 1), Some(("ba".into(), 2)));
        assert_eq!(transposed("abc", 1), Some(("bac".into(), 2)));
        // At the end of the line: swap the two that precede the caret.
        assert_eq!(transposed("ab", 2), Some(("ba".into(), 2)));
        assert_eq!(transposed("teh", 3), Some(("the".into(), 3)));
        // Nothing to swap: too short, or the caret is at the very start.
        assert_eq!(transpose("a", 1), None);
        assert_eq!(transpose("", 0), None);
        assert_eq!(transpose("ab", 0), None);
        // Multi-byte graphemes move whole, never split.
        assert_eq!(transposed("é🎉", "é".len()), Some(("🎉é".into(), "🎉é".len())));
        let s = "a👍🏽";
        assert_eq!(transposed(s, 1), Some(("👍🏽a".into(), s.len())));
    }

    #[test]
    fn deleting_a_word_back_actually_removes_a_word() {
        let mut s = String::from("alpha beta gamma");
        let mut caret = s.len();
        let r = delete_word_back_range(&s, caret);
        s.replace_range(r.clone(), "");
        caret = r.start;
        assert_eq!(s, "alpha beta ");
        let r = delete_word_back_range(&s, caret);
        s.replace_range(r.clone(), "");
        assert_eq!(s, "alpha ", "the whitespace goes with the word");
    }

    // -- 6: scoping ---------------------------------------------------------

    /// The functions must be local, not O(document). A 5 MB buffer with 100 000
    /// random probes would take minutes if any of them scanned the document.
    #[test]
    fn scoping_is_local() {
        // ~5 MB of short lines, with blank lines and separators mixed in.
        let mut text = String::with_capacity(5 * 1024 * 1024 + 128);
        let mut i = 0u64;
        while text.len() < 5 * 1024 * 1024 {
            match i % 11 {
                4 => text.push('\n'),
                9 => text.push_str("---\n"),
                _ => {
                    text.push_str("the quick brown fox 日本語 jumps over lazy dogs\n");
                }
            }
            i += 1;
        }
        let len = text.len();
        assert!(len >= 5 * 1024 * 1024, "corpus is {len} bytes");

        // Deterministic xorshift so the timing is reproducible.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        const N: usize = 100_000;
        let offsets: Vec<usize> = (0..N).map(|_| (rand() as usize) % (len + 1)).collect();

        let mut checksum = 0usize;

        let t = Instant::now();
        for &o in &offsets {
            checksum ^= word_range(&text, o).end;
        }
        let word = t.elapsed();

        let t = Instant::now();
        for &o in &offsets {
            checksum ^= line_range(&text, o).end;
        }
        let line = t.elapsed();

        let t = Instant::now();
        for &o in &offsets {
            checksum ^= paragraph_range(&text, o).end;
        }
        let para = t.elapsed();

        let total = word + line + para;
        println!(
            "scoping_is_local: {len} byte buffer, {N} calls each\n  \
             word_range      {word:?}\n  \
             line_range      {line:?}\n  \
             paragraph_range {para:?}\n  \
             total           {total:?}  (checksum {checksum})"
        );

        // Measured on an M-series Mac: ~31ms optimised, ~331ms at opt-level 0.
        // The budgets leave an order of magnitude of headroom while staying
        // ~4 orders of magnitude below what an O(document) scan would need
        // (300k probes × 5 MB is half a terabyte of scanning).
        let budget = if cfg!(debug_assertions) { 3.0 } else { 1.0 };
        assert!(
            total.as_secs_f64() < budget,
            "300k calls took {total:?}, over the {budget}s budget — something is scanning the whole document"
        );
    }

    // -- internal helpers ---------------------------------------------------

    #[test]
    fn separator_line_detection() {
        for line in ["---", "***", "___", "- - -", "  ---  ", "-----", "\t***\t"] {
            assert!(is_separator_line(line), "{line:?}");
        }
        for line in ["", "  ", "--", "-*-", "---a", "a---", "-", "text"] {
            assert!(!is_separator_line(line), "{line:?}");
        }
    }

    #[test]
    fn segment_kinds() {
        assert_eq!(kind_of("word"), Kind::Word);
        assert_eq!(kind_of("42"), Kind::Word);
        assert_eq!(kind_of("日"), Kind::Word);
        assert_eq!(kind_of("://"), Kind::Punct);
        assert_eq!(kind_of("_"), Kind::Punct);
        assert_eq!(kind_of("👍🏽"), Kind::Symbol);
        assert_eq!(kind_of(" "), Kind::Space);
        assert_eq!(kind_of("\t"), Kind::Space);
        assert_eq!(kind_of("≈"), Kind::Symbol);
    }
}
