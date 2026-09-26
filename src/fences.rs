//! Which lines sit inside a fenced code block.
//!
//! The highlighter needs, for every line it draws, whether that line begins
//! inside a ``` fence. That is a fold over the whole document — and the renderer
//! only ever draws a screenful, so recomputing it per frame, or even per
//! keystroke, would put an O(document) scan back on the typing path.
//!
//! [`FenceMap`] caches the state per line and patches it after an edit:
//!
//! * the rows the edit replaced are spliced out and the new ones spliced in,
//!   which keeps every later entry aligned with its (renumbered) line;
//! * the walk then resumes at the edited line and **stops as soon as the
//!   recomputed state matches what was already cached** — from there on nothing
//!   downstream can have changed.
//!
//! Ordinary typing does not touch a fence, so the walk stops after one line.
//! Toggling a fence at the top of the document is the worst case and costs one
//! pass.
//!
//! The early exit is only sound because the splice keeps line numbering
//! correct; [`FenceMap::rebuild`] exists for the cases that renumber everything
//! (a note moved, the whole buffer replaced). Correctness here is checked by
//! differential tests: after any edit sequence, a spliced map must equal a map
//! built from scratch.

use crate::index::LineIndex;
use crate::markdown;

/// Per-line "does this line begin inside a fenced code block" state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FenceMap {
    open: Vec<bool>,
    /// The line the document's final unclosed fence *opens on*, when it ends
    /// inside one. Everything after it is code being written, not code that
    /// has been written. See [`FenceMap::in_closed_fence`].
    first_unclosed: Option<usize>,
}

impl FenceMap {
    /// Build from scratch. O(lines).
    pub fn new(text: &str, index: &LineIndex) -> Self {
        let mut map = Self {
            first_unclosed: None,
            open: Vec::with_capacity(index.line_count()),
        };
        map.rebuild(text, index);
        map
    }

    /// Whether `line` sits inside a fence that is actually **closed**.
    ///
    /// A fence you have opened and not yet closed runs to the end of the
    /// document, which is right for colouring — you can see the block forming
    /// as you type it — and wrong for anything structural. Segmentation asks
    /// this one, so typing ``` does not swallow every note below it until you
    /// type the closing fence.
    pub fn in_closed_fence(&self, line: usize) -> bool {
        self.is_open(line) && self.first_unclosed.is_none_or(|first| line < first)
    }

    /// Where the document's final, never-closed fence begins, if it ends inside
    /// one.
    ///
    /// Walks back over the trailing open run, so while a fence is open every
    /// keystroke costs a pass over it — 60 µs on a twenty-year note, against
    /// 36 ns when the fence is closed. Bounded by the same run the walk above
    /// already had to rewrite when the fence was opened, and not a freeze, but
    /// it is the one thing in this file that is not incremental. Making it so
    /// needs the differential test the other caches have: an optimisation whose
    /// failure mode is silent corruption earns one first.
    fn recompute_first_unclosed(&mut self, text: &str, index: &LineIndex) {
        self.first_unclosed = None;
        let Some(last) = index.line_count().checked_sub(1) else {
            return;
        };
        if !self.is_open(last) {
            return;
        }
        // The closing fence line is itself "inside" the fence it closes, so a
        // document ending on one ends closed.
        let (start, end) = index.line_range(last).unwrap_or((0, 0));
        if markdown::is_fence(&text[start..end]) {
            return;
        }
        // Back to the head of the trailing run: that is the fence still open.
        let mut first = last;
        while first > 0 && self.open[first - 1] {
            first -= 1;
        }
        self.first_unclosed = Some(first.saturating_sub(1));
    }

    /// True when line `i` begins inside a fenced code block — including one
    /// that was opened and never closed, which runs to the end of the document.
    /// That is what colouring wants; [`Self::in_closed_fence`] is what anything
    /// structural wants. Out-of-range lines are reported as outside, never
    /// panic.
    pub fn is_open(&self, line: usize) -> bool {
        self.open.get(line).copied().unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Discard everything and recompute. Use after the buffer was rewritten
    /// wholesale, where line numbering no longer corresponds to the cache.
    pub fn rebuild(&mut self, text: &str, index: &LineIndex) {
        self.open.clear();
        self.open.resize(index.line_count(), false);
        self.walk(text, index, 0, None);
        self.recompute_first_unclosed(text, index);
    }

    /// Patch after an edit that replaced the `removed_lines + 1` rows starting
    /// at `first_line` with `inserted_lines + 1` rows.
    ///
    /// `index` must already describe the text *after* the edit.
    pub fn splice(
        &mut self,
        text: &str,
        index: &LineIndex,
        first_line: usize,
        removed_lines: usize,
        inserted_lines: usize,
    ) {
        let line_count = index.line_count();
        if first_line >= line_count {
            // The edit landed past the end of what we know about; a rebuild is
            // both correct and cheap relative to getting this wrong.
            self.rebuild(text, index);
            return;
        }

        // The state at the *start* of `first_line` cannot be changed by an edit
        // inside that line, so it is the resume point.
        let resume = self.is_open(first_line);

        let replaced_end = first_line
            .saturating_add(removed_lines)
            .saturating_add(1)
            .min(self.open.len());
        if first_line <= replaced_end {
            self.open.splice(
                first_line..replaced_end,
                std::iter::repeat_n(resume, inserted_lines + 1),
            );
        }
        self.open.resize(line_count, false);

        // The rows just spliced in are placeholders, all `resume`. The walk may
        // only reconverge against rows it did not invent, so it is told where
        // the cache becomes trustworthy again: one past the last placeholder.
        let trusted_from = first_line
            .saturating_add(inserted_lines)
            .saturating_add(1);
        self.walk(text, index, first_line, Some(trusted_from));
        self.recompute_first_unclosed(text, index);
    }

    /// Recompute forward from `from`.
    ///
    /// `trusted_from` is the first line whose cached state was not invented by
    /// the splice that just ran. Past it the walk may stop as soon as the
    /// recomputed state reconverges with the cache; before it, reconverging
    /// would only mean agreeing with a placeholder. `None` never stops early.
    fn walk(&mut self, text: &str, index: &LineIndex, from: usize, trusted_from: Option<usize>) {
        let line_count = index.line_count();
        debug_assert_eq!(self.open.len(), line_count, "map must be sized to the text");

        let mut state = if from == 0 { false } else { self.is_open(from) };

        for i in from..line_count {
            self.open[i] = state;
            let (start, end) = index.line_range(i).unwrap_or((0, 0));
            if markdown::is_fence(&text[start..end]) {
                state = !state;
            }
            let reconverged = trusted_from.is_some_and(|trusted| i + 1 >= trusted.max(from + 1))
                && self.open.get(i + 1).copied() == Some(state);
            if reconverged {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(text: &str) -> Vec<bool> {
        let mut out = Vec::new();
        let mut state = false;
        for line in text.split('\n') {
            out.push(state);
            if markdown::is_fence(line) {
                state = !state;
            }
        }
        out
    }

    fn assert_matches_reference(text: &str, map: &FenceMap, context: &str) {
        let expected = reference(text);
        assert_eq!(
            map.open, expected,
            "{context}\n--- text ---\n{text:?}\n--- got ---\n{:?}",
            map.open
        );
    }

    #[test]
    fn new_matches_the_reference_on_fixtures() {
        for text in [
            "",
            "plain",
            "```\ncode\n```",
            "```rust\ncode\n```\nafter",
            "before\n```\nin\n```\nafter",
            "```\nunterminated",
            "~~~\ntilde\n~~~",
            "```\na\n~~~\nb\n```",
            "---\n```\nx\n```\n---",
            "a\n\n```\n\n```\n\nb",
            "````\nfour ticks\n````",
            "  ```\nindented fence\n  ```",
        ] {
            let index = LineIndex::new(text);
            assert_matches_reference(text, &FenceMap::new(text, &index), "new");
        }
    }

    /// The whole point of the cache: a patched map must be indistinguishable
    /// from one built from scratch, after any edit.
    #[test]
    fn splice_matches_a_rebuild_under_random_edits() {
        let seeds = [
            "alpha\nbravo\ncharlie",
            "```\ncode\n```\ntail",
            "# h\n\ntext\n\n```rust\nfn main() {}\n```\n\nmore",
            "```\n```\n```\n```",
            "",
            "one line only",
        ];
        // Multi-line inserts are the interesting case and the one this test
        // used to miss: nothing here inserted more than a single newline, so
        // the splice never left more than one invented row behind, and a walk
        // that reconverged against an invented row always happened to be right.
        let alphabet = [
            "a",
            "\n",
            "```",
            "~~~",
            "`",
            "x\ny",
            "",
            "  ```",
            "日",
            "🎉",
            "\n\n```md\n# Title\n\n---\n\ntext\n```",
            "```\ncode\n```\n\nafter\n",
            "\n\n\n\n",
            "one\ntwo\nthree\nfour\nfive",
            "```\n\n\n",
        ];

        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for seed in seeds {
            let mut text = seed.to_string();
            let mut index = LineIndex::new(&text);
            let mut map = FenceMap::new(&text, &index);

            for step in 0..2_000 {
                let start = if text.is_empty() {
                    0
                } else {
                    let mut at = (next() as usize) % (text.len() + 1);
                    while !text.is_char_boundary(at) {
                        at -= 1;
                    }
                    at
                };
                let max_remove = text.len() - start;
                let mut remove_len = if max_remove == 0 {
                    0
                } else {
                    (next() as usize) % (max_remove.min(6) + 1)
                };
                while !text.is_char_boundary(start + remove_len) {
                    remove_len += 1;
                }
                let inserted = alphabet[(next() as usize) % alphabet.len()];

                let removed = text[start..start + remove_len].to_string();
                let first_line = index.line_at(start);
                let removed_lines = removed.matches('\n').count();
                let inserted_lines = inserted.matches('\n').count();

                text.replace_range(start..start + remove_len, inserted);
                index.splice(&text, start, &removed, inserted);
                map.splice(&text, &index, first_line, removed_lines, inserted_lines);

                assert_matches_reference(
                    &text,
                    &map,
                    &format!("seed {seed:?} step {step} start {start} +{inserted:?} -{removed:?}"),
                );
            }
        }
    }

    #[test]
    fn splice_handles_edits_that_open_and_close_fences() {
        // Typing a fence at the very top must flip every following line.
        let mut text = "a\nb\nc\nd".to_string();
        let mut index = LineIndex::new(&text);
        let mut map = FenceMap::new(&text, &index);
        assert_eq!(map.open, vec![false, false, false, false]);

        let inserted = "```\n";
        text.insert_str(0, inserted);
        index.splice(&text, 0, "", inserted);
        map.splice(&text, &index, 0, 0, 1);
        assert_matches_reference(&text, &map, "after opening a fence at the top");
        assert!(map.is_open(1), "everything after the fence is inside it");

        // Removing it flips them all back.
        let removed = inserted.to_string();
        text.replace_range(0..removed.len(), "");
        index.splice(&text, 0, &removed, "");
        map.splice(&text, &index, 0, 1, 0);
        assert_matches_reference(&text, &map, "after removing the fence again");
    }

    #[test]
    fn rebuild_recovers_from_wholesale_replacement() {
        let mut text = "```\nx\n```".to_string();
        let mut index = LineIndex::new(&text);
        let mut map = FenceMap::new(&text, &index);

        text = "completely\ndifferent\n```\nnow\n```".to_string();
        index = LineIndex::new(&text);
        map.rebuild(&text, &index);
        assert_matches_reference(&text, &map, "rebuild");
    }

    #[test]
    fn out_of_range_queries_do_not_panic() {
        let text = "a\nb";
        let index = LineIndex::new(text);
        let map = FenceMap::new(text, &index);
        assert!(!map.is_open(999));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn splice_past_the_end_falls_back_to_a_rebuild() {
        let text = "a\n```\nb";
        let index = LineIndex::new(text);
        let mut map = FenceMap::new(text, &index);
        map.splice(text, &index, 999, 0, 0);
        assert_matches_reference(text, &map, "out-of-range splice");
    }

    #[test]
    fn typing_does_not_walk_the_whole_document() {
        // 60k lines, one fence pair near the top. Typing far below it must not
        // rescan: assert by timing, since the early exit is the whole point.
        let mut text = String::from("```\nfenced\n```\n");
        for i in 0..60_000 {
            text.push_str(&format!("line {i}\n"));
        }
        let mut index = LineIndex::new(&text);
        let mut map = FenceMap::new(&text, &index);

        let at = text.len() - 1;
        // Time only the fence walk: in a debug build the line index re-verifies
        // itself against a full rebuild on every splice, which would otherwise
        // be all this measured.
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..2_000 {
            text.insert(at, 'x');
            index.splice(&text, at, "", "x");
            let first_line = index.line_at(at);
            let started = std::time::Instant::now();
            map.splice(&text, &index, first_line, 0, 0);
            elapsed += started.elapsed();
        }
        eprintln!(
            "[fences] 2000 splices near the end of {} lines: {:?} ({:?} each)",
            index.line_count(),
            elapsed,
            elapsed / 2_000
        );
        assert_matches_reference(&text, &map, "after typing at the end");
        // A full walk would be ~60k line lookups per keystroke.
        let budget = if cfg!(debug_assertions) { 2_000 } else { 200 };
        assert!(
            elapsed.as_millis() < budget,
            "2000 splices took {elapsed:?}, over the {budget} ms budget"
        );
    }
}
