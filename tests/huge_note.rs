//! Performance guard for the document layer against a twenty-year corpus.
//!
//! ```sh
//! cargo test --release --test huge_note -- --nocapture
//! ```
//!
//! **Run it with `--release`.** Integration tests build in the `test` profile,
//! which is unoptimised by default; the numbers printed by a debug build are
//! not the numbers the app experiences, they are 15–25x worse. Every threshold
//! below is written as a *release* budget and multiplied by [`SLOWDOWN`] when
//! `debug_assertions` is on, so `cargo test` still passes unoptimised — it just
//! stops meaning anything.
//!
//! Iteration counts are likewise scaled down in debug builds (see [`scaled`]),
//! so no test here takes more than ~15 s unoptimised and nothing needs to be
//! `#[ignore]`d. If you add a heavier case that *would* blow past ~15 s in a
//! debug build, mark it `#[ignore]` rather than letting `cargo test` crawl.
//!
//! # What is actually being defended
//!
//! The app holds every note in one markdown buffer. After twenty years that
//! buffer is ~9 MB and ~214k lines (see `corpus_shape`). Nothing on the
//! keystroke path may be O(document):
//!
//! * `line_at`, `to_utf16`, `from_utf16` and the grapheme walkers must be
//!   sublinear — macOS IME calls the UTF-16 conversions on **every keystroke**.
//! * Highlighting must be per-viewport, never per-document. Test 6 prints the
//!   whole-document cost next to the screenful cost to make that obvious.
//! * `blocks()` is O(n) by design and the app caches it; test 7 exists purely
//!   to catch it turning quadratic.
//!
//! A failure here is a regression in asymptotics, not a slow machine: the
//! budgets carry roughly 10x headroom over measured release timings.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gravitynote::{corpus, images, index, markdown, note};

// ---------------------------------------------------------------------------
// Shared corpus
// ---------------------------------------------------------------------------

/// The 20-year document, generated once and shared by every test in this file.
fn document() -> &'static str {
    static DOC: OnceLock<String> = OnceLock::new();
    DOC.get_or_init(|| {
        let started = Instant::now();
        let text = corpus::generate(corpus::CorpusSpec::twenty_years());
        eprintln!(
            "[corpus] generated {} bytes in {:.1} ms",
            text.len(),
            ms(started.elapsed())
        );
        text
    })
}

// ---------------------------------------------------------------------------
// Timing helpers
// ---------------------------------------------------------------------------

/// How much slower an unoptimised build is assumed to be. Generous on purpose:
/// this file is a guard against algorithmic regressions, not a benchmark.
const SLOWDOWN: f64 = if cfg!(debug_assertions) { 25.0 } else { 1.0 };

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Scale an iteration count down for debug builds so nothing here crawls.
fn scaled(n: usize) -> usize {
    if cfg!(debug_assertions) {
        (n / 10).max(1)
    } else {
        n
    }
}

/// Assert `elapsed` fits a *release* budget, widened for debug builds.
fn assert_under(label: &str, elapsed: Duration, release_budget_ms: f64) {
    let budget = release_budget_ms * SLOWDOWN;
    assert!(
        ms(elapsed) < budget,
        "{label}: {:.2} ms exceeds the {budget:.0} ms budget \
         (release budget {release_budget_ms:.0} ms x{SLOWDOWN} for this profile). \
         This is an asymptotic regression, not a slow machine.",
        ms(elapsed)
    );
}

/// Small deterministic xorshift so "random" offsets are reproducible across runs.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// A random byte offset that is a valid UTF-8 char boundary.
    fn offset_in(&mut self, text: &str) -> usize {
        let mut o = self.below(text.len());
        while o > 0 && !text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }
}

// ---------------------------------------------------------------------------
// 0. Shape
// ---------------------------------------------------------------------------

#[test]
fn corpus_shape() {
    let doc = document();
    let s = corpus::stats(doc);
    eprintln!("--- corpus ------------------------------------------------");
    eprintln!(
        "  bytes  {:>12}  ({:.2} MB)",
        s.bytes,
        s.bytes as f64 / (1024.0 * 1024.0)
    );
    eprintln!("  lines  {:>12}", s.lines);
    eprintln!("  notes  {:>12}", s.notes);
    eprintln!("  chars  {:>12}", s.chars);
    eprintln!(
        "  profile {:>11}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    eprintln!("-----------------------------------------------------------");
    if cfg!(debug_assertions) {
        eprintln!("  NOTE: unoptimised build — run with `cargo test --release --test huge_note`");
    }

    assert_eq!(s.notes, 36_500, "20 years x 5 notes a day");
    assert!(
        s.bytes > 8 * 1024 * 1024,
        "corpus shrank: {} bytes",
        s.bytes
    );
    assert!(s.lines > 150_000, "corpus shrank: {} lines", s.lines);
    // Multi-byte content must survive into the corpus, or every offset path
    // below is only being tested on its ASCII fast path.
    assert!(
        s.chars < s.bytes,
        "corpus is pure ASCII — offset paths untested"
    );
}

// ---------------------------------------------------------------------------
// 1. Index construction
// ---------------------------------------------------------------------------

#[test]
fn index_build_is_fast() {
    let doc = document();

    let started = Instant::now();
    let idx = index::LineIndex::new(doc);
    let elapsed = started.elapsed();

    let lines = idx.line_count();
    eprintln!(
        "[index_build] LineIndex::new over {} bytes / {lines} lines: {:.2} ms",
        doc.len(),
        ms(elapsed)
    );

    assert_eq!(
        lines,
        doc.split('\n').count(),
        "line_count disagrees with split"
    );
    // The index is rebuilt on load and after structural edits; one linear pass
    // over 9 MB should be a handful of milliseconds.
    assert_under("index_build", elapsed, 300.0);

    // line_start / line_end / line_range must agree with each other.
    for i in [0, 1, lines / 3, lines / 2, lines - 2, lines - 1] {
        let (s, e) = idx
            .line_range(i)
            .unwrap_or_else(|| panic!("line {i} is out of range"));
        assert_eq!(
            s,
            idx.line_start(i),
            "line_start disagrees with line_range at {i}"
        );
        assert_eq!(
            e,
            idx.line_end(i),
            "line_end disagrees with line_range at {i}"
        );
        assert!(s <= e && e <= doc.len(), "bad range at line {i}: {s}..{e}");
        assert!(!doc[s..e].contains('\n'), "line {i} spans a newline");
    }
}

// ---------------------------------------------------------------------------
// 2. Offset -> line
// ---------------------------------------------------------------------------

#[test]
fn line_at_is_sublinear() {
    let doc = document();
    let idx = index::LineIndex::new(doc);
    let n = scaled(100_000);

    let mut rng = Rng::new(0xA11CE);
    let offsets: Vec<usize> = (0..n).map(|_| rng.offset_in(doc)).collect();

    let started = Instant::now();
    let mut acc = 0usize;
    for &o in &offsets {
        acc += idx.line_at(o);
    }
    let elapsed = started.elapsed();
    std::hint::black_box(acc);

    eprintln!(
        "[line_at] {n} lookups: {:.2} ms ({:.3} us each)",
        ms(elapsed),
        ms(elapsed) * 1000.0 / n as f64
    );

    // Correctness spot-check: the line reported must actually contain the offset.
    for &o in offsets.iter().take(200) {
        let l = idx.line_at(o);
        let (s, e) = idx
            .line_range(l)
            .unwrap_or_else(|| panic!("line {l} is out of range"));
        assert!(
            s <= o && o <= e,
            "line_at({o}) = {l}, whose range is {s}..{e}"
        );
    }

    // Binary search over 214k lines is ~18 comparisons; a linear scan would be
    // four orders of magnitude worse.
    assert_under("line_at", elapsed, 200.0);
}

// ---------------------------------------------------------------------------
// 3. UTF-16 conversion — the keystroke-critical one
// ---------------------------------------------------------------------------

/// macOS hands the IME UTF-16 offsets and asks for them back on **every**
/// keystroke, selection change and marked-text update. If these are O(document)
/// the app is unusable long before twenty years are up.
#[test]
fn utf16_conversion_is_sublinear() {
    let doc = document();
    let idx = index::LineIndex::new(doc);
    let n = scaled(10_000);

    let total_u16 = idx.total_utf16();
    eprintln!("[utf16] total_utf16 = {total_u16} for {} bytes", doc.len());
    assert!(total_u16 > 0);
    assert!(
        total_u16 <= doc.len(),
        "UTF-16 length {total_u16} exceeds byte length {}",
        doc.len()
    );

    let mut rng = Rng::new(0xBEEF);
    let offsets: Vec<usize> = (0..n).map(|_| rng.offset_in(doc)).collect();

    let started = Instant::now();
    let mut acc = 0usize;
    for &o in &offsets {
        acc += idx.to_utf16(doc, o);
    }
    let to_elapsed = started.elapsed();
    std::hint::black_box(acc);

    let mut rng = Rng::new(0xF00D);
    let units: Vec<usize> = (0..n).map(|_| rng.below(total_u16)).collect();

    let started = Instant::now();
    let mut acc = 0usize;
    for &u in &units {
        acc += idx.from_utf16(doc, u);
    }
    let from_elapsed = started.elapsed();
    std::hint::black_box(acc);

    eprintln!(
        "[utf16] {n} to_utf16: {:.2} ms ({:.3} us each)",
        ms(to_elapsed),
        ms(to_elapsed) * 1000.0 / n as f64
    );
    eprintln!(
        "[utf16] {n} from_utf16: {:.2} ms ({:.3} us each)",
        ms(from_elapsed),
        ms(from_elapsed) * 1000.0 / n as f64
    );

    // Round-tripping must be exact, or the IME will place the caret wrongly.
    for &o in offsets.iter().take(500) {
        let u = idx.to_utf16(doc, o);
        assert_eq!(
            idx.from_utf16(doc, u),
            o,
            "utf16 round-trip broke at byte {o}"
        );
    }

    assert_under("to_utf16", to_elapsed, 250.0);
    assert_under("from_utf16", from_elapsed, 250.0);
}

// ---------------------------------------------------------------------------
// 4. Caret motion
// ---------------------------------------------------------------------------

#[test]
fn caret_motion_is_local() {
    let doc = document();
    let idx = index::LineIndex::new(doc);
    let n = scaled(100_000);

    // Start in the middle so neither direction runs off the end quickly.
    let mut offset = {
        let mut o = doc.len() / 2;
        while o > 0 && !doc.is_char_boundary(o) {
            o -= 1;
        }
        o
    };

    let started = Instant::now();
    for i in 0..n {
        offset = if i % 2 == 0 {
            index::next_grapheme(doc, &idx, offset)
        } else {
            index::prev_grapheme(doc, &idx, offset)
        };
    }
    let elapsed = started.elapsed();
    std::hint::black_box(offset);

    eprintln!(
        "[caret] {n} prev/next_grapheme: {:.2} ms ({:.3} us each)",
        ms(elapsed),
        ms(elapsed) * 1000.0 / n as f64
    );
    assert!(doc.is_char_boundary(offset), "caret left a char boundary");
    assert_under("caret_motion", elapsed, 200.0);

    // Column arithmetic runs on the same path (arrow up/down, click-to-caret).
    let mut rng = Rng::new(0xC0FFEE);
    let m = scaled(10_000);
    let offsets: Vec<usize> = (0..m).map(|_| rng.offset_in(doc)).collect();

    let started = Instant::now();
    let mut acc = 0usize;
    for &o in &offsets {
        acc += idx.grapheme_col(doc, o);
    }
    let col_elapsed = started.elapsed();
    std::hint::black_box(acc);

    let started = Instant::now();
    let mut acc = 0usize;
    for &o in &offsets {
        let line = idx.line_at(o);
        let col = idx.grapheme_col(doc, o);
        acc += idx.offset_at_grapheme_col(doc, line, col);
    }
    let round_elapsed = started.elapsed();
    std::hint::black_box(acc);

    eprintln!(
        "[caret] {m} grapheme_col: {:.2} ms; {m} col round-trips: {:.2} ms",
        ms(col_elapsed),
        ms(round_elapsed)
    );

    // A column round-trip must be stable. It need not return the *same* byte
    // offset — a random char boundary can sit inside a grapheme cluster (the
    // corpus contains 👍🏽 and 🇸🇪, which are several code points each), and the
    // caret is expected to snap to the cluster edge. What must hold is that
    // measuring the snapped offset yields the column we asked for, and that the
    // caret never escapes its line.
    for &o in offsets.iter().take(500) {
        let line = idx.line_at(o);
        let (ls, le) = idx
            .line_range(line)
            .unwrap_or_else(|| panic!("caret line {line} is out of range"));
        let col = idx.grapheme_col(doc, o);
        let snapped = idx.offset_at_grapheme_col(doc, line, col);
        assert!(
            (ls..=le).contains(&snapped),
            "offset_at_grapheme_col({line}, {col}) = {snapped} escaped line range {ls}..{le}"
        );
        assert!(
            doc.is_char_boundary(snapped),
            "snapped off a char boundary at {snapped}"
        );
        assert_eq!(
            idx.grapheme_col(doc, snapped),
            col,
            "grapheme column round-trip is unstable at byte {o} (line {line}, col {col})"
        );
    }

    assert_under("grapheme_col", col_elapsed, 200.0);
    assert_under("grapheme_col_roundtrip", round_elapsed, 400.0);
}

// ---------------------------------------------------------------------------
// 5. Typing
// ---------------------------------------------------------------------------

/// The real per-keystroke cost: mutate the buffer, then rebuild the line index.
/// One frame at 60 Hz is 16.7 ms; if a keystroke costs more than that, typing
/// visibly stutters. This is the test that says whether the app is usable.
#[test]
fn typing_a_character_is_fast() {
    let doc = document();
    let mut n = note::Note::from_text(doc.to_string());
    let keystrokes = scaled(200);

    // Type in the MIDDLE — the worst case for a `String` insert, since half the
    // buffer has to be memmoved.
    let mid = n.len() / 2;
    let (mut at, _) = n.clamp_range(mid, mid);

    let started = Instant::now();
    for _ in 0..keystrokes {
        n.replace_range(at, at, "x");
        at += 1;
        let idx = index::LineIndex::new(n.text());
        std::hint::black_box(idx.line_count());
    }
    let elapsed = started.elapsed();

    let per = ms(elapsed) / keystrokes as f64;
    eprintln!(
        "[typing] {keystrokes} keystrokes (insert + LineIndex rebuild): \
         {:.1} ms total, {per:.3} ms per keystroke",
        ms(elapsed)
    );

    // What the app actually does per keystroke: insert, then patch the index
    // rather than rebuild it. The rebuild above is the pessimistic bound; this
    // is the number the README quotes.
    let mut n = note::Note::from_text(doc.to_string());
    let mut idx = index::LineIndex::new(n.text());
    let (mut at, _) = n.clamp_range(n.len() / 2, n.len() / 2);
    let started = Instant::now();
    for _ in 0..keystrokes {
        n.replace_range(at, at, "x");
        idx.splice(n.text(), at, "", "x");
        at += 1;
        std::hint::black_box(idx.line_count());
    }
    let spliced = ms(started.elapsed()) / keystrokes as f64;
    eprintln!("[typing] the same, splicing the index instead: {spliced:.3} ms per keystroke");
    // Unoptimised, the splice's own debug assertions cost more than the work,
    // so this only means anything in release — as the module header says of
    // every timing here.
    if !cfg!(debug_assertions) {
        assert!(
            spliced < per,
            "splicing must beat rebuilding: {spliced:.3} ms vs {per:.3} ms"
        );
    }
    if per / SLOWDOWN > 8.0 {
        eprintln!("[typing] WARNING: a keystroke costs more than half a 60 Hz frame");
    }

    assert_eq!(n.len(), doc.len() + keystrokes, "inserts did not land");
    assert_eq!(&n.text()[at - keystrokes..at], "x".repeat(keystrokes));

    // 16.7 ms is one frame. The release budget is exactly that; the debug
    // multiplier keeps `cargo test` (no --release) from failing spuriously.
    assert!(
        per < 16.7 * SLOWDOWN,
        "typing costs {per:.3} ms per keystroke — over one 60 Hz frame \
         (budget {:.1} ms for this profile)",
        16.7 * SLOWDOWN
    );
}

// ---------------------------------------------------------------------------
// 6. Highlighting
// ---------------------------------------------------------------------------

/// Only what is on screen may be highlighted. A screenful is ~60 lines.
#[test]
fn highlighting_a_viewport_is_fast() {
    const VIEWPORT: usize = 60;
    let doc = document();
    let idx = index::LineIndex::new(doc);
    let lines = idx.line_count();
    let screenfuls = scaled(50).max(5);

    let mut rng = Rng::new(0x5C011);
    let starts: Vec<usize> = (0..screenfuls)
        .map(|_| rng.below(lines - VIEWPORT))
        .collect();

    let mut worst = Duration::ZERO;
    let mut spans_seen = 0usize;
    let started = Instant::now();
    for &start in &starts {
        let one = Instant::now();
        // Exactly what the renderer does: walk the visible lines, track the
        // fence state across them, style each one.
        let mut in_code_block = false;
        for l in start..start + VIEWPORT {
            let (s, e) = idx
                .line_range(l)
                .unwrap_or_else(|| panic!("line {l} is out of range"));
            let line = &doc[s..e];
            let spans = markdown::highlight_line(line, in_code_block);
            spans_seen += spans.len();
            if markdown::is_fence(line) {
                in_code_block = !in_code_block;
            }
            std::hint::black_box(markdown::is_separator(line));
            // The renderer asks this of every visible line too: is this line
            // nothing but a reference into the image store? It must stay a
            // couple of `find`s on one line, never anything that grows with
            // the document or touches the filesystem.
            std::hint::black_box(images::parse_reference(line));
        }
        worst = worst.max(one.elapsed());
    }
    let elapsed = started.elapsed();

    let per = ms(elapsed) / screenfuls as f64;
    eprintln!(
        "[highlight] {screenfuls} screenfuls of {VIEWPORT} lines: {:.2} ms total, \
         {per:.4} ms per screenful (worst {:.4} ms), {spans_seen} spans",
        ms(elapsed),
        ms(worst)
    );
    assert!(spans_seen > 0, "highlighter produced nothing");

    // A screenful must be far cheaper than a frame; 1 ms is already 10x more
    // than a release build needs.
    assert!(
        per < 1.0 * SLOWDOWN,
        "a screenful costs {per:.4} ms (budget {:.2} ms for this profile)",
        1.0 * SLOWDOWN
    );
    assert_under("highlight_viewport_worst_case", worst, 5.0);

    // ---- and now the reason viewport-only highlighting is not optional -----
    // Measured and printed, never asserted: this is the number that justifies
    // the design, and it also allocates a Vec<Span> per line for 214k lines.
    if cfg!(debug_assertions) {
        eprintln!(
            "[highlight] whole-document pass skipped in a debug build \
             (run with --release to see it)"
        );
    } else {
        let split_started = Instant::now();
        let all: Vec<&str> = doc.split('\n').collect();
        let split_elapsed = split_started.elapsed();

        let doc_started = Instant::now();
        let styled = markdown::highlight_document(&all);
        let doc_elapsed = doc_started.elapsed();

        let total_spans: usize = styled.iter().map(|v| v.len()).sum();
        eprintln!(
            "[highlight] whole document: split {:.1} ms + highlight_document {:.1} ms \
             for {} lines / {total_spans} spans",
            ms(split_elapsed),
            ms(doc_elapsed),
            all.len()
        );
        eprintln!(
            "[highlight] => whole-document highlighting is {:.0}x one screenful; \
             never do it on the render path",
            ms(doc_elapsed) / per.max(f64::MIN_POSITIVE)
        );
        assert_eq!(styled.len(), all.len());
    }
}

// ---------------------------------------------------------------------------
// 7. Block scan
// ---------------------------------------------------------------------------

/// `blocks()` is O(n) by design and the app caches it. This test is here to
/// catch it becoming quadratic, not to demand that it be fast.
#[test]
fn blocks_scan_is_acceptable() {
    let doc = document();
    let n = note::Note::from_text(doc.to_string());

    let started = Instant::now();
    let blocks = n.blocks();
    let elapsed = started.elapsed();

    eprintln!(
        "[blocks] blocks() over {} bytes: {:.2} ms for {} blocks",
        doc.len(),
        ms(elapsed),
        blocks.len()
    );
    assert_eq!(
        blocks.len(),
        corpus::stats(doc).notes,
        "block count != note count"
    );
    assert_eq!(blocks.len(), 36_500);
    assert_under("blocks", elapsed, 250.0);

    // Locating the block under the caret happens on every click and on every
    // "promote this note". NOTE: as written, `block_index_at` re-splits the
    // whole document and `block_start_offset` rebuilds the whole block list, so
    // each probe is O(document) — measured at ~20 ms on a 9 MB corpus. That is
    // fine only because the app calls them per user action, never per frame.
    // Keep the probe count tiny; this budget catches a quadratic regression,
    // it does not certify the calls as cheap.
    let mut rng = Rng::new(0xB10C);
    let probes = scaled(20).max(3);
    let offsets: Vec<usize> = (0..probes).map(|_| rng.offset_in(doc)).collect();

    let started = Instant::now();
    let mut acc = 0usize;
    for &o in &offsets {
        acc += n.block_index_at(o);
        acc += n.block_start_offset(o);
    }
    let probe_elapsed = started.elapsed();
    std::hint::black_box(acc);

    let per_probe = ms(probe_elapsed) / probes as f64;
    eprintln!(
        "[blocks] {probes} x (block_index_at + block_start_offset): {:.1} ms \
         ({per_probe:.2} ms per caret probe — O(document), do not call per frame)",
        ms(probe_elapsed)
    );
    assert!(
        per_probe < 150.0 * SLOWDOWN,
        "a caret block probe costs {per_probe:.2} ms (budget {:.0} ms for this profile) \
         — block lookup has gone quadratic",
        150.0 * SLOWDOWN
    );

    for &o in offsets.iter().take(3) {
        let i = n.block_index_at(o);
        assert!(i < blocks.len(), "block_index_at({o}) = {i} out of range");
        assert_eq!(n.block_start_offset(o), blocks[i].start);
    }
}

// ---------------------------------------------------------------------------
// 8. Promoting a note
// ---------------------------------------------------------------------------

/// The app's headline gesture: pull an old note back to the top. It is a cut
/// and a paste of one note's bytes — the scan for the note's own boundaries is
/// the only part that touches the whole document — and what it reports has to
/// stay the size of that note, because it is what lands in the undo history.
#[test]
fn promoting_a_note_is_fast() {
    let doc = document();
    let mut n = note::Note::from_text(doc.to_string());
    let index = index::LineIndex::new(n.text());
    let count = n.blocks().len();
    let target = count - 3; // a note from the very end of twenty years

    let started = Instant::now();
    let moved = n.plan_bring_block_up(&index, target).inspect(|planned| n.apply_move(planned));
    let elapsed = started.elapsed();

    eprintln!(
        "[promote] bring_block_up({target}) of {count} blocks: {:.1} ms",
        ms(elapsed)
    );
    let moved = moved.expect("bring_block_up did not report the move");
    assert_eq!(moved.moved_to, 0, "the promoted note is at the top");
    assert_eq!(n.blocks().len(), count, "promotion changed the block count");
    assert_under("bring_block_up", elapsed, 500.0);

    // What undo has to hold is the note that moved, not the document above it.
    let recorded = moved.cut_text.len() + moved.insert_text.len();
    eprintln!(
        "[promote] recorded into undo: {} bytes ({} of document)",
        recorded,
        format_args!("{:.5}%", 100.0 * recorded as f64 / doc.len() as f64)
    );
    assert!(
        recorded < doc.len() / 100,
        "promoting recorded {recorded} bytes of a {} byte document — the undo \
         entry has to be the size of one note",
        doc.len()
    );

    let mut n2 = note::Note::from_text(doc.to_string());
    let index2 = index::LineIndex::new(n2.text());
    let started = Instant::now();
    let swapped = n2
        .plan_move_block_up_one(&index2, count - 1)
        .inspect(|planned| n2.apply_move(planned));
    let swap_elapsed = started.elapsed();
    assert!(swapped.is_some());
    eprintln!("[promote] move_block_up_one: {:.1} ms", ms(swap_elapsed));
    assert_under("move_block_up_one", swap_elapsed, 500.0);

    let mut n3 = note::Note::from_text(doc.to_string());
    let started = Instant::now();
    let caret = n3.new_block_at_top();
    let new_elapsed = started.elapsed();
    assert_eq!(caret, 0);
    assert_eq!(n3.blocks().len(), count + 1);
    eprintln!("[promote] new_block_at_top: {:.1} ms", ms(new_elapsed));
    assert_under("new_block_at_top", new_elapsed, 500.0);
}

// ---------------------------------------------------------------------------
// Integrated hot paths added for the performance run: fence build, search,
// full cold-start, and the autosave write. These are what a user actually
// waits on — launching, searching, saving — on a twenty-year note.
// ---------------------------------------------------------------------------

#[test]
fn perf_fence_map_build() {
    let doc = document();
    let idx = index::LineIndex::new(doc);
    let started = Instant::now();
    let fences = gravitynote::fences::FenceMap::new(doc, &idx);
    let elapsed = started.elapsed();
    std::hint::black_box(&fences);
    eprintln!("[fences] FenceMap::new over {} lines: {:.2} ms", idx.line_count(), ms(elapsed));
}

#[test]
fn perf_search_whole_document() {
    let doc = document();
    for q in ["note", "the", "z", "twenty-year-never-appears-xyzzy"] {
        let started = Instant::now();
        let hits = gravitynote::find::find_all(doc, q, gravitynote::find::MatchOptions::default());
        let elapsed = started.elapsed();
        eprintln!("[search] find_all({q:?}): {:.2} ms, {} hits", ms(elapsed), hits.len());
    }
}

#[test]
fn perf_cold_start_sequence() {
    let doc = document();
    // What NoteApp::new pays before the first frame: parse into the buffer, build
    // the line index, build the fence map, and enumerate blocks.
    let started = Instant::now();
    let n = note::Note::from_text(doc.to_string());
    let t_buf = started.elapsed();
    let idx = index::LineIndex::new(n.text());
    let t_idx = started.elapsed();
    let fences = gravitynote::fences::FenceMap::new(n.text(), &idx);
    let t_fence = started.elapsed();
    std::hint::black_box((&fences, &idx));
    eprintln!(
        "[cold_start] buffer {:.2} ms, +index {:.2} ms, +fences {:.2} ms (total {:.2} ms)",
        ms(t_buf), ms(t_idx - t_buf), ms(t_fence - t_idx), ms(t_fence)
    );
}

#[test]
fn perf_autosave_write() {
    let doc = document();
    let dir = std::env::temp_dir().join(format!("gravitynote-perf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("note.md");
    let mut store = gravitynote::persist::Persist::new(path.clone());
    let started = Instant::now();
    store.save(doc).unwrap();
    let elapsed = started.elapsed();
    eprintln!("[autosave] atomic write of {} bytes: {:.2} ms", doc.len(), ms(elapsed));
    let _ = std::fs::remove_dir_all(&dir);
}
