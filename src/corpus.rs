//! Deterministic generator for a realistic markdown note corpus.
//!
//! The app keeps every note in a single markdown file, notes separated by a
//! `---` thematic break. The question this module answers is: *what does that
//! file look like after twenty years of regular note-taking, and is the
//! document layer still fast against it?*
//!
//! [`generate`] produces a document that is deliberately awkward for a text
//! editor: heavy-tailed note lengths, mixed markdown constructs, fenced code
//! blocks, and enough multi-byte content (accents, CJK, emoji) that the
//! byte-offset / UTF-16 / grapheme paths are actually exercised instead of
//! quietly taking the ASCII fast path.
//!
//! # Properties
//!
//! * **Pure std.** No `rand`, no `gpui`, no dependency on any other module in
//!   this crate — including [`crate::note`]. This file must stay free-standing
//!   so it can be lifted into a benchmark harness or verified in isolation.
//! * **Deterministic.** The same [`CorpusSpec`] always yields byte-identical
//!   output; the randomness is an xorshift64 seeded from the spec.
//! * **Shape-locked.** [`CorpusSpec::twenty_years`] is asserted by unit test to
//!   land in 8–12 MB and 150k–250k lines, so the corpus cannot silently drift
//!   out from under the performance tests that consume it.

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// Shape of a generated note corpus.
#[derive(Clone, Copy, Debug)]
pub struct CorpusSpec {
    /// How many notes to generate.
    pub notes: usize,
    /// Deterministic seed.
    pub seed: u64,
}

impl CorpusSpec {
    /// Roughly 20 years of daily note-taking: 5 notes a day for 20 years.
    pub fn twenty_years() -> Self {
        Self {
            notes: 36_500,
            seed: 0x5EED,
        }
    }

    /// A corpus of `notes` notes with the default seed.
    pub fn with_notes(notes: usize) -> Self {
        Self {
            notes,
            seed: 0x5EED,
        }
    }
}

impl Default for CorpusSpec {
    fn default() -> Self {
        Self::twenty_years()
    }
}

/// The separator line the generator writes between notes.
pub const SEPARATOR: &str = "---";

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Generate a markdown document: notes joined by `"\n---\n"`.
///
/// The result never starts or ends with a separator and never ends with a
/// newline, so `document.split("\n---\n").count() == spec.notes`.
pub fn generate(spec: CorpusSpec) -> String {
    let mut state = seed_state(spec.seed);
    // Estimated bytes per note; over-allocating a little beats reallocating a
    // 10 MB string half a dozen times.
    let mut out = String::with_capacity(spec.notes.saturating_mul(300) + 64);
    for i in 0..spec.notes {
        if i > 0 {
            out.push('\n');
            out.push_str(SEPARATOR);
            out.push('\n');
        }
        out.push_str(&generate_note(&mut state, i));
    }
    out
}

/// Generate one note body (no separator).
///
/// Never empty, never starts or ends with `\n`, and never contains a line that
/// would be read as a thematic break — so notes stay notes when the document is
/// split again.
pub fn generate_note(rng_state: &mut u64, index: usize) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(8);

    // Most notes are titled; a few are just a thought dumped in.
    if chance(rng_state, 86) {
        lines.push(heading_line(rng_state, index));
    }

    let body = body_line_count(rng_state);
    let mut since_break = 0usize;
    for _ in 0..body {
        // Paragraph breaks in longer notes.
        if since_break >= 3 && chance(rng_state, 14) {
            lines.push(String::new());
            since_break = 0;
        }
        lines.push(body_line(rng_state));
        since_break += 1;
    }

    // ~8% of notes carry a fenced code block.
    if chance(rng_state, 8) {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        push_code_block(rng_state, &mut lines);
    }

    if lines.iter().all(|l| l.is_empty()) {
        lines.push(sentence(rng_state));
    }

    // Guarantee: no line of a note may look like a separator, or splitting the
    // document back into notes would disagree with the generator.
    for line in &mut lines {
        if looks_like_separator(line) {
            line.push_str(" (note)");
        }
    }

    // No leading/trailing blank line: those would collapse into the separator.
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(sentence(rng_state));
    }

    lines.join("\n")
}

/// Summary statistics, handy for test output.
#[derive(Clone, Copy, Debug)]
pub struct CorpusStats {
    pub bytes: usize,
    pub lines: usize,
    pub notes: usize,
    pub chars: usize,
}

/// Measure a document. `notes` counts separator lines plus one, matching the
/// block model in `crate::note` (an empty document is one note).
pub fn stats(text: &str) -> CorpusStats {
    let mut lines = 0usize;
    let mut separators = 0usize;
    for line in text.split('\n') {
        lines += 1;
        if looks_like_separator(line) {
            separators += 1;
        }
    }
    CorpusStats {
        bytes: text.len(),
        lines,
        notes: separators + 1,
        chars: text.chars().count(),
    }
}

// ---------------------------------------------------------------------------
// Deterministic randomness (xorshift64 — small, fast, reproducible)
// ---------------------------------------------------------------------------

fn seed_state(seed: u64) -> u64 {
    // xorshift64 has one fixed point (0); splitmix the seed so tiny seeds like
    // 0 or 1 still produce a well-mixed stream.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    if z == 0 {
        0x2545_F491_4F6C_DD1D
    } else {
        z
    }
}

#[inline]
fn next_u64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Uniform in `0..n` (`n` must be non-zero).
#[inline]
fn below(state: &mut u64, n: usize) -> usize {
    (next_u64(state) % n as u64) as usize
}

/// True `percent` of the time.
#[inline]
fn chance(state: &mut u64, percent: u64) -> bool {
    next_u64(state) % 100 < percent
}

#[inline]
fn pick<'a>(state: &mut u64, items: &[&'a str]) -> &'a str {
    items[below(state, items.len())]
}

// ---------------------------------------------------------------------------
// Word banks
// ---------------------------------------------------------------------------

const NOUNS: &[&str] = &[
    "invoice",
    "sprint",
    "backlog",
    "prototype",
    "deadline",
    "landlord",
    "dentist",
    "bike chain",
    "rent",
    "commit",
    "migration",
    "onboarding doc",
    "grocery run",
    "tax return",
    "index",
    "render loop",
    "keyboard",
    "scroll offset",
    "caret",
    "font stack",
    "layout pass",
    "cache",
    "roadmap",
    "retro",
    "standup",
    "budget",
    "mortgage",
    "sourdough starter",
    "car service",
    "passport renewal",
    "reading list",
    "guitar lesson",
    "physio appointment",
    "insurance claim",
    "meeting notes",
    "expense report",
    "design review",
    "changelog",
    "release branch",
    "flight booking",
    "hotel confirmation",
    "vet visit",
    "plant watering",
    "coffee order",
    "gym plan",
    "walking route",
    "recipe",
    "wine list",
    "podcast queue",
    "test suite",
    "benchmark",
    "profiler trace",
    "memory graph",
    "diff",
    "pull request",
    "postmortem",
];

const ADJECTIVES: &[&str] = &[
    "half-finished",
    "overdue",
    "quiet",
    "expensive",
    "temporary",
    "obvious",
    "annoying",
    "surprisingly slow",
    "cheap",
    "brittle",
    "clean",
    "long-overdue",
    "small",
    "enormous",
    "flaky",
    "shared",
    "personal",
    "weekly",
    "monthly",
    "unread",
    "leftover",
    "sensible",
    "unreasonable",
    "provisional",
    "final",
    "rough",
    "polished",
];

const PEOPLE: &[&str] = &[
    "Mira",
    "Tomas",
    "Ada",
    "Jonas",
    "Priya",
    "Lena",
    "Sam",
    "Yuki",
    "Rafa",
    "Ines",
    "Karel",
    "Nadia",
    "Ben",
    "Wei",
    "Olof",
    "Clara",
    "Dmitri",
    "Fatima",
    "Hugo",
    "Sanne",
    "the landlord",
    "my sister",
    "the accountant",
    "the new hire",
    "the dentist",
];

const TIMES: &[&str] = &[
    "tomorrow",
    "before Friday",
    "next week",
    "this evening",
    "after the standup",
    "by the 15th",
    "sometime in spring",
    "on Monday",
    "before the release",
    "over the weekend",
    "in the morning",
    "at the end of the month",
    "tonight",
    "next quarter",
    "after lunch",
];

const VERBS: &[&str] = &[
    "rewrite",
    "check",
    "cancel",
    "renew",
    "call about",
    "book",
    "measure",
    "refactor",
    "delete",
    "archive",
    "sketch",
    "cost out",
    "re-read",
    "test",
    "ship",
    "revert",
    "profile",
    "pay",
    "return",
    "chase",
    "confirm",
    "reschedule",
    "print",
    "sign",
    "compare",
];

const GERUNDS: &[&str] = &[
    "reallocating",
    "rebuilding the whole index",
    "blocking the main thread",
    "dropping frames",
    "waking me up at 3am",
    "eating the battery",
    "growing without bound",
    "asking twice",
    "silently failing",
    "hanging on save",
    "scrolling badly",
];

/// Sentence skeletons. `{n}` noun, `{a}` adjective, `{p}` person, `{t}` time,
/// `{v}` verb, `{g}` gerund. Nouns/adjectives get inline markup applied.
const TEMPLATES: &[&str] = &[
    "Need to {v} the {a} {n} {t}.",
    "{p} says the {n} is blocked on the {n}; going to {v} it {t}.",
    "Rough idea: {v} the {n} so the {n} stops {g}.",
    "The {a} {n} is still {a} — {v} it {t} or drop it entirely.",
    "Spent the morning on the {n}. Result: {a}, but it works.",
    "Ask {p} whether the {n} can wait until {t}.",
    "Note to self: the {n} only breaks when the {n} is {a}.",
    "Called {p} about the {n}. They will get back to me {t}.",
    "If the {n} keeps {g} I will just {v} the {n} and move on.",
    "Read half of the {n} last night; the part about the {a} {n} is worth revisiting.",
    "{a} realisation: the {n} and the {n} are the same problem.",
    "Decided against the {a} {n}. Too expensive for what it buys.",
    "Left the {n} on the table for {p} to look at {t}.",
    "Every time I touch the {n} the {n} needs {g} too.",
    "Budget check: the {n} came in under, the {n} did not.",
    "Try to {v} the {n} without touching the {a} {n}.",
    "Weather was {a}, walked the long way, thought about the {n}.",
    "Reminder that the {n} expires {t} — {v} it before that.",
    "Half a plan: {v} the {n}, then the {n}, then stop.",
    "{p} sent over the {n}; it is {a} but broadly right.",
    "Kept the {a} {n} and threw out the rest.",
    "Nothing urgent today. The {n} can wait until {t}.",
    "Third attempt at the {n}. This one is at least {a}.",
    "The {a} {n} is the only thing standing between me and {t}.",
];

const BULLET_TEMPLATES: &[&str] = &[
    "{v} the {a} {n}",
    "{n} — ask {p}",
    "{n} ({t})",
    "{a} {n}, then the {n}",
    "{v} the {n} before it starts {g}",
    "{p}: {n}",
    "{n} still open",
    "double-check the {n}",
];

const QUOTE_TEMPLATES: &[&str] = &[
    "the {n} is not the {n}, and it never was",
    "{p} put it well: keep the {a} {n}, drop the rest",
    "the {a} {n} beats the perfect one that never ships",
    "you cannot {v} your way out of a {a} {n}",
];

const CODE_LINES: &[&str] = &[
    "let idx = LineIndex::new(&text);",
    "for (i, line) in lines.iter().enumerate() {",
    "    if is_fence(line) { in_code = !in_code; }",
    "}",
    "assert_eq!(idx.line_count(), lines.len());",
    "fn to_utf16(&self, text: &str, offset: usize) -> usize {",
    "    debug_assert!(text.is_char_boundary(offset));",
    "rg --files -g '*.rs' | xargs wc -l",
    "git rebase -i HEAD~4",
    "cargo test --release --test huge_note -- --nocapture",
    "SELECT id, created_at FROM notes ORDER BY created_at DESC LIMIT 20;",
    "export PATH=\"$HOME/.cargo/bin:$PATH\"",
    "const MAX_VIEWPORT_LINES: usize = 60;",
    "self.blocks.get_or_insert_with(|| note.blocks())",
    "// TODO: cache this, it is O(n) per keystroke",
    "match style { MdStyle::Heading(l) => theme.heading(l), _ => theme.text }",
    "python3 -m http.server 8000",
    "docker compose up -d --build",
    "impl Iterator for Graphemes<'_> {",
    "    fn next(&mut self) -> Option<Self::Item> { self.inner.next() }",
    "defaults write com.apple.dock autohide -bool true",
    "let (start, end) = note.clamp_range(a, b);",
    "if offset >= self.starts[mid] { lo = mid; } else { hi = mid; }",
    "println!(\"{:?}\", stats(&text));",
];

const CODE_INFO: &[&str] = &["rust", "sh", "", "sql", "python", "js", "toml"];

/// Multi-byte snippets: accented Latin, CJK, and emoji. Sprinkled into ~5% of
/// lines so the byte-offset and grapheme paths are genuinely exercised.
const MULTIBYTE: &[&str] = &[
    "café",
    "naïve",
    "résumé",
    "Ærø",
    "Zürich",
    "señor",
    "smörgås",
    "会議のメモ",
    "进度更新",
    "설계 검토",
    "рабочая версия",
    "παράδειγμα",
    "→ next",
    "≈ 12 h",
    "🚀",
    "✅",
    "🌱",
    "☕️",
    "👍🏽",
    "🇸🇪",
    "«citat»",
    "日本語のノート",
];

const SLUGS: &[&str] = &[
    "notes",
    "docs/index",
    "thread/8812",
    "r/rust",
    "issues/431",
    "wiki/Caret",
    "posts/latency",
    "a/b",
    "x",
    "commit/9f2c1a",
    "blog/2019/perf",
    "files/report.pdf",
];

const TOPICS: &[&str] = &[
    "Standup",
    "Weekly review",
    "Groceries",
    "Reading",
    "Ideas",
    "Bugs",
    "Trip planning",
    "Budget",
    "House",
    "Health",
    "Music",
    "Rust notes",
    "Design",
    "Calls",
    "Errands",
    "Interview prep",
    "Garden",
    "Recipes",
    "Kids",
    "Bike",
    "Meeting",
    "Retro",
    "Scratch",
    "Log",
    "Inbox",
    "Follow-ups",
    "Photography",
    "Language practice",
    "Taxes",
    "Car",
];

// ---------------------------------------------------------------------------
// Line construction
// ---------------------------------------------------------------------------

/// Heavy-tailed: most notes are a few lines, a handful run very long.
fn body_line_count(state: &mut u64) -> usize {
    match below(state, 1000) {
        0..=649 => 1 + below(state, 2),   // 1..2   — a jotted thought
        650..=869 => 3 + below(state, 3), // 3..5   — a normal note
        870..=964 => 5 + below(state, 5), // 5..9   — a worked-through note
        965..=994 => 9 + below(state, 9), // 9..17  — a meeting
        _ => 18 + below(state, 30),       // 18..47 — the occasional brain dump
    }
}

fn heading_line(state: &mut u64, index: usize) -> String {
    let mut out = String::with_capacity(48);
    out.push('#');
    if chance(state, 40) {
        out.push('#');
    }
    out.push(' ');
    out.push_str(pick(state, TOPICS));
    out.push(' ');
    // Five notes a day, starting 2005-01-01.
    out.push_str(&date_string(index / 5));
    if chance(state, 18) {
        out.push_str(" — ");
        out.push_str(pick(state, NOUNS));
    }
    out
}

fn body_line(state: &mut u64) -> String {
    let mut line = match below(state, 100) {
        0..=33 => sentence(state),
        34..=57 => {
            let mut s = String::from("- ");
            s.push_str(&expand_one_of(state, BULLET_TEMPLATES));
            s
        }
        58..=75 => {
            let mut s = String::from(if chance(state, 35) {
                "- [x] "
            } else {
                "- [ ] "
            });
            s.push_str(&expand_one_of(state, BULLET_TEMPLATES));
            s
        }
        76..=85 => {
            let mut s = String::new();
            s.push_str(&(1 + below(state, 9)).to_string());
            s.push_str(". ");
            s.push_str(&expand_one_of(state, BULLET_TEMPLATES));
            s
        }
        86..=92 => {
            let mut s = String::from("> ");
            s.push_str(&expand_one_of(state, QUOTE_TEMPLATES));
            s
        }
        _ => sentence(state),
    };

    // ~6% of lines carry a link.
    if chance(state, 6) {
        line.push_str(" [");
        line.push_str(pick(state, NOUNS));
        line.push_str("](https://example.com/");
        line.push_str(pick(state, SLUGS));
        line.push(')');
    }
    // ~5% of lines carry multi-byte content.
    if chance(state, 5) {
        line.push(' ');
        line.push_str(pick(state, MULTIBYTE));
    }
    line
}

fn sentence(state: &mut u64) -> String {
    let mut s = expand_one_of(state, TEMPLATES);
    // Often a second clause, so line lengths are not all alike.
    if chance(state, 40) {
        s.push(' ');
        s.push_str(&expand_one_of(state, TEMPLATES));
    }
    s
}

fn push_code_block(state: &mut u64, lines: &mut Vec<String>) {
    let mut fence = String::from("```");
    fence.push_str(pick(state, CODE_INFO));
    lines.push(fence);
    let n = 3 + below(state, 8); // 3..=10
    for _ in 0..n {
        lines.push(pick(state, CODE_LINES).to_string());
    }
    lines.push(String::from("```"));
}

/// Pick a template from `bank` and expand it.
fn expand_one_of(state: &mut u64, bank: &[&str]) -> String {
    let template = pick(state, bank);
    expand(state, template)
}

/// Expand a template, substituting word banks and sprinkling inline markup.
fn expand(state: &mut u64, template: &str) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let bytes = template.as_bytes();
    let mut i = 0usize;
    // Everything from `copied` up to the next placeholder is passed through as a
    // string slice, so non-ASCII template text (the em dash) survives intact.
    let mut copied = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' && i + 2 < bytes.len() && bytes[i + 2] == b'}' {
            out.push_str(&template[copied..i]);
            let word = match bytes[i + 1] {
                b'n' => pick(state, NOUNS),
                b'a' => pick(state, ADJECTIVES),
                b'p' => pick(state, PEOPLE),
                b't' => pick(state, TIMES),
                b'v' => pick(state, VERBS),
                b'g' => pick(state, GERUNDS),
                _ => "thing",
            };
            push_marked_up(state, &mut out, word);
            i += 3;
            copied = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&template[copied..]);
    out
}

/// Wrap a word in inline markup some of the time.
fn push_marked_up(state: &mut u64, out: &mut String, word: &str) {
    match below(state, 100) {
        0..=4 => {
            out.push_str("**");
            out.push_str(word);
            out.push_str("**");
        }
        5..=9 => {
            out.push('_');
            out.push_str(word);
            out.push('_');
        }
        10..=13 => {
            out.push('`');
            out.push_str(word);
            out.push('`');
        }
        _ => out.push_str(word),
    }
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD`, `days` days after 2005-01-01.
fn date_string(days: usize) -> String {
    const EPOCH_2005_01_01: i64 = 12_784; // days since 1970-01-01
    let (y, m, d) = civil_from_days(EPOCH_2005_01_01 + days as i64);
    let mut s = String::with_capacity(10);
    s.push_str(&y.to_string());
    s.push('-');
    if m < 10 {
        s.push('0');
    }
    s.push_str(&m.to_string());
    s.push('-');
    if d < 10 {
        s.push('0');
    }
    s.push_str(&d.to_string());
    s
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Separator probe (local copy — this module stays dependency-free)
// ---------------------------------------------------------------------------

/// Mirrors `crate::note::is_separator_line` without importing it: 3+ of the
/// same `-`/`*`/`_` after trimming ASCII whitespace.
fn looks_like_separator(line: &str) -> bool {
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MB: usize = 1024 * 1024;

    /// The one test that pins the corpus down. If this drifts, every timing in
    /// `tests/huge_note.rs` is measuring a different document.
    #[test]
    fn twenty_years_has_the_expected_shape() {
        let spec = CorpusSpec::twenty_years();
        let text = generate(spec);
        let s = stats(&text);
        eprintln!(
            "twenty_years: {} bytes ({:.2} MB), {} lines, {} notes, {} chars",
            s.bytes,
            s.bytes as f64 / MB as f64,
            s.lines,
            s.notes,
            s.chars
        );
        assert!(
            (8 * MB..=12 * MB).contains(&s.bytes),
            "expected 8-12 MB, got {} bytes",
            s.bytes
        );
        assert!(
            (150_000..=250_000).contains(&s.lines),
            "expected 150k-250k lines, got {}",
            s.lines
        );
        assert_eq!(s.notes, 36_500);
    }

    #[test]
    fn generation_is_deterministic_for_a_fixed_seed() {
        let spec = CorpusSpec::with_notes(400);
        assert_eq!(generate(spec), generate(spec));
    }

    #[test]
    fn different_seeds_produce_different_documents() {
        let a = generate(CorpusSpec {
            notes: 400,
            seed: 1,
        });
        let b = generate(CorpusSpec {
            notes: 400,
            seed: 2,
        });
        assert_ne!(a, b);
        // Zero is a valid seed (the state is splitmixed, not used raw).
        let z = generate(CorpusSpec {
            notes: 400,
            seed: 0,
        });
        assert!(!z.is_empty());
        assert_ne!(z, a);
    }

    #[test]
    fn generate_note_is_a_pure_function_of_the_state() {
        let mut a = 12_345u64;
        let mut b = 12_345u64;
        for i in 0..50 {
            assert_eq!(generate_note(&mut a, i), generate_note(&mut b, i));
        }
        assert_eq!(a, b);
    }

    #[test]
    fn stats_line_count_matches_split() {
        let text = generate(CorpusSpec::with_notes(500));
        let s = stats(&text);
        assert_eq!(s.lines, text.split('\n').count());
        assert_eq!(s.bytes, text.len());
        assert_eq!(s.chars, text.chars().count());
    }

    #[test]
    fn document_round_trips_through_separator_splitting() {
        let notes = 700;
        let text = generate(CorpusSpec::with_notes(notes));

        // Counting separator lines the way `note::Note::blocks` does.
        let sep_lines = text.split('\n').filter(|l| looks_like_separator(l)).count();
        assert_eq!(sep_lines, notes - 1);
        assert_eq!(stats(&text).notes, notes);

        // And splitting on the literal joiner yields exactly the note bodies.
        let parts: Vec<&str> = text.split("\n---\n").collect();
        assert_eq!(parts.len(), notes);
        for p in &parts {
            assert!(!p.is_empty(), "empty note body");
            assert!(
                !p.starts_with('\n') && !p.ends_with('\n'),
                "stray blank edge"
            );
            assert!(
                !p.split('\n').any(looks_like_separator),
                "note body contains a separator line: {p:?}"
            );
        }
    }

    #[test]
    fn empty_and_tiny_specs_are_well_formed() {
        assert_eq!(generate(CorpusSpec::with_notes(0)), "");
        let one = generate(CorpusSpec::with_notes(1));
        assert!(!one.is_empty());
        assert_eq!(stats(&one).notes, 1);
        let two = generate(CorpusSpec::with_notes(2));
        assert_eq!(stats(&two).notes, 2);
    }

    #[test]
    fn corpus_contains_the_markdown_constructs_we_highlight() {
        let text = generate(CorpusSpec::with_notes(3_000));
        let has = |p: &str| text.split('\n').any(|l| l.contains(p));
        assert!(text.split('\n').any(|l| l.starts_with("# ")), "h1");
        assert!(text.split('\n').any(|l| l.starts_with("## ")), "h2");
        assert!(text.split('\n').any(|l| l.starts_with("- ")), "bullet");
        assert!(text.split('\n').any(|l| l.starts_with("- [ ] ")), "task");
        assert!(text.split('\n').any(|l| l.starts_with("- [x] ")), "done");
        assert!(text.split('\n').any(|l| l.starts_with("> ")), "quote");
        assert!(text.split('\n').any(|l| l.starts_with("1. ")), "ordered");
        assert!(text.split('\n').any(|l| l.starts_with("```")), "fence");
        assert!(has("**"), "bold");
        assert!(has("_"), "italic");
        assert!(has("`"), "code");
        assert!(has("](https://example.com/"), "link");
        assert!(text.split('\n').any(|l| l.is_empty()), "blank line");
    }

    #[test]
    fn corpus_has_real_multibyte_content() {
        let text = generate(CorpusSpec::with_notes(3_000));
        let s = stats(&text);
        assert!(s.chars < s.bytes, "corpus is pure ASCII");
        let multibyte_lines = text.split('\n').filter(|l| !l.is_ascii()).count();
        // Roughly 5% of body lines; assert a loose band so it cannot vanish.
        assert!(
            multibyte_lines * 100 > s.lines,
            "too few multi-byte lines: {multibyte_lines} of {}",
            s.lines
        );
        assert!(text.contains('世') || text.contains('会') || text.contains('进'));
        assert!(text.contains('🚀') || text.contains('✅') || text.contains('🌱'));
    }

    #[test]
    fn note_lengths_are_heavy_tailed() {
        let text = generate(CorpusSpec::with_notes(5_000));
        let mut lens: Vec<usize> = text
            .split("\n---\n")
            .map(|n| n.split('\n').count())
            .collect();
        lens.sort_unstable();
        let median = lens[lens.len() / 2];
        let max = *lens.last().unwrap();
        assert!(
            median <= 8,
            "median note is {median} lines — not short enough"
        );
        assert!(max >= 18, "longest note is {max} lines — no tail");
    }

    #[test]
    fn dates_advance_and_are_well_formed() {
        assert_eq!(date_string(0), "2005-01-01");
        assert_eq!(date_string(31), "2005-02-01");
        assert_eq!(date_string(365), "2006-01-01");
        // 20 years of 5 notes a day lands in 2024.
        let last = date_string(36_499 / 5);
        assert!(last.starts_with("2024-"), "last date was {last}");
    }

    #[test]
    fn templates_expand_without_mangling_non_ascii() {
        let mut state = seed_state(7);
        let out = expand(&mut state, "{n} — ask {p}, ✅ {t}");
        assert!(out.contains(" — ask "), "em dash lost: {out:?}");
        assert!(out.contains(", ✅ "), "emoji lost: {out:?}");
        assert!(
            !out.contains('{') && !out.contains('}'),
            "placeholder left: {out:?}"
        );
        // Literal text either side of a placeholder is preserved verbatim.
        let mut state = seed_state(7);
        assert_eq!(
            expand(&mut state, "no placeholders — ✅"),
            "no placeholders — ✅"
        );
    }

    #[test]
    fn separator_probe_matches_the_note_module_rules() {
        for s in ["---", "----", "***", "___", "- - -", "  ---  ", "\t---\t"] {
            assert!(looks_like_separator(s), "{s:?}");
        }
        for s in [
            "", "   ", "--", "-*-", "---a", "- item", "> quote", "# head",
        ] {
            assert!(!looks_like_separator(s), "{s:?}");
        }
    }
}
