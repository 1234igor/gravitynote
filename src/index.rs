//! Byte-offset index over a document's lines.
//!
//! The whole note is one `String`. Naively, everything the editor needs per
//! keystroke — "which line is the caret on?", "where does line 3 start?",
//! "what is this offset in UTF-16 units?" — is a scan of the entire buffer.
//! At 20 years of daily notes (~10 MB, ~200k lines) that is a visible stall on
//! every arrow key, and macOS IME calls the UTF-16 conversion constantly.
//!
//! [`LineIndex`] is built once per text change (O(n)) and then answers:
//!
//! * `line_at`      — O(log n) binary search over line starts
//! * `line_start` / `line_end` / `line_range` — O(1)
//! * `to_utf16` / `from_utf16` — O(log n + line length), via a cumulative
//!   UTF-16 table sampled at every line start
//! * [`prev_grapheme`] / [`next_grapheme`] — O(line length), capped
//!
//! Memory is two `Vec<usize>` with one entry per line: ~3.2 MB at 200k lines.
//!
//! Rebuilding on every keystroke is the wrong shape, though: at 9.2 MB
//! [`LineIndex::new`] costs ~5.5 ms, a third of a 60 Hz frame, to re-derive
//! something a one-character insert barely changed. So [`LineIndex::splice`] is
//! the typing fast path — it shifts the tail of the two tables in place by the
//! signed byte / UTF-16 delta of the edit and splices in only the lines the edit
//! actually created or destroyed, touching no text outside the edited span.
//! [`LineIndex::new`] remains the fallback, for the first build, for edits large
//! or unusual enough that a rebuild is no worse (a whole-document paste, a
//! reload from disk), and for any call whose arguments do not line up with the
//! new buffer. The two are required to agree exactly: `splice` ends with a
//! `debug_assert_eq!` against a full rebuild, so a divergence is a loud test
//! failure rather than a silently crooked caret.
//!
//! This module is pure logic — no `gpui`, no I/O. Its only dependency is
//! `unicode-segmentation`.

use unicode_segmentation::UnicodeSegmentation;

/// Maximum number of bytes [`prev_grapheme`] / [`next_grapheme`] will scan.
///
/// A grapheme cluster is bounded by the line it lives on, so scanning the line
/// is already correct — but a pathological document could be one 5 MB line with
/// no `\n` at all, and the caret must still move instantly. So the scan window
/// is additionally capped at this many bytes and floored to a `char` boundary.
///
/// Starting grapheme segmentation from an arbitrary `char` boundary 1 KiB away
/// is exact for every real grapheme cluster: no Unicode cluster (emoji ZWJ
/// sequence, flag pair, Devanagari cluster, arbitrarily long combining-mark
/// run) comes anywhere near 1024 bytes, so the window always starts *between*
/// clusters, never inside one.
const SCAN_CAP: usize = 1024;

/// Byte-offset index over a document's lines, plus a cumulative UTF-16 table so
/// IME offset conversion is O(log n + line length) instead of O(document).
///
/// Build it once per text change with [`LineIndex::new`]. Every method that
/// takes a `text: &str` expects the exact text the index was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset of the first byte of each logical line. Always starts with
    /// `0`, so `line_starts.len() == line_count()` and is never empty.
    line_starts: Vec<usize>,
    /// Cumulative UTF-16 code units *before* each line start. Same length as
    /// `line_starts`; `utf16_starts[0] == 0`.
    utf16_starts: Vec<usize>,
    /// Byte length of the indexed text.
    len: usize,
    /// Total UTF-16 code units in the indexed text.
    total_utf16: usize,
}

impl LineIndex {
    /// Build the index. O(n), done once per text change.
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        // Rough guess: ~50 bytes per line keeps reallocation down on big docs.
        let guess = bytes.len() / 48 + 1;
        let mut line_starts = Vec::with_capacity(guess);
        let mut utf16_starts = Vec::with_capacity(guess);
        line_starts.push(0);
        utf16_starts.push(0);

        let mut utf16 = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            // Count one UTF-16 unit per `char` (i.e. per non-continuation
            // byte), plus one extra for astral chars (4-byte lead => surrogate
            // pair). This is the same as summing `char::len_utf16` but without
            // decoding.
            if b & 0xC0 != 0x80 {
                utf16 += 1;
                if b >= 0xF0 {
                    utf16 += 1;
                }
            }
            if b == b'\n' {
                line_starts.push(i + 1);
                utf16_starts.push(utf16);
            }
        }

        LineIndex {
            line_starts,
            utf16_starts,
            len: bytes.len(),
            total_utf16: utf16,
        }
    }

    /// Update the index in place after a replacement, instead of rebuilding.
    ///
    /// `new_text` is the buffer **after** the edit. The bytes `removed` used to
    /// sit at `start`, and `inserted` now sits there — i.e. the old buffer had
    /// `removed` at `start..start + removed.len()` and the new buffer has
    /// `inserted` at `start..start + inserted.len()`.
    ///
    /// O(lines after `start` + `inserted.len()`), with no reallocation when the
    /// line count is unchanged — versus O(document) for [`LineIndex::new`].
    /// (`removed` is scanned once too, for its UTF-16 length; when typing that
    /// is zero or one byte.)
    ///
    /// The result is always exactly equal to `LineIndex::new(new_text)`.
    ///
    /// Arguments that cannot describe this edit — a `start` past the end or
    /// inside a character, lengths that do not add up, an `inserted` that is not
    /// what actually sits at `start` in `new_text` — are caught cheaply and fall
    /// back to a full rebuild, so a confused caller gets a slow index rather
    /// than a wrong one. The *contents* of `removed` cannot be checked (the old
    /// buffer is gone) beyond its `\n` count, which must match what the index
    /// already recorded for that span; its UTF-16 width is taken on trust, and
    /// the `debug_assert_eq!` below catches a caller that lies about it.
    pub fn splice(&mut self, new_text: &str, start: usize, removed: &str, inserted: &str) {
        let removed_len = removed.len();
        let inserted_len = inserted.len();

        // Cheap consistency gate. The third condition is the interesting one:
        // it ties the old length, the new length and both spans together, and
        // it implies `start + removed_len <= self.len` (so the old span really
        // was inside the old buffer).
        if start > new_text.len()
            || inserted_len > new_text.len() - start
            || self.len + inserted_len != new_text.len() + removed_len
            || !new_text.is_char_boundary(start)
            || &new_text[start..start + inserted_len] != inserted
        {
            *self = LineIndex::new(new_text);
            return;
        }

        // Entries strictly inside the replaced span: line starts in
        // `(start, start + removed_len]`, i.e. the lines opened by a `\n` that
        // `removed` took away. `line_starts[0] == 0 <= start`, so `first >= 1`
        // and the line *containing* `start` is `first - 1` — it keeps its start.
        let old_end = start + removed_len;
        let first = self.line_starts.partition_point(|&s| s <= start);
        let last = self.line_starts.partition_point(|&s| s <= old_end);

        // One pass over `removed`: UTF-16 width and `\n` count. The `\n` count
        // must agree with the entries we are about to drop, or the caller is
        // describing an edit this index never saw.
        let mut removed_utf16 = 0usize;
        let mut removed_newlines = 0usize;
        for &b in removed.as_bytes() {
            if b & 0xC0 != 0x80 {
                removed_utf16 += 1;
                if b >= 0xF0 {
                    removed_utf16 += 1;
                }
            }
            if b == b'\n' {
                removed_newlines += 1;
            }
        }
        if removed_newlines != last - first || removed_utf16 > self.total_utf16 {
            *self = LineIndex::new(new_text);
            return;
        }

        // One pass over `inserted`: UTF-16 width, plus every line it opens as
        // (absolute byte start, UTF-16 units from `start`). Allocates only when
        // the insertion actually contains a `\n` — the typing path does not.
        let mut inserted_utf16 = 0usize;
        let mut opened: Vec<(usize, usize)> = Vec::new();
        for (i, &b) in inserted.as_bytes().iter().enumerate() {
            if b & 0xC0 != 0x80 {
                inserted_utf16 += 1;
                if b >= 0xF0 {
                    inserted_utf16 += 1;
                }
            }
            if b == b'\n' {
                opened.push((start + i + 1, inserted_utf16));
            }
        }

        // Cumulative UTF-16 at `start`, needed only to place those new lines.
        // Everything before `start` is untouched by the edit, so this reads the
        // *new* text against the *old* table and is still exact. Costs at most
        // the length of the partial line before the caret.
        let base_utf16 = if opened.is_empty() {
            0
        } else {
            let line = first - 1;
            self.utf16_starts[line] + utf16_len(&new_text[self.line_starts[line]..start])
        };

        // Shift the untouched tail. `wrapping_add` of a negative delta cast to
        // `usize` is plain two's-complement subtraction; no entry can go below
        // zero, since every one of them is `> old_end >= |delta|`.
        let byte_delta = inserted_len.wrapping_sub(removed_len);
        let utf16_delta = inserted_utf16.wrapping_sub(removed_utf16);
        if byte_delta != 0 {
            for s in &mut self.line_starts[last..] {
                *s = s.wrapping_add(byte_delta);
            }
        }
        if utf16_delta != 0 {
            for u in &mut self.utf16_starts[last..] {
                *u = u.wrapping_add(utf16_delta);
            }
        }

        // Swap the dropped lines for the opened ones. Equal counts (including
        // the overwhelmingly common zero-for-zero) write straight through
        // without touching the tail or the allocation.
        if opened.len() == last - first {
            for (slot, &(s, _)) in self.line_starts[first..last].iter_mut().zip(opened.iter()) {
                *slot = s;
            }
            for (slot, &(_, u)) in self.utf16_starts[first..last].iter_mut().zip(opened.iter()) {
                *slot = base_utf16 + u;
            }
        } else {
            drop(
                self.line_starts
                    .splice(first..last, opened.iter().map(|&(s, _)| s)),
            );
            drop(
                self.utf16_starts
                    .splice(first..last, opened.iter().map(|&(_, u)| base_utf16 + u)),
            );
        }

        self.len = new_text.len();
        self.total_utf16 = self.total_utf16 + inserted_utf16 - removed_utf16;

        debug_assert_eq!(*self, LineIndex::new(new_text));
    }

    /// Logical line count. Equals `text.split('\n').count()`; 1 for `""`.
    ///
    /// A trailing `\n` therefore yields a final empty line, which is what makes
    /// pressing Enter at the end of the document feel natural.
    #[inline]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte offset where line `i` starts. Clamped to [`len`](Self::len) when
    /// out of range.
    #[inline]
    pub fn line_start(&self, i: usize) -> usize {
        self.line_starts.get(i).copied().unwrap_or(self.len)
    }

    /// Byte offset where line `i`'s CONTENT ends (before its `\n`).
    /// Clamped to [`len`](Self::len) when out of range.
    #[inline]
    pub fn line_end(&self, i: usize) -> usize {
        match self.line_starts.get(i + 1) {
            // The next line starts just past this line's `\n`.
            Some(&next) => next - 1,
            None => self.len,
        }
    }

    /// Content range of line `i`, or `None` when `i >= line_count()`.
    #[inline]
    pub fn line_range(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.line_count() {
            return None;
        }
        Some((self.line_start(i), self.line_end(i)))
    }

    /// O(log n). Index of the line containing `offset`.
    ///
    /// TIE-BREAK (matches the app's original `Note::line_index_at`): an offset
    /// sitting ON the `\n` that terminates line `i` belongs to line `i`, not to
    /// line `i + 1`. For `"aa\nbb\ncc"`: offsets `0..=2` are line 0 (2 is the
    /// `\n`), `3..=5` are line 1, `6..=8` are line 2. Offsets past the end
    /// return the last line.
    #[inline]
    pub fn line_at(&self, offset: usize) -> usize {
        let offset = offset.min(self.len);
        // Largest `i` with `line_starts[i] <= offset`. `line_starts[0] == 0`,
        // so the partition point is always >= 1 and the subtraction is safe.
        //
        // This *is* the tie-break: the `\n` terminating line `i` sits at
        // `line_starts[i + 1] - 1`, strictly below the next line's start, so it
        // naturally lands on line `i`.
        self.line_starts.partition_point(|&s| s <= offset) - 1
    }

    /// Byte length of the indexed text.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// O(log n + line length). `text` MUST be the exact text this index was
    /// built from; passing anything else is a logic error.
    ///
    /// `offset` is clamped to `len()` and floored to a `char` boundary, so a
    /// mid-character offset can never panic.
    pub fn to_utf16(&self, text: &str, offset: usize) -> usize {
        debug_assert_eq!(
            text.len(),
            self.len,
            "LineIndex::to_utf16 called with text it was not built from"
        );
        let offset = floor_char_boundary(text, offset.min(text.len()));
        let line = self.line_at(offset);
        let start = self.line_start(line).min(offset);
        self.utf16_starts[line] + utf16_len(&text[start..offset])
    }

    /// O(log n + line length). Inverse of [`to_utf16`](Self::to_utf16).
    ///
    /// A `utf16_offset` that lands inside a surrogate pair rounds *up* to the
    /// end of that character, matching the conversion this replaces (and what
    /// AppKit expects: never a byte offset inside a character).
    pub fn from_utf16(&self, text: &str, utf16_offset: usize) -> usize {
        debug_assert_eq!(
            text.len(),
            self.len,
            "LineIndex::from_utf16 called with text it was not built from"
        );
        let target = utf16_offset.min(self.total_utf16);
        // Largest `i` with `utf16_starts[i] <= target`; `utf16_starts[0] == 0`.
        let line = self.utf16_starts.partition_point(|&u| u <= target) - 1;

        let start = self.line_start(line);
        // Span of this line INCLUDING its `\n`, which owns one UTF-16 unit.
        let span_end = self.line_starts.get(line + 1).copied().unwrap_or(self.len);
        let mut remaining = target - self.utf16_starts[line];

        let mut offset = start;
        for ch in text[start..span_end].chars() {
            if remaining == 0 {
                break;
            }
            remaining = remaining.saturating_sub(ch.len_utf16());
            offset += ch.len_utf8();
        }
        offset
    }

    /// Total UTF-16 length of the document.
    #[inline]
    pub fn total_utf16(&self) -> usize {
        self.total_utf16
    }

    /// Grapheme column of `offset` within its line (0-based). O(line length).
    pub fn grapheme_col(&self, text: &str, offset: usize) -> usize {
        debug_assert_eq!(
            text.len(),
            self.len,
            "LineIndex::grapheme_col called with text it was not built from"
        );
        let offset = floor_char_boundary(text, offset.min(text.len()));
        let line = self.line_at(offset);
        let start = self.line_start(line).min(offset);
        text[start..offset].graphemes(true).count()
    }

    /// Byte offset of grapheme column `col` on line `i`, clamped to the line
    /// end. O(line length).
    ///
    /// Out-of-range `i` returns `len()`.
    pub fn offset_at_grapheme_col(&self, text: &str, i: usize, col: usize) -> usize {
        debug_assert_eq!(
            text.len(),
            self.len,
            "LineIndex::offset_at_grapheme_col called with text it was not built from"
        );
        let Some((start, end)) = self.line_range(i) else {
            return self.len;
        };
        if col == 0 {
            return start;
        }
        match text[start..end].grapheme_indices(true).nth(col) {
            Some((rel, _)) => start + rel,
            None => end,
        }
    }
}

/// Previous grapheme-cluster boundary strictly before `offset`.
///
/// Scans at most the current line (a grapheme cluster never spans `\n`, with
/// `\r\n` handled explicitly below), so this is O(line length), not
/// O(document) — and the scan is further capped at [`SCAN_CAP`] bytes. At a
/// line start it returns the offset of the preceding `\n`. At offset 0 it
/// returns 0.
pub fn prev_grapheme(text: &str, index: &LineIndex, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset.min(text.len()));
    if offset == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let line = index.line_at(offset);
    let start = index.line_start(line).min(offset);

    if offset == start {
        // We are at a line start: the boundary before us is the `\n` that ended
        // the previous line — or, for a CRLF, the `\r` that begins that cluster.
        if offset >= 2 && bytes[offset - 1] == b'\n' && bytes[offset - 2] == b'\r' {
            return offset - 2;
        }
        return offset - 1;
    }

    // Scan backwards within the line, capped. Flooring the window start to a
    // `char` boundary is exact: no real cluster is 1 KiB long, so the window
    // never begins inside one.
    let window_start = if offset - start > SCAN_CAP {
        floor_char_boundary(text, offset - SCAN_CAP)
    } else {
        start
    };
    match text[window_start..offset].grapheme_indices(true).next_back() {
        Some((rel, _)) => window_start + rel,
        None => window_start,
    }
}

/// Next grapheme-cluster boundary strictly after `offset`. At a line end it
/// returns the offset just past the `\n`. At the document end it returns
/// `index.len()`.
///
/// Same O(line length) / [`SCAN_CAP`] bound as [`prev_grapheme`].
pub fn next_grapheme(text: &str, index: &LineIndex, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset.min(text.len()));
    if offset >= text.len() {
        return text.len();
    }
    let bytes = text.as_bytes();
    // `\r\n` is the one grapheme cluster that spans a line break.
    if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
        return offset + 2;
    }
    let line = index.line_at(offset);
    let end = index.line_end(line).max(offset);
    if offset >= end {
        // Sitting on the `\n` that ends this line (the document-end case was
        // handled above): step just past it, onto the next line's first byte.
        return offset + 1;
    }

    let window_end = if end - offset > SCAN_CAP {
        floor_char_boundary(text, offset + SCAN_CAP)
    } else {
        end
    };
    match text[offset..window_end].graphemes(true).next() {
        Some(g) => offset + g.len(),
        None => end,
    }
}

/// Largest `i <= at` that is a `char` boundary of `text`.
///
/// `str::floor_char_boundary` is still unstable, so this is the hand-rolled
/// equivalent. UTF-8 characters are at most 4 bytes, so this steps back at most
/// 3 times.
#[inline]
fn floor_char_boundary(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let mut i = at;
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// UTF-16 code-unit length of `s`, without decoding `char`s.
#[inline]
fn utf16_len(s: &str) -> usize {
    let mut n = 0usize;
    for &b in s.as_bytes() {
        if b & 0xC0 != 0x80 {
            n += 1;
            if b >= 0xF0 {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    /// Wall-clock budgets are guards against an algorithm going quadratic, not
    /// benchmarks. Unoptimised they mean little, and a machine with something
    /// else running on it is not a fair judge — so debug builds get room rather
    /// than a test that fails for reasons that have nothing to do with the code.
    fn budget(release_ms: u128) -> u128 {
        if cfg!(debug_assertions) {
            release_ms * 10
        } else {
            release_ms
        }
    }

    use super::*;
    use std::time::{Duration, Instant};

    // ---- naive reference implementations ---------------------------------

    /// The lines of `text`, matching `Note::lines`.
    fn naive_lines(text: &str) -> Vec<&str> {
        text.split('\n').collect()
    }

    /// Verbatim port of the app's original `Note::line_index_at`.
    fn naive_line_at(text: &str, offset: usize) -> usize {
        let offset = offset.min(text.len());
        let lines = naive_lines(text);
        let last = lines.len().saturating_sub(1);
        let mut start = 0usize;
        for (i, line) in lines.into_iter().enumerate() {
            let content_end = start + line.len();
            let span_end = if i < last { content_end } else { text.len() };
            if offset <= span_end {
                return i;
            }
            start = content_end + 1;
        }
        last
    }

    /// Verbatim port of the app's original `Note::line_byte_range`.
    fn naive_line_range(text: &str, line_idx: usize) -> Option<(usize, usize)> {
        let lines = naive_lines(text);
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

    /// Verbatim port of the app's original `offset_to_utf16`.
    fn naive_to_utf16(text: &str, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in text.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    /// Verbatim port of the app's original `offset_from_utf16`.
    fn naive_from_utf16(text: &str, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in text.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    /// Every grapheme boundary of the whole document, ascending, including 0
    /// and `len`.
    fn naive_boundaries(text: &str) -> Vec<usize> {
        let mut v: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).collect();
        v.push(text.len());
        v
    }

    // ---- fixtures ---------------------------------------------------------

    /// Documents that between them hit every edge case the index has to
    /// survive. Kept small so the O(n^2) naive cross-checks stay instant.
    fn fixtures() -> Vec<String> {
        vec![
            // 0: empty
            String::new(),
            // 1: single char, no newline
            "a".to_string(),
            // 2: one line, no trailing newline
            "hello world".to_string(),
            // 3: classic three lines, no trailing newline
            "aa\nbb\ncc".to_string(),
            // 4: trailing newline => final empty line
            "aa\nbb\n".to_string(),
            // 5: leading newline => first line empty
            "\nfoo".to_string(),
            // 6: only newlines
            "\n\n\n\n".to_string(),
            // 7: a single newline
            "\n".to_string(),
            // 8: blank line in the middle
            "one\n\nthree".to_string(),
            // 9: CRLF line endings
            "aa\r\nbb\r\ncc".to_string(),
            // 10: CRLF with trailing CRLF
            "x\r\ny\r\n".to_string(),
            // 11: lone carriage returns
            "a\rb\r\nc".to_string(),
            // 12: 2-byte UTF-8
            "ααα\nββ\nγ".to_string(),
            // 13: 3-byte UTF-8 (CJK)
            "日本語\nテスト\n".to_string(),
            // 14: 4-byte UTF-8 (astral => surrogate pairs)
            "𝄞𝄞\n𝕳𝖊𝖑𝖑𝖔\n".to_string(),
            // 15: emoji with ZWJ family cluster
            "hi 👩‍👩‍👧‍👦 there\nnext 👨‍💻 line".to_string(),
            // 16: combining marks and a flag (regional indicator pair)
            "e\u{0301}cole 🇯🇵\nq\u{0308}\n".to_string(),
            // 17: skin-tone modifier + variation selector
            "👍🏽 ok\n❤️\u{fe0f} yes".to_string(),
            // 18: mixed everything, last line empty
            "a\nβ\n日\n𝄞\n👍🏽\n".to_string(),
            // 19: markdown-ish real content with a separator line
            "# Title\n\nsome *body* text\n\n---\n\n- item ώ\n- item 🌍\n".to_string(),
            // 20: many short lines
            (0..40).map(|i| format!("line {i}\n")).collect::<String>(),
            // 21: one long-ish line well under the scan cap
            "x".repeat(300),
            // 22: trailing spaces / tabs
            "  indented\t\n\ttabbed  \n".to_string(),
        ]
    }

    // ---- correctness ------------------------------------------------------

    #[test]
    fn line_at_matches_naive_for_every_offset() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            for offset in 0..=text.len() {
                assert_eq!(
                    index.line_at(offset),
                    naive_line_at(text, offset),
                    "fixture {f} offset {offset} in {text:?}"
                );
            }
            // Past the end clamps to the last line.
            for extra in 1..8 {
                assert_eq!(
                    index.line_at(text.len() + extra),
                    index.line_count() - 1,
                    "fixture {f} past-end +{extra}"
                );
            }
        }
    }

    #[test]
    fn line_count_matches_naive() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            assert_eq!(
                index.line_count(),
                text.split('\n').count(),
                "fixture {f} {text:?}"
            );
            assert!(index.line_count() >= 1);
            assert_eq!(index.len(), text.len());
            assert_eq!(index.is_empty(), text.is_empty());
        }
        assert_eq!(LineIndex::new("").line_count(), 1);
    }

    #[test]
    fn line_ranges_match_naive() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            for i in 0..index.line_count() {
                let expected = naive_line_range(text, i).unwrap();
                assert_eq!(index.line_range(i), Some(expected), "fixture {f} line {i}");
                assert_eq!(index.line_start(i), expected.0, "fixture {f} line {i}");
                assert_eq!(index.line_end(i), expected.1, "fixture {f} line {i}");
                // The slice must be a valid str slice and equal the naive line.
                assert_eq!(&text[expected.0..expected.1], naive_lines(text)[i]);
            }
            // Out of range.
            for i in index.line_count()..index.line_count() + 4 {
                assert_eq!(index.line_range(i), None, "fixture {f} oob line {i}");
                assert_eq!(index.line_start(i), text.len());
                assert_eq!(index.line_end(i), text.len());
            }
        }
    }

    #[test]
    fn line_at_agrees_with_line_range() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            for offset in 0..=text.len() {
                let line = index.line_at(offset);
                let (s, e) = index.line_range(line).unwrap();
                assert!(
                    s <= offset && offset <= e,
                    "fixture {f}: offset {offset} not inside line {line} range {s}..{e}"
                );
            }
        }
    }

    #[test]
    fn utf16_matches_naive_and_round_trips() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            assert_eq!(
                index.total_utf16(),
                text.encode_utf16().count(),
                "fixture {f} total_utf16"
            );

            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) {
                    continue;
                }
                let u = index.to_utf16(text, offset);
                assert_eq!(u, naive_to_utf16(text, offset), "fixture {f} offset {offset}");
                assert_eq!(
                    u,
                    text[..offset].encode_utf16().count(),
                    "fixture {f} offset {offset} vs encode_utf16"
                );
                assert_eq!(
                    index.from_utf16(text, u),
                    offset,
                    "fixture {f} round trip at {offset}"
                );
            }

            // Every UTF-16 offset, including ones inside a surrogate pair,
            // must agree with the implementation this replaces.
            for u in 0..=index.total_utf16() {
                assert_eq!(
                    index.from_utf16(text, u),
                    naive_from_utf16(text, u),
                    "fixture {f} from_utf16({u})"
                );
            }
        }
    }

    #[test]
    fn grapheme_walk_matches_whole_document_walk() {
        for (f, text) in fixtures().iter().enumerate() {
            // Every fixture's lines are far shorter than SCAN_CAP.
            let index = LineIndex::new(text);
            let expected = naive_boundaries(text);

            // Forward from 0.
            let mut forward = vec![0usize];
            let mut at = 0usize;
            while at < text.len() {
                let next = next_grapheme(text, &index, at);
                assert!(next > at, "fixture {f}: next_grapheme stalled at {at}");
                at = next;
                forward.push(at);
            }
            assert_eq!(forward, expected, "fixture {f} forward walk in {text:?}");

            // Backward from len.
            let mut backward = vec![text.len()];
            let mut at = text.len();
            while at > 0 {
                let prev = prev_grapheme(text, &index, at);
                assert!(prev < at, "fixture {f}: prev_grapheme stalled at {at}");
                at = prev;
                backward.push(at);
            }
            backward.reverse();
            assert_eq!(backward, expected, "fixture {f} backward walk in {text:?}");
        }
    }

    #[test]
    fn grapheme_ends_saturate() {
        for text in fixtures() {
            let index = LineIndex::new(&text);
            assert_eq!(prev_grapheme(&text, &index, 0), 0);
            assert_eq!(next_grapheme(&text, &index, text.len()), text.len());
            assert_eq!(next_grapheme(&text, &index, text.len() + 99), text.len());
        }
    }

    #[test]
    fn grapheme_col_round_trips() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            for line in 0..index.line_count() {
                let (s, e) = index.line_range(line).unwrap();
                let cols = text[s..e].graphemes(true).count();
                for col in 0..=cols {
                    let offset = index.offset_at_grapheme_col(text, line, col);
                    assert!(s <= offset && offset <= e, "fixture {f} line {line} col {col}");
                    assert_eq!(
                        index.grapheme_col(text, offset),
                        col,
                        "fixture {f} line {line} col {col} -> offset {offset}"
                    );
                }
                // Past the last column clamps to the line end.
                assert_eq!(index.offset_at_grapheme_col(text, line, cols + 7), e);
            }
            // Out-of-range line.
            assert_eq!(
                index.offset_at_grapheme_col(text, index.line_count() + 3, 0),
                text.len()
            );
        }
    }

    #[test]
    fn grapheme_col_matches_forward_walk() {
        for (f, text) in fixtures().iter().enumerate() {
            let index = LineIndex::new(text);
            for line in 0..index.line_count() {
                let (s, e) = index.line_range(line).unwrap();
                let mut col = 0usize;
                let mut at = s;
                while at < e {
                    assert_eq!(index.grapheme_col(text, at), col, "fixture {f} line {line}");
                    assert_eq!(index.offset_at_grapheme_col(text, line, col), at);
                    at = next_grapheme(text, &index, at);
                    col += 1;
                }
            }
        }
    }

    #[test]
    fn nothing_panics_on_bad_offsets() {
        for text in fixtures() {
            let index = LineIndex::new(&text);
            let len = text.len();
            let mut probes: Vec<usize> = (0..=len).collect();
            probes.extend([len + 1, len + 2, len + 1000, usize::MAX / 2, usize::MAX]);
            for &offset in &probes {
                // Includes offsets in the middle of multi-byte characters.
                let _ = index.line_at(offset);
                let _ = index.line_start(offset.min(1_000_000));
                let _ = index.line_end(offset.min(1_000_000));
                let _ = index.line_range(offset.min(1_000_000));
                let _ = index.to_utf16(&text, offset);
                let _ = index.from_utf16(&text, offset);
                let _ = index.grapheme_col(&text, offset);
                let _ = prev_grapheme(&text, &index, offset);
                let _ = next_grapheme(&text, &index, offset);
            }
            for line in 0..index.line_count() + 3 {
                for col in [0usize, 1, 7, 10_000, usize::MAX] {
                    let _ = index.offset_at_grapheme_col(&text, line, col);
                }
            }
        }
    }

    #[test]
    fn mid_character_offsets_floor_to_char_boundary() {
        let text = "𝄞ab";
        let index = LineIndex::new(text);
        for bad in 1..4 {
            assert!(!text.is_char_boundary(bad));
            assert_eq!(index.to_utf16(text, bad), 0);
            assert_eq!(index.grapheme_col(text, bad), 0);
            assert_eq!(prev_grapheme(text, &index, bad), 0);
            assert_eq!(next_grapheme(text, &index, bad), 4);
        }
    }

    #[test]
    fn scan_cap_still_finds_boundaries_on_a_huge_single_line() {
        // One line far longer than SCAN_CAP, with a multi-byte cluster at the
        // point we probe, to prove the capped window is still exact.
        let mut text = "a".repeat(SCAN_CAP * 8);
        let probe = text.len();
        text.push_str("👍🏽");
        let tail = text.len();
        text.push_str(&"b".repeat(SCAN_CAP * 8));
        let index = LineIndex::new(&text);

        assert_eq!(index.line_count(), 1);
        assert_eq!(next_grapheme(&text, &index, probe), tail);
        assert_eq!(prev_grapheme(&text, &index, tail), probe);
        assert_eq!(prev_grapheme(&text, &index, probe), probe - 1);
        assert_eq!(next_grapheme(&text, &index, tail), tail + 1);
    }

    #[test]
    fn documented_tie_break_examples() {
        let text = "aa\nbb\ncc";
        let index = LineIndex::new(text);
        assert_eq!(index.line_at(0), 0);
        assert_eq!(index.line_at(1), 0);
        assert_eq!(index.line_at(2), 0); // ON the `\n` ending line 0
        assert_eq!(index.line_at(3), 1);
        assert_eq!(index.line_at(5), 1); // ON the `\n` ending line 1
        assert_eq!(index.line_at(6), 2);
        assert_eq!(index.line_at(index.len()), 2);
    }

    // ---- splice -----------------------------------------------------------
    //
    // The contract is exact equivalence with a rebuild, so every test here ends
    // in the same assertion: `index == LineIndex::new(text)`.

    /// Apply `start..start + removed.len()` -> `inserted` to `text` with both
    /// `splice` and a rebuild, assert they agree, and return the new text.
    #[track_caller]
    fn check_splice(text: &str, start: usize, removed: &str, inserted: &str) -> String {
        assert_eq!(
            &text[start..start + removed.len()],
            removed,
            "bad test case: {removed:?} is not what sits at {start} in {text:?}"
        );
        let mut index = LineIndex::new(text);
        let mut new_text = text.to_string();
        new_text.replace_range(start..start + removed.len(), inserted);

        index.splice(&new_text, start, removed, inserted);
        assert_eq!(
            index,
            LineIndex::new(&new_text),
            "splice(start {start}, removed {removed:?}, inserted {inserted:?}) on {text:?}"
        );
        new_text
    }

    #[test]
    fn splice_targeted_cases() {
        // Insert a plain char mid-line: line count unchanged.
        let t = check_splice("aa\nbb\ncc", 4, "", "X");
        assert_eq!(t, "aa\nbXb\ncc");
        assert_eq!(LineIndex::new(&t).line_count(), 3);

        // Insert a `\n`: line count grows.
        let t = check_splice("aa\nbb\ncc", 4, "", "\n");
        assert_eq!(t, "aa\nb\nb\ncc");
        assert_eq!(LineIndex::new(&t).line_count(), 4);

        // Delete a `\n`: line count shrinks.
        let t = check_splice("aa\nbb\ncc", 2, "\n", "");
        assert_eq!(t, "aabb\ncc");
        assert_eq!(LineIndex::new(&t).line_count(), 2);

        // Insert a multi-line string.
        let t = check_splice("aa\nbb\ncc", 3, "", "1\n2\n3\n");
        assert_eq!(t, "aa\n1\n2\n3\nbb\ncc");
        assert_eq!(LineIndex::new(&t).line_count(), 6);

        // Delete a range spanning several lines.
        let t = check_splice("aa\nbb\ncc\ndd\nee", 1, "a\nbb\ncc\nd", "");
        assert_eq!(t, "ad\nee");

        // Replace a multi-line range with a multi-line string.
        check_splice("aa\nbb\ncc\ndd", 1, "a\nbb\nc", "X\nY\nZ\nW");
        // ...and with a string containing fewer newlines than it replaces.
        check_splice("aa\nbb\ncc\ndd", 1, "a\nbb\nc", "q");
        // ...and with more.
        check_splice("aa\nbb\ncc\ndd", 1, "a\nb", "\n\n\n\n");

        // At offset 0, inserting and deleting, with and without a newline.
        check_splice("aa\nbb", 0, "", "z");
        check_splice("aa\nbb", 0, "", "z\n");
        check_splice("aa\nbb", 0, "a", "");
        check_splice("\nbb", 0, "\n", "");
        check_splice("aa\nbb", 0, "aa\nb", "hello");

        // At the very end of the document.
        check_splice("aa\nbb", 5, "", "z");
        check_splice("aa\nbb", 5, "", "\n");
        check_splice("aa\nbb\n", 6, "", "z");
        check_splice("aa\nbb\n", 6, "", "\nmore\n");

        // On the last line of a document with no trailing newline.
        check_splice("aa\nbb\ncc", 7, "c", "");
        check_splice("aa\nbb\ncc", 6, "cc", "d\ne\nf");

        // Delete everything.
        let t = check_splice("aa\nbb\ncc\n", 0, "aa\nbb\ncc\n", "");
        assert_eq!(t, "");
        assert_eq!(LineIndex::new(&t).line_count(), 1);

        // Insert into an empty document.
        check_splice("", 0, "", "hello");
        check_splice("", 0, "", "\n");
        check_splice("", 0, "", "a\nb\nc");

        // No-op splices, on an empty and a non-empty document.
        check_splice("", 0, "", "");
        check_splice("aa\nbb\ncc", 4, "", "");
        check_splice("aa\nbb\ncc", 0, "", "");
        check_splice("aa\nbb\ncc", 8, "", "");

        // Multi-byte content: the UTF-16 table has to move too.
        check_splice("日本語\nテスト\n", 9, "", "🌍");
        check_splice("日本語\nテスト\n", 0, "日", "");
        check_splice("a\nβ\n日\n𝄞\n👍🏽\n", 4, "\n日\n", "é\n");
        check_splice("👩‍👩‍👧‍👦 fam\nnext", 0, "", "𝄞\n𝄞");
        check_splice("𝄞𝄞\n𝕳𝖊", 8, "\n", "");

        // Consecutive newlines, added and removed in bulk.
        check_splice("\n\n\n\n", 2, "\n\n", "");
        check_splice("\n\n\n\n", 2, "", "\n\n\n");
        check_splice("\n\n\n\n", 0, "\n\n\n\n", "x");
    }

    #[test]
    fn splice_repeated_typing_stays_exact() {
        // The real usage pattern: a long run of splices on one live index,
        // never rebuilding, must never drift.
        let mut text = String::new();
        let mut index = LineIndex::new(&text);
        for (i, ch) in "the quick\nbrown 🦊\njumps 日本\n".chars().cycle().take(400).enumerate() {
            let at = if i % 3 == 0 { 0 } else { text.len() };
            let s = ch.to_string();
            text.insert_str(at, &s);
            index.splice(&text, at, "", &s);
            assert_eq!(index, LineIndex::new(&text), "insert step {i}");
        }
        // ...and back down to nothing, deleting from the front.
        let mut step = 0;
        while !text.is_empty() {
            let end = next_grapheme(&text, &index, 0).max(1);
            let removed = text[..end].to_string();
            text.replace_range(..end, "");
            index.splice(&text, 0, &removed, "");
            assert_eq!(index, LineIndex::new(&text), "delete step {step}");
            step += 1;
        }
        assert_eq!(index, LineIndex::new(""));
    }

    #[test]
    fn splice_matches_rebuild_under_random_edits() {
        const EDITS: usize = 5_000;
        /// Pieces to build insertions from: empty, ASCII, newlines, 2-, 3- and
        /// 4-byte characters, and a CRLF.
        const ALPHABET: [&str; 12] = [
            "", "a", "z ", "hello ", "\n", "\n\n", "é", "ώ", "日", "🌍", "\r\n", "x\ny\nz",
        ];
        let seeds = [
            "",
            "one line",
            "aa\nbb\ncc\ndd\nee",
            "日本語 é 👩‍👩‍👧‍👦\nmixed 🌍 line ώ\n𝄞 last",
            "trailing\nnewline\n",
            "\n\n\n\n\n\n\n\n",
        ];

        for (f, seed) in seeds.iter().enumerate() {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((f as u64 + 1) * 0x0123_4567_89AB_CDEF));
            let mut reference = seed.to_string();
            let mut index = LineIndex::new(&reference);

            for step in 0..EDITS {
                // Keep the document in a range where a full rebuild per edit is
                // still instant, by leaning on deletion once it gets large.
                let big = reference.len() > 1_200;

                let start = floor_char_boundary(&reference, rng.below(reference.len() + 1));
                let room = reference.len() - start;
                let want = match rng.below(10) {
                    0..=3 if !big => 0,
                    4..=6 => 1,
                    7 | 8 => rng.below(8),
                    _ => rng.below(room + 1),
                };
                let end = floor_char_boundary(&reference, start + want.min(room));

                let mut inserted = String::new();
                if !(big && rng.below(4) != 0) {
                    for _ in 0..=rng.below(3) {
                        inserted.push_str(ALPHABET[rng.below(ALPHABET.len())]);
                    }
                }

                let removed = reference[start..end].to_string();
                reference.replace_range(start..end, &inserted);
                index.splice(&reference, start, &removed, &inserted);

                let rebuilt = LineIndex::new(&reference);
                assert_eq!(
                    index, rebuilt,
                    "fixture {f} step {step}: start {start}, removed {removed:?}, \
                     inserted {inserted:?}, now {} bytes",
                    reference.len()
                );

                if step % 16 == 0 {
                    let len = reference.len();
                    for probe in [0, len / 3, len / 2, len.saturating_sub(1), len, len + 5] {
                        assert_eq!(index.line_at(probe), rebuilt.line_at(probe));
                        let b = floor_char_boundary(&reference, probe.min(len));
                        assert_eq!(
                            index.to_utf16(&reference, b),
                            rebuilt.to_utf16(&reference, b)
                        );
                        assert_eq!(
                            index.from_utf16(&reference, probe),
                            rebuilt.from_utf16(&reference, probe)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn splice_falls_back_on_inconsistent_arguments() {
        let text = "aa\nbb\ncc";
        let bad: &[(&str, usize, &str, &str)] = &[
            // Lengths do not add up: claims one byte removed, two inserted.
            ("aa\nXX\ncc", 3, "b", "XX"),
            // Claims nothing was removed when two bytes were.
            ("aa\ncc", 3, "", ""),
            // `start` past the end of the new text.
            ("aa\nbb\ncc", 99, "", ""),
            // `inserted` runs past the end of the new text.
            ("aa\nbb\ncc", 7, "", "long tail"),
            // Right lengths, but `inserted` is not what is actually there.
            ("aa\nXb\ncc", 3, "b", "Y"),
            // Right lengths, but `removed` claims a `\n` the index never had.
            ("aa\nXb\ncc", 3, "\n", "X"),
            // Wildly wrong everything.
            ("", 4, "zzz", "qqq"),
        ];
        for &(new_text, start, removed, inserted) in bad {
            let mut index = LineIndex::new(text);
            index.splice(new_text, start, removed, inserted);
            assert_eq!(
                index,
                LineIndex::new(new_text),
                "expected a rebuild fallback for ({start}, {removed:?}, {inserted:?})"
            );
        }

        // A `start` inside a multi-byte character also falls back rather than
        // panicking on the slice.
        for start in 1..4 {
            let mut index = LineIndex::new("𝄞ab");
            index.splice("𝄞ab", start, "", "");
            assert_eq!(index, LineIndex::new("𝄞ab"));
        }
    }

    // ---- performance ------------------------------------------------------
    //
    // Thresholds carry ~10x headroom so a loaded machine will not flake, while
    // an accidental O(document) regression (which would be seconds, not
    // milliseconds, at 10 MB) still fails loudly. Run under `--release` or a
    // test profile with `opt-level = 2`; a debug measurement is meaningless.

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// ~10 MB / ~200,000 lines: twenty years of daily note-taking.
    fn big_doc() -> &'static str {
        static DOC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DOC.get_or_init(|| {
            let mut rng = Rng(0x5DEECE66D);
            let mut s = String::with_capacity(11 << 20);
            for i in 0..200_000usize {
                match i % 17 {
                    0 => s.push_str("# 2026-08-05 standup notes ώ\n"),
                    3 => s.push_str("- shipped the index; caret is instant now 👍🏽 ok\n"),
                    5 => s.push_str("  日本語のメモ、長い行になるかもしれない\n"),
                    7 => s.push('\n'),
                    9 => s.push_str("---\n"),
                    11 => s.push_str("family 👩‍👩‍👧‍👦 dinner at 19:00, don't forget 𝄞\n"),
                    _ => {
                        s.push_str("the quick brown fox jumps over the lazy dog — filed #");
                        s.push_str(&(rng.next() % 1_000_000).to_string());
                        s.push('\n');
                    }
                }
            }
            s
        })
    }

    #[test]
    fn perf_build_index() {
        let text = big_doc();
        let t = Instant::now();
        let index = LineIndex::new(text);
        let elapsed = t.elapsed();
        eprintln!(
            "perf: LineIndex::new over {:.2} MB / {} lines -> {:?}",
            text.len() as f64 / (1 << 20) as f64,
            index.line_count(),
            elapsed
        );
        assert!(text.len() > 9 << 20, "fixture should be ~10 MB");
        assert!(index.line_count() > 190_000, "fixture should be ~200k lines");
        assert!(
            elapsed.as_millis() < budget(500),
            "LineIndex::new took {elapsed:?}, over the {} ms budget",
            budget(500)
        );
    }

    #[test]
    fn perf_line_at() {
        let text = big_doc();
        let index = LineIndex::new(text);
        let mut rng = Rng(0xDEADBEEF);
        let offsets: Vec<usize> = (0..100_000).map(|_| rng.below(index.len() + 1)).collect();

        let t = Instant::now();
        let mut sink = 0usize;
        for &o in &offsets {
            sink = sink.wrapping_add(index.line_at(o));
        }
        let elapsed = t.elapsed();
        eprintln!("perf: 100,000 line_at -> {elapsed:?} (checksum {sink})");
        assert!(
            elapsed.as_millis() < budget(200),
            "100,000 line_at took {elapsed:?}, over the {} ms budget",
            budget(200)
        );
    }

    #[test]
    fn perf_to_utf16() {
        let text = big_doc();
        let index = LineIndex::new(text);
        let mut rng = Rng(0xC0FFEE);
        let offsets: Vec<usize> = (0..10_000).map(|_| rng.below(index.len() + 1)).collect();

        let t = Instant::now();
        let mut sink = 0usize;
        for &o in &offsets {
            sink = sink.wrapping_add(index.to_utf16(text, o));
        }
        let elapsed = t.elapsed();
        eprintln!("perf: 10,000 to_utf16 -> {elapsed:?} (checksum {sink})");
        assert!(
            elapsed.as_millis() < budget(200),
            "10,000 to_utf16 took {elapsed:?}, over the {} ms budget",
            budget(200)
        );

        // The inverse must be just as cheap.
        let us: Vec<usize> = (0..10_000).map(|_| rng.below(index.total_utf16() + 1)).collect();
        let t = Instant::now();
        let mut sink = 0usize;
        for &u in &us {
            sink = sink.wrapping_add(index.from_utf16(text, u));
        }
        let elapsed = t.elapsed();
        eprintln!("perf: 10,000 from_utf16 -> {elapsed:?} (checksum {sink})");
        assert!(
            elapsed.as_millis() < budget(200),
            "10,000 from_utf16 took {elapsed:?}, over the {} ms budget",
            budget(200)
        );
    }

    #[test]
    fn perf_grapheme_steps() {
        let text = big_doc();
        let index = LineIndex::new(text);
        let mut rng = Rng(0xABCDEF01);
        let offsets: Vec<usize> = (0..50_000).map(|_| rng.below(index.len() + 1)).collect();

        let t = Instant::now();
        let mut sink = 0usize;
        for &o in &offsets {
            sink = sink.wrapping_add(prev_grapheme(text, &index, o));
            sink = sink.wrapping_add(next_grapheme(text, &index, o));
        }
        let elapsed = t.elapsed();
        eprintln!("perf: 100,000 prev/next_grapheme -> {elapsed:?} (checksum {sink})");
        assert!(
            elapsed.as_millis() < budget(200),
            "100,000 grapheme steps took {elapsed:?}, over the {} ms budget",
            budget(200)
        );
    }

    #[test]
    fn perf_splice_beats_rebuild() {
        let text = big_doc();
        let mut buf = text.to_string();
        let mut index = LineIndex::new(&buf);

        // Baseline: what a keystroke costs today, rebuilding from scratch.
        let t = Instant::now();
        let rebuilt = LineIndex::new(&buf);
        let rebuild = t.elapsed();
        assert_eq!(index, rebuilt);
        eprintln!(
            "perf: baseline LineIndex::new over {:.2} MB / {} lines -> {:?}",
            buf.len() as f64 / (1 << 20) as f64,
            index.line_count(),
            rebuild
        );

        // In debug, `splice` rebuilds the whole 10 MB index on every call to
        // check itself, so a timing here would measure the assertion, not the
        // code. Still run a few iterations for the correctness it buys.
        let debug = cfg!(debug_assertions);
        let iters = if debug { 4 } else { 2_000 };

        // Worst case: an edit near the very start, so every line after it moves.
        let head = 5usize;
        assert!(buf.is_char_boundary(head));
        let mut total = Duration::ZERO;
        for _ in 0..iters {
            buf.insert(head, 'x');
            let t = Instant::now();
            index.splice(&buf, head, "", "x");
            total += t.elapsed();
        }
        let at_start = total / iters as u32;

        // Best case: an edit at the end, where nothing after it needs moving.
        let mut total = Duration::ZERO;
        for _ in 0..iters {
            let at = buf.len();
            buf.push('y');
            let t = Instant::now();
            index.splice(&buf, at, "", "y");
            total += t.elapsed();
        }
        let at_end = total / iters as u32;

        eprintln!(
            "perf: {iters} splices at doc start -> {at_start:?} each; \
             at doc end -> {at_end:?} each (rebuild was {rebuild:?})"
        );
        assert_eq!(index, LineIndex::new(&buf), "splice drifted over {iters} edits");

        if debug {
            eprintln!("perf: skipping splice timing assertions in a debug build");
            return;
        }
        assert!(
            at_start < Duration::from_millis(1),
            "splice at the start of the document averaged {at_start:?}, expected well under 1ms"
        );
        assert!(
            at_end < Duration::from_millis(1),
            "splice at the end of the document averaged {at_end:?}, expected well under 1ms"
        );
    }

    #[test]
    fn big_doc_line_at_agrees_with_line_range() {
        // Spot-check the invariants on the real-size document too.
        let text = big_doc();
        let index = LineIndex::new(text);
        let mut rng = Rng(0x1234_5678);
        for _ in 0..20_000 {
            let offset = rng.below(index.len() + 1);
            let line = index.line_at(offset);
            let (s, e) = index.line_range(line).unwrap();
            assert!(s <= offset && offset <= e, "offset {offset} vs line {line} {s}..{e}");
            let b = floor_char_boundary(text, offset);
            assert_eq!(index.from_utf16(text, index.to_utf16(text, b)), b);
        }
        assert_eq!(index.total_utf16(), text.encode_utf16().count());
    }
}
