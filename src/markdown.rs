//! Lightweight, dependency-free markdown highlighter.
//!
//! The renderer shapes **one line at a time** and turns the output of
//! [`highlight_line`] directly into `gpui::TextRun`s, so this module's contract
//! is load-bearing. For any `line` and any `in_code_block`, the returned
//! `Vec<Span>` satisfies:
//!
//! 1. **Empty line ⇒ empty vec.** `line.is_empty()` yields `[]`.
//! 2. **Exact contiguous cover.** Otherwise the spans are sorted by `start`,
//!    non-overlapping and gapless: `spans[0].start == 0`,
//!    `spans[i].end == spans[i + 1].start`, and
//!    `spans.last().end == line.len()`.
//! 3. **Char boundaries.** Every `start` and `end` satisfies
//!    `line.is_char_boundary(x)`, so slicing `&line[s.start..s.end]` is always
//!    safe.
//! 4. **No empty spans.** `start < end` for every span.
//! 5. **Total.** The function never panics, on any byte string that is valid
//!    UTF-8: malformed markdown, lone `*`, unterminated backticks, unterminated
//!    `[`, nested emphasis, emoji, CJK, combining marks, gigantic lines.
//!
//! Additionally, adjacent spans always have *different* styles (runs with equal
//! styles are merged) which keeps `TextRun` counts low.
//!
//! Offsets are byte offsets **into the line**, never into the document.
//!
//! Fenced-code state is not inferable from a single line, so the caller threads
//! it: pass `in_code_block = true` when previous lines opened a fence. The fence
//! line itself is passed with the state that was active *before* it, and
//! [`is_fence`] tells the caller when to toggle. [`highlight_document`] does
//! that bookkeeping for whole buffers.
//!
//! This is deliberately *not* a CommonMark implementation. It is a syntax
//! highlighter: it never fails, never reflows, and prefers "looks right in an
//! editor" over spec conformance.

/// The visual role of a byte range on a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdStyle {
    /// Ordinary prose.
    Text,
    /// Heading body text; the u8 is the level 1..=6.
    Heading(u8),
    /// The `###` sigil and the space after it, for a heading of that level.
    HeadingMarker(u8),
    Bold,
    Italic,
    BoldItalic,
    Strikethrough,
    /// `==marked==` — the one markdown extension a note app is expected to
    /// have, and the thing people reach for straight after bold and italic.
    Highlight,
    /// Inline `code` body (between the backticks).
    Code,
    /// Text inside a fenced code block.
    CodeBlock,
    /// The ``` fence line itself.
    Fence,
    /// Punctuation that is markup, not content: `*`, `_`, `` ` ``, `~`, `[`, `]`,
    /// `(`, `)`. Rendered dim.
    Marker,
    /// The visible text of a `[text](url)` link.
    LinkText,
    /// The url part of a `[text](url)`, and bare autolinks.
    LinkUrl,
    /// The `- `, `* `, `+ `, `1. ` bullet at the head of a list item.
    ListMarker,
    /// An unchecked `[ ]` task box.
    TaskOpen,
    /// A checked `[x]` task box.
    TaskDone,
    /// The `>` blockquote sigil.
    QuoteMarker,
    /// Text inside a blockquote.
    Quote,
    /// A thematic-break line: `---`, `***`, `___`.
    Separator,
}

/// A styled byte range of a single line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte offset into the LINE (not the document).
    pub start: usize,
    /// Byte offset into the LINE, exclusive.
    pub end: usize,
    pub style: MdStyle,
}

impl Span {
    #[inline]
    fn new(start: usize, end: usize, style: MdStyle) -> Self {
        Span { start, end, style }
    }
}

// ---------------------------------------------------------------------------
// Block-level probes
// ---------------------------------------------------------------------------

/// How many `>` sigils a blockquote line opens with, counting through the
/// spaces between them. `0` for anything that is not a quote.
///
/// The renderer indents by this, because nesting that is only visible in the
/// punctuation is not visible: `> > inner` sat at the same margin as the quote
/// around it, and a bullet inside a quote at the same margin as the quote.
pub fn quote_depth(line: &str) -> usize {
    let mut depth = 0usize;
    for c in line.chars() {
        match c {
            '>' => depth += 1,
            ' ' | '\t' => {}
            _ => break,
        }
    }
    depth
}

/// Whether `line` is an indented code block: four spaces or a tab of
/// indentation, and nothing that would make it something else.
///
/// The caller supplies whether the line above is blank or itself indented code,
/// because a run of indented lines is one block and only its first line can be
/// decided on its own. Deliberately narrow: this app indents outlines two
/// spaces a level, so a second-level item is four spaces in, and reading that as
/// code would band half of somebody's outline grey. A list marker, a quote or a
/// heading therefore always wins, and the run must start after a blank line.
pub fn is_indented_code(line: &str, after_blank_or_code: bool) -> bool {
    if !after_blank_or_code {
        return false;
    }
    let indent: usize = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    if indent < 4 {
        return false;
    }
    let rest = line.trim_start();
    !rest.is_empty()
        && list_marker_end(line).is_none()
        && !rest.starts_with('>')
        && !rest.starts_with('#')
        && !is_fence(rest)
}

/// Whether `line` is a run of `=`, which underlines the line above it into a
/// heading. `None` for anything else.
///
/// **Only `=`, though markdown also allows `-`.** A run of dashes is this app's
/// note separator, and that is not a detail to be clever about: the app writes
/// `note\n---\n` itself every time you start a note or move one, so reading a
/// dash run as an underline would make ⌘N merge the new note into the old one
/// on the first keystroke. `=` is claimed by nothing else, so it can mean what
/// it means everywhere else.
///
/// Whether the line above is ordinary text is a question about the document
/// rather than about this line — [`takes_setext_underline`] answers that half.
pub fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_matches(|c: char| c.is_ascii_whitespace());
    (!trimmed.is_empty() && trimmed.bytes().all(|b| b == b'=')).then_some(1)
}

/// Whether `line` can carry a setext underline: ordinary prose, not blank, not
/// itself a rule, a fence, a heading, a quote or a list item. Those all have
/// their own meaning, and none of them is something a `---` underneath turns
/// into a heading.
pub fn takes_setext_underline(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && setext_level(line).is_none()
        && !is_fence(line)
        // A rule least of all: it draws as a hairline in exactly one row, and
        // typing `===` under one grew that row to heading height and pushed the
        // document down.
        && !is_separator(line)
        && !trimmed.starts_with('#')
        && !trimmed.starts_with('>')
        && list_marker_end(line).is_none()
}

/// True when `line` opens or closes a fenced code block (``` or ~~~, 3+ chars,
/// optionally indented up to 3 spaces, optionally followed by an info string).
pub fn is_fence(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && i < 3 && b[i] == b' ' {
        i += 1;
    }
    if i >= b.len() {
        return false;
    }
    let c = b[i];
    if c != b'`' && c != b'~' {
        return false;
    }
    let run = run_len(b, i, c);
    if run < 3 {
        return false;
    }
    // A backtick fence's info string may not itself contain a backtick.
    if c == b'`' && b[i + run..].contains(&b'`') {
        return false;
    }
    true
}

/// True when `line` is a thematic break (3+ of `-`, `*`, or `_`, possibly
/// space-separated, after trimming).
pub fn is_separator(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let b = t.as_bytes();
    let c = b[0];
    if c != b'-' && c != b'*' && c != b'_' {
        return false;
    }
    let mut count = 0usize;
    for &x in b {
        if x == c {
            count += 1;
        } else if x != b' ' && x != b'\t' {
            return false;
        }
    }
    count >= 3
}

/// `(level, marker_end)` when the line opens an ATX heading.
fn heading_at(line: &str) -> Option<(u8, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && i < 3 && b[i] == b' ' {
        i += 1;
    }
    let hash_start = i;
    while i < b.len() && b[i] == b'#' {
        i += 1;
    }
    let level = i - hash_start;
    if level == 0 || level > 6 {
        return None;
    }
    if i == b.len() {
        return Some((level as u8, i));
    }
    if b[i] == b' ' || b[i] == b'\t' {
        return Some((level as u8, i + 1));
    }
    None
}

/// End offset of the blockquote sigil run (`>`, `> `, `>> `, `> > `), if any.
fn quote_at(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && i < 3 && b[i] == b' ' {
        i += 1;
    }
    if i >= b.len() || b[i] != b'>' {
        return None;
    }
    while i < b.len() && b[i] == b'>' {
        i += 1;
        if i < b.len() && b[i] == b' ' {
            i += 1;
        }
    }
    Some(i)
}

/// End offset of a list bullet including its trailing space, if any.
///
/// Public as [`list_marker_end`] so the list-editing commands work from the
/// same idea of what a bullet is as the highlighter draws.
pub fn list_marker_end(line: &str) -> Option<usize> {
    list_at(line)
}

fn list_at(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    let after = match b[i] {
        b'-' | b'*' | b'+' => i + 1,
        b'0'..=b'9' => {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j - i > 9 {
                return None;
            }
            if j < b.len() && (b[j] == b'.' || b[j] == b')') {
                j + 1
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if after < b.len() && (b[after] == b' ' || b[after] == b'\t') {
        Some(after + 1)
    } else {
        None
    }
}

/// `(end, style)` for a `[ ]` / `[x]` task box at the head of `rest`.
fn task_at(rest: &str) -> Option<(usize, MdStyle)> {
    let b = rest.as_bytes();
    if b.len() < 3 || b[0] != b'[' || b[2] != b']' {
        return None;
    }
    let style = match b[1] {
        b' ' => MdStyle::TaskOpen,
        b'x' | b'X' => MdStyle::TaskDone,
        _ => return None,
    };
    let end = if b.len() > 3 && b[3] == b' ' { 4 } else { 3 };
    Some((end, style))
}

/// What pressing Enter should do to the list or quote a line is part of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continuation {
    /// Begin the next line with this prefix.
    Next(String),
    /// The item is empty and at the margin, so writing another marker would only
    /// leave litter. Clear the line instead, which is how a list is left.
    Clear,
    /// The item is empty but *nested*, so Enter steps it out one level rather
    /// than abandoning every level of structure at once. Replace the current
    /// line with this less-indented marker; the next Enter steps out again, and
    /// eventually [`Clear`](Continuation::Clear)s at the margin.
    Outdent(String),
}

/// How Enter should carry on the structure `line` starts, with the caret at
/// byte offset `caret` within it. `None` means Enter just breaks the line.
///
/// Typing the second item of a list by hand is the kind of chore an editor is
/// supposed to absorb, and leaving the list has to be just as easy — hence
/// [`Continuation::Clear`], which turns a second Enter into "I'm done".
pub fn continuation(line: &str, caret: usize) -> Option<Continuation> {
    // `- - -` is a thematic break that happens to start like a bullet, and the
    // highlighter already ranks it as one. Carrying it on as a list would put a
    // stray `- ` at the head of the next note.
    if is_separator(line) {
        return None;
    }
    let Some((prefix, marker_end)) = marker(line) else {
        // Not a list, but indentation is structure too: a wrapped thought or a
        // hand-made outline should not jump back to the margin on Enter.
        let indent = crate::lists::indent_of(line);
        if indent.is_empty() || caret < indent.len() || line.trim().is_empty() {
            return None;
        }
        return Some(Continuation::Next(indent.to_string()));
    };
    // Inside the marker itself there is no item yet to continue.
    if caret < marker_end {
        return None;
    }
    if line[marker_end..].trim().is_empty() {
        // An empty item. A *nested* one steps out one level per Enter — a
        // mis-nested bullet is walked back to the margin one press at a time,
        // rather than losing the whole outline in one keystroke. Only at the
        // margin, where there is nothing left to outdent, does Enter clear the
        // line and leave the list.
        if crate::lists::can_outdent(line) {
            return Some(Continuation::Outdent(crate::lists::outdent(line)));
        }
        return Some(Continuation::Clear);
    }
    Some(Continuation::Next(prefix))
}

/// The prefix that carries a line's structure onto the next one, and the byte
/// offset where that structure ends.
fn marker(line: &str) -> Option<(String, usize)> {
    if let Some(end) = quote_at(line) {
        // A quote can hold a list, so carry both. The remainder no longer
        // starts with `>`, so this recurses at most one level.
        return Some(match marker(&line[end..]) {
            Some((inner, inner_end)) => (format!("{}{inner}", &line[..end]), end + inner_end),
            None => (line[..end].to_string(), end),
        });
    }

    let end = list_at(line)?;
    let mut prefix = next_bullet(&line[..end]);
    let mut marker_end = end;
    if let Some((task_len, _)) = task_at(&line[end..]) {
        marker_end += task_len;
        // A carried-over task starts unticked whether or not this one is.
        prefix.push_str("[ ] ");
    }
    Some((prefix, marker_end))
}

/// The bullet that follows `bullet`: the same one, except that an ordered list
/// counts up.
fn next_bullet(bullet: &str) -> String {
    let indent_end = bullet.len() - bullet.trim_start_matches([' ', '\t']).len();
    let (indent, rest) = bullet.split_at(indent_end);
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let Ok(number) = rest[..digits_end].parse::<u64>() else {
        return bullet.to_string();
    };
    format!("{indent}{}{}", number.saturating_add(1), &rest[digits_end..])
}

// ---------------------------------------------------------------------------
// Line entry point
// ---------------------------------------------------------------------------

/// Highlight a single line. `in_code_block` is true when the PREVIOUS lines put
/// us inside a fenced code block (the fence line itself is passed with the state
/// that was active BEFORE it).
pub fn highlight_line(line: &str, in_code_block: bool) -> Vec<Span> {
    if line.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Span> = Vec::new();
    build_line(line, in_code_block, &mut out);
    merge(&mut out);
    debug_assert!(covers(line, &out));
    out
}

fn build_line(line: &str, in_code_block: bool, out: &mut Vec<Span>) {
    let len = line.len();

    if is_fence(line) {
        out.push(Span::new(0, len, MdStyle::Fence));
        return;
    }
    if in_code_block {
        out.push(Span::new(0, len, MdStyle::CodeBlock));
        return;
    }
    if is_separator(line) {
        out.push(Span::new(0, len, MdStyle::Separator));
        return;
    }
    if let Some((level, marker_end)) = heading_at(line) {
        out.push(Span::new(0, marker_end, MdStyle::HeadingMarker(level)));
        if marker_end < len {
            // Inline markup nests inside a heading: `# **bold** and `code`` is
            // parsed like any other line, and only its plain prose takes the
            // heading style. Without this the `**`/`` ` `` show through as
            // literal punctuation at heading size.
            highlight_wrapped(&line[marker_end..], marker_end, MdStyle::Heading(level), 0, out);
        }
        return;
    }
    if let Some(marker_end) = quote_at(line) {
        out.push(Span::new(0, marker_end, MdStyle::QuoteMarker));
        if marker_end < len {
            // The remainder no longer starts with `>`, so this recurses at most
            // one level.
            let inner = highlight_line(&line[marker_end..], false);
            for mut s in inner {
                s.start += marker_end;
                s.end += marker_end;
                if s.style == MdStyle::Text {
                    s.style = MdStyle::Quote;
                }
                out.push(s);
            }
        }
        return;
    }
    if let Some(marker_end) = list_at(line) {
        out.push(Span::new(0, marker_end, MdStyle::ListMarker));
        let mut at = marker_end;
        let done = matches!(task_at(&line[at..]), Some((_, MdStyle::TaskDone)));
        if let Some((task_len, style)) = task_at(&line[at..]) {
            out.push(Span::new(at, at + task_len, style));
            at += task_len;
        }
        if at < len {
            let from = out.len();
            highlight_inline(&line[at..], at, 0, out);
            // A ticked box changed four characters and left the sentence
            // identical to an open one, so the result of ⌘⏎ was invisible while
            // scanning. The whole content is struck, not only its plain prose:
            // `- [x] fix `bug`` reads as done end to end, its code and emphasis
            // included. Only the bullet and the box keep their own styling, so
            // the line still reads as a task. The flat style set has no
            // "struck code", so the sub-styles collapse into one strike — the
            // struck-out look wins, which is the point of completing.
            if done {
                for span in &mut out[from..] {
                    span.style = MdStyle::Strikethrough;
                }
            }
        }
        return;
    }
    highlight_inline(line, 0, 0, out);
}

/// Highlight a whole document, tracking fence state across lines.
/// `out[i]` corresponds to `lines[i]`.
pub fn highlight_document(lines: &[&str]) -> Vec<Vec<Span>> {
    let mut in_code_block = false;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.push(highlight_line(line, in_code_block));
        if is_fence(line) {
            in_code_block = !in_code_block;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Inline scanner
// ---------------------------------------------------------------------------

#[inline]
fn run_len(b: &[u8], i: usize, c: u8) -> usize {
    let mut j = i;
    while j < b.len() && b[j] == c {
        j += 1;
    }
    j - i
}

#[inline]
fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// Is the char immediately before byte offset `i` alphanumeric?
fn alnum_before(s: &str, i: usize) -> bool {
    s[..i]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
}

/// Is the char starting at byte offset `i` alphanumeric?
fn alnum_at(s: &str, i: usize) -> bool {
    if i >= s.len() {
        return false;
    }
    s[i..].chars().next().is_some_and(|c| c.is_alphanumeric())
}

/// Find a run of exactly `need` copies of `c` at or after `from`.
fn find_exact_run(b: &[u8], from: usize, c: u8, need: usize) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        if b[j] == c {
            let r = run_len(b, j, c);
            if r == need {
                return Some(j);
            }
            j += r;
        } else {
            j += 1;
        }
    }
    None
}

/// Find a closing emphasis run of at least `need` copies of `c` at or after
/// `from`. `underscore` enables the intra-word guard.
fn find_emph_close(s: &str, from: usize, c: u8, need: usize, underscore: bool) -> Option<usize> {
    let b = s.as_bytes();
    let mut j = from;
    while j < b.len() {
        if b[j] == c {
            let r = run_len(b, j, c);
            if r >= need && j > from && !is_space(b[j - 1]) && (!underscore || !alnum_at(s, j + r))
            {
                return Some(j);
            }
            j += r;
        } else {
            j += 1;
        }
    }
    None
}

/// `(close_bracket, close_paren)` for a `[text](url)` starting at `open`.
fn find_link(b: &[u8], open: usize) -> Option<(usize, usize)> {
    let mut rb = open + 1;
    while rb < b.len() && b[rb] != b']' {
        // Bail on a nested `[` rather than mis-pairing across it.
        if b[rb] == b'[' {
            return None;
        }
        rb += 1;
    }
    if rb >= b.len() || rb + 1 >= b.len() || b[rb + 1] != b'(' {
        return None;
    }
    let mut rp = rb + 2;
    while rp < b.len() && b[rp] != b')' {
        rp += 1;
    }
    if rp >= b.len() {
        return None;
    }
    Some((rb, rp))
}

/// End offset of a bare `http://` / `https://` run starting at `i`.
fn autolink_end(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let scheme = if rest.starts_with("http://") {
        7
    } else if rest.starts_with("https://") {
        8
    } else {
        return None;
    };
    if alnum_before(s, i) {
        return None;
    }
    let b = s.as_bytes();
    let mut end = i + scheme;
    while end < b.len() {
        let c = b[end];
        if c == b' ' || c == b'\t' || c == b'<' || c == b'>' || c == b'"' || c == b'`' {
            break;
        }
        end += 1;
    }
    // Don't swallow sentence punctuation that trails the url.
    while end > i + scheme
        && matches!(
            b[end - 1],
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b')' | b']' | b'}' | b'\''
        )
    {
        end -= 1;
    }
    Some(end)
}

/// The URL to open for a click at byte `offset` within `line`, if the offset
/// lands on a link. Covers a `[text](url)` anywhere from the opening `[` through
/// the closing `)`, and a bare `http(s)://…` autolink. It mirrors the same
/// parsers [`highlight_line`] paints with, so what reads as a link is exactly
/// what opens. `None` when the offset is on ordinary text.
pub fn link_at(line: &str, offset: usize) -> Option<String> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // An inline `[text](url)`: clickable across the whole construct.
        if b[i] == b'[' {
            if let Some((rb, rp)) = find_link(b, i) {
                if (i..=rp).contains(&offset) {
                    let url = line[rb + 2..rp].trim();
                    if !url.is_empty() {
                        return Some(url.to_string());
                    }
                }
                i = rp + 1;
                continue;
            }
        }
        // A bare autolink: the run itself is the URL. `end` is exclusive (and
        // trailing punctuation is already trimmed off it), so the range is
        // half-open — a click on the space or period just past the URL is not on
        // the link.
        if b[i] == b'h' {
            if let Some(end) = autolink_end(line, i) {
                if (i..end).contains(&offset) {
                    return Some(line[i..end].to_string());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn push_text(out: &mut Vec<Span>, base: usize, start: usize, end: usize) {
    if start < end {
        out.push(Span::new(base + start, base + end, MdStyle::Text));
    }
}

/// How deep inline markup may nest before the scanner stops recursing and draws
/// the remaining content flat. Real markdown never nests near this; the cap
/// only exists so a pathological line — `*_*_…_*_*` thousands of levels deep —
/// cannot recurse the stack into an overflow, keeping [`highlight_line`] total
/// on any input (invariant 5).
const MAX_INLINE_NEST: u8 = 8;

/// Highlight `inner` (a substring at byte offset `base` within the line) and
/// append its spans, recolouring only its plain [`MdStyle::Text`] to `wrap`.
///
/// This is how inline markup nests: code, links and further emphasis found
/// inside a heading, a link label or an emphasis run keep their own style,
/// while the surrounding prose takes the wrapper's. It is the inline-scanner
/// twin of the whole-line restyling the quote and done-task branches already
/// do. `inner` is non-empty at every call site, so it always contributes at
/// least one span and the caller's cover stays gapless.
///
/// At [`MAX_INLINE_NEST`] it stops recursing and draws `inner` as one flat span
/// — the pre-nesting behaviour — which still tiles.
fn highlight_wrapped(inner: &str, base: usize, wrap: MdStyle, depth: u8, out: &mut Vec<Span>) {
    if depth >= MAX_INLINE_NEST {
        out.push(Span::new(base, base + inner.len(), wrap));
        return;
    }
    let from = out.len();
    highlight_inline(inner, base, depth + 1, out);
    for span in &mut out[from..] {
        if span.style == MdStyle::Text {
            span.style = wrap;
        }
    }
}

/// Scan `s` left to right, appending contiguous spans covering
/// `base..base + s.len()`. `depth` is the inline-nesting level, threaded so
/// [`highlight_wrapped`] can cap recursion (see [`MAX_INLINE_NEST`]).
fn highlight_inline(s: &str, base: usize, depth: u8, out: &mut Vec<Span>) {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < n {
        match b[i] {
            // ---- inline code -------------------------------------------------
            b'`' => {
                let run = run_len(b, i, b'`');
                if let Some(j) = find_exact_run(b, i + run, b'`', run) {
                    push_text(out, base, text_start, i);
                    // Backticks included: a chip that starts after the opening
                    // tick and stops before the closing one is the ragged edge
                    // the fenced block was fixed for.
                    out.push(Span::new(base + i, base + j + run, MdStyle::Code));
                    i = j + run;
                    text_start = i;
                } else {
                    i += run;
                }
            }

            // ---- emphasis ----------------------------------------------------
            c @ (b'*' | b'_' | b'~' | b'=') => {
                let run = run_len(b, i, c);
                let underscore = c == b'_';
                let paired = c == b'~' || c == b'=';
                let max = if paired { 2 } else { run.min(3) };
                let min = if paired { 2 } else { 1 };
                let opener_ok = run >= min
                    && i + run < n
                    && !is_space(b[i + run])
                    && !(underscore && alnum_before(s, i));

                let mut matched = false;
                if opener_ok {
                    let mut need = max.min(run);
                    while need >= min {
                        if let Some(j) = find_emph_close(s, i + need, c, need, underscore) {
                            let style = match (c, need) {
                                (b'~', _) => MdStyle::Strikethrough,
                                (b'=', _) => MdStyle::Highlight,
                                (_, 3) => MdStyle::BoldItalic,
                                (_, 2) => MdStyle::Bold,
                                _ => MdStyle::Italic,
                            };
                            push_text(out, base, text_start, i);
                            out.push(Span::new(base + i, base + i + need, MdStyle::Marker));
                            // The content nests: `**`code`**` keeps its code
                            // chip, `*a `b` c*` styles the code and italicises
                            // the rest. The close is strictly after the open, so
                            // this inner slice is never empty.
                            highlight_wrapped(&s[i + need..j], base + i + need, style, depth, out);
                            out.push(Span::new(base + j, base + j + need, MdStyle::Marker));
                            i = j + need;
                            text_start = i;
                            matched = true;
                            break;
                        }
                        need -= 1;
                    }
                }
                if !matched {
                    i += run;
                }
            }

            // ---- links and images --------------------------------------------
            b'!' | b'[' => {
                let bang = b[i] == b'!';
                let open = if bang { i + 1 } else { i };
                let link = if !bang || (open < n && b[open] == b'[') {
                    find_link(b, open)
                } else {
                    None
                };
                if let Some((rb, rp)) = link {
                    push_text(out, base, text_start, i);
                    // `!` + `[`
                    out.push(Span::new(base + i, base + open + 1, MdStyle::Marker));
                    if rb > open + 1 {
                        // The label nests too, so `[*a*](b)` italicises its `a`.
                        // `find_link` bars a nested `[`, so no link recurses
                        // inside a link.
                        highlight_wrapped(
                            &s[open + 1..rb],
                            base + open + 1,
                            MdStyle::LinkText,
                            depth,
                            out,
                        );
                    }
                    // `](`
                    out.push(Span::new(base + rb, base + rb + 2, MdStyle::Marker));
                    if rp > rb + 2 {
                        out.push(Span::new(base + rb + 2, base + rp, MdStyle::Marker));
                    }
                    out.push(Span::new(base + rp, base + rp + 1, MdStyle::Marker));
                    i = rp + 1;
                    text_start = i;
                } else {
                    i += 1;
                }
            }

            // ---- bare urls -----------------------------------------------------
            b'h' => {
                if let Some(end) = autolink_end(s, i) {
                    push_text(out, base, text_start, i);
                    out.push(Span::new(base + i, base + end, MdStyle::LinkUrl));
                    i = end;
                    text_start = i;
                } else {
                    i += 1;
                }
            }

            _ => i += 1,
        }
    }

    push_text(out, base, text_start, n);
}

// ---------------------------------------------------------------------------
// Post-processing
// ---------------------------------------------------------------------------

/// Collapse adjacent spans that share a style.
fn merge(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    let mut write = 0usize;
    for read in 1..spans.len() {
        let cur = spans[read];
        if spans[write].style == cur.style && spans[write].end == cur.start {
            spans[write].end = cur.end;
        } else {
            write += 1;
            spans[write] = cur;
        }
    }
    spans.truncate(write + 1);
}

/// Debug-only invariant check used by `debug_assert!`.
fn covers(line: &str, spans: &[Span]) -> bool {
    if line.is_empty() {
        return spans.is_empty();
    }
    if spans.is_empty() || spans[0].start != 0 || spans[spans.len() - 1].end != line.len() {
        return false;
    }
    for (i, s) in spans.iter().enumerate() {
        if s.start >= s.end || !line.is_char_boundary(s.start) || !line.is_char_boundary(s.end) {
            return false;
        }
        if i + 1 < spans.len() && s.end != spans[i + 1].start {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Adversarial corpus. Every entry is checked against invariants 1-5 for
    /// both values of `in_code_block`.
    const CASES: &[&str] = &[
        "",
        " ",
        "   ",
        "\t",
        "plain text",
        "trailing spaces   ",
        "   leading spaces",
        "*",
        "**",
        "***",
        "****",
        "*****",
        "*a*",
        "**a**",
        "***bold italic***",
        "***unclosed",
        "**a*",
        "*a**",
        "a * b * c",
        "_",
        "__",
        "___",
        "_x_",
        "__x__",
        "___x___",
        "snake_case_ident",
        "a_b_c_d",
        "_leading underscore",
        "`code`",
        "`",
        "``",
        "```",
        "~~~",
        "~~~rust",
        "```rust",
        "```rust```",
        "`` a `` b",
        "`unterminated",
        "a `b` c `d",
        "~",
        "~~",
        "~~strike~~",
        "~~unclosed",
        "[a](b)",
        "[a](",
        "[a]",
        "[",
        "]",
        "[](",
        "[]()",
        "![img](u)",
        "![](u)",
        "!not a link",
        "[nested [x]](y)",
        "[a](b) and [c](d)",
        "- [ ] todo",
        "- [x] done",
        "- [X] DONE",
        "- [ ]",
        "* [ ] star todo",
        "+ plus item",
        "- item",
        "- ",
        "-",
        "1. item",
        "12) item",
        "1.",
        "99999999999999. too many digits",
        "  - nested item",
        "\t- tabbed item",
        "> quote",
        ">> nested",
        "> > spaced nested",
        ">",
        "> ",
        "> **bold** in quote",
        "> - list in quote",
        "> # heading in quote",
        "# h1",
        "## h2",
        "### h3",
        "#### h4",
        "##### h5",
        "###### h6",
        "####### seven hashes",
        "#nohash",
        "#",
        "###",
        "#  ",
        "   # indented heading",
        "    # over-indented",
        "---",
        "***",
        "___",
        "- - -",
        "* * *",
        "_ _ _",
        "--",
        "----------",
        "http://x.y",
        "https://example.com/a?b=c#d",
        "see https://example.com. done",
        "nohttp://x.y",
        "http://",
        "**a** _b_ `c`",
        "mixed **bold `code` inside** tail",
        "emoji 🎉🎉 *star* 🚀",
        "👨‍👩‍👧‍👦 family",
        "中文字符测试",
        "**中文粗体**",
        "`日本語コード`",
        "e\u{301}\u{301}combining",
        "a\u{0301}*b*",
        "ünïcödé _emphasis_",
        "🎉*🎉*🎉",
        "> 🎉 **quote** with emoji",
        "- [ ] 中文 todo `code`",
        "\u{200b}zero width",
        "text with ) and ( parens",
        "|table|cell|",
        "<html>tag</html>",
        "&amp; entity",
        "\\*escaped\\*",
        "a*b*c",
        "**",
        "*_*_*",
        "[*a*](b)",
        "**[link](url)**",
        // Nested inline markup (item 38): every one of these must still tile.
        "# **bold**",
        "# a `code` b",
        "## _em_ and `c`",
        "### **[x](y)** z",
        "**`code`**",
        "*a `b` c*",
        "_`c`_",
        "~~`s`~~",
        "**_a_**",
        "**_`x`_**",
        "***`nested`***",
        "**[*a*](b)**",
        "`**not bold**`",
        "> # **q**",
        "> **`c`**",
        "- [x] fix `bug`",
        "- [x] **done** and `code`",
        "# `*`",
        "**",
        "*`*`*",
        "[`a`](b)",
        "# ",
        "***`***",
    ];

    fn check(line: &str, in_code_block: bool) {
        let spans = highlight_line(line, in_code_block);

        // 1. empty line -> empty spans
        if line.is_empty() {
            assert!(spans.is_empty(), "empty line produced spans");
            return;
        }
        assert!(
            !spans.is_empty(),
            "non-empty line {:?} produced no spans",
            line
        );

        // 2. contiguous exact cover
        assert_eq!(spans[0].start, 0, "line {:?} does not start at 0", line);
        assert_eq!(
            spans[spans.len() - 1].end,
            line.len(),
            "line {:?} does not end at len",
            line
        );
        for w in spans.windows(2) {
            assert_eq!(
                w[0].end, w[1].start,
                "gap/overlap in {:?}: {:?}",
                line, spans
            );
            assert!(w[0].start < w[1].start, "unsorted in {:?}", line);
        }

        for s in &spans {
            // 3. char boundaries
            assert!(
                line.is_char_boundary(s.start) && line.is_char_boundary(s.end),
                "non-boundary span {:?} in {:?}",
                s,
                line
            );
            // 4. non-empty
            assert!(s.start < s.end, "empty span {:?} in {:?}", s, line);
            // slicing must not panic
            let _ = &line[s.start..s.end];
        }

        // adjacent spans never share a style
        for w in spans.windows(2) {
            assert_ne!(w[0].style, w[1].style, "unmerged run in {:?}", line);
        }
    }

    fn check_both(line: &str) {
        check(line, false);
        check(line, true);
    }

    #[test]
    fn corpus_holds_invariants() {
        assert!(CASES.len() >= 60, "corpus too small: {}", CASES.len());
        for case in CASES {
            check_both(case);
        }
    }

    #[test]
    fn corpus_prefixes_and_suffixes_hold_invariants() {
        // Every char-boundary prefix and suffix of every case is also a line.
        for case in CASES {
            for (i, _) in case.char_indices() {
                check_both(&case[..i]);
                check_both(&case[i..]);
            }
        }
    }

    #[test]
    fn corpus_pairs_hold_invariants() {
        for a in CASES.iter().take(40) {
            for b in CASES.iter().take(40) {
                let joined = format!("{} {}", a, b);
                check_both(&joined);
                let tight = format!("{}{}", a, b);
                check_both(&tight);
            }
        }
    }

    #[test]
    fn very_long_lines() {
        let long: String = "*".repeat(5000);
        check_both(&long);

        let mut mixed = String::new();
        for i in 0..1000 {
            mixed.push_str(match i % 7 {
                0 => "**bold** ",
                1 => "`code` ",
                2 => "[a](b) ",
                3 => "🎉中文 ",
                4 => "_em_ ",
                5 => "~~s~~ ",
                _ => "plain ",
            });
        }
        assert!(mixed.len() > 4000);
        check_both(&mixed);

        check_both(&"a".repeat(5000));
        check_both(&"`".repeat(5000));
        // Deeply nested inline markup must not recurse the stack into an
        // overflow: the scanner caps nesting and draws the rest flat, but the
        // cover still has to tile. `*_*_…x…_*_*` alternates delimiters so each
        // pair genuinely nests inside the last.
        let deep = format!("{}x{}", "*_".repeat(4000), "_*".repeat(4000));
        check_both(&deep);
        check_both(&format!("# {}", "*_".repeat(4000)));
        check_both(&"[".repeat(2000));
        check_both(&"🎉".repeat(2000));
        check_both(&format!("> {}", "_".repeat(3000)));
        check_both(&format!("- [ ] {}", "*x* ".repeat(1000)));
    }

    #[test]
    fn pseudo_random_soup() {
        // Deterministic LCG over a nasty alphabet.
        const ALPHABET: &[&str] = &[
            "*", "_", "`", "~", "[", "]", "(", ")", "!", "#", ">", "-", "+", "1", ".", " ", "a",
            "🎉", "中", "\u{301}", "h", "t", "p", ":", "/", "\t", "x", "\\",
        ];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..4000 {
            let mut line = String::new();
            let len = {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as usize % 40
            };
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                line.push_str(ALPHABET[(state >> 33) as usize % ALPHABET.len()]);
            }
            check_both(&line);
        }
    }

    // -- behaviour ---------------------------------------------------------

    fn styles(line: &str) -> Vec<(usize, usize, MdStyle)> {
        highlight_line(line, false)
            .into_iter()
            .map(|s| (s.start, s.end, s.style))
            .collect()
    }

    #[test]
    fn empty_line_is_empty() {
        assert!(highlight_line("", false).is_empty());
        assert!(highlight_line("", true).is_empty());
    }

    #[test]
    fn fences() {
        assert!(is_fence("```"));
        assert!(is_fence("```rust"));
        assert!(is_fence("~~~"));
        assert!(is_fence("~~~toml"));
        assert!(is_fence("   ```"));
        assert!(!is_fence("    ```"));
        assert!(!is_fence("``"));
        assert!(!is_fence("```a```"));
        assert!(!is_fence("text"));

        assert_eq!(styles("```rust"), vec![(0, 7, MdStyle::Fence)]);
        assert_eq!(
            highlight_line("```", true),
            vec![Span::new(0, 3, MdStyle::Fence)]
        );
        assert_eq!(
            highlight_line("# not a heading here", true),
            vec![Span::new(0, 20, MdStyle::CodeBlock)]
        );
    }

    #[test]
    fn separators() {
        assert!(is_separator("---"));
        assert!(is_separator("***"));
        assert!(is_separator("___"));
        assert!(is_separator("- - -"));
        assert!(is_separator("  ***  "));
        assert!(!is_separator("--"));
        assert!(!is_separator("-*-"));
        assert!(!is_separator(""));
        assert!(!is_separator("   "));
        assert!(!is_separator("---a"));
        assert_eq!(styles("---"), vec![(0, 3, MdStyle::Separator)]);
    }

    #[test]
    fn headings() {
        assert_eq!(
            styles("# h1"),
            vec![
                (0, 2, MdStyle::HeadingMarker(1)),
                (2, 4, MdStyle::Heading(1))
            ]
        );
        assert_eq!(
            styles("###### h6"),
            vec![
                (0, 7, MdStyle::HeadingMarker(6)),
                (7, 9, MdStyle::Heading(6))
            ]
        );
        assert_eq!(styles("#"), vec![(0, 1, MdStyle::HeadingMarker(1))]);
        assert_eq!(styles("#nohash"), vec![(0, 7, MdStyle::Text)]);
        assert_eq!(styles("####### x"), vec![(0, 9, MdStyle::Text)]);
    }

    #[test]
    fn quotes() {
        let s = styles("> hi");
        assert_eq!(s[0], (0, 2, MdStyle::QuoteMarker));
        assert_eq!(s[1], (2, 4, MdStyle::Quote));

        let s = styles(">> deep");
        assert_eq!(s[0].2, MdStyle::QuoteMarker);
        assert_eq!(s[0].1, 3);

        // Text inside a quote becomes Quote, but markup keeps its own style.
        let s = styles("> **b**");
        assert_eq!(s[0].2, MdStyle::QuoteMarker);
        assert!(s.iter().any(|x| x.2 == MdStyle::Bold));
        assert_eq!(styles(">"), vec![(0, 1, MdStyle::QuoteMarker)]);
    }

    #[test]
    fn lists_and_tasks() {
        assert_eq!(
            styles("- item"),
            vec![(0, 2, MdStyle::ListMarker), (2, 6, MdStyle::Text)]
        );
        assert_eq!(
            styles("1. item"),
            vec![(0, 3, MdStyle::ListMarker), (3, 7, MdStyle::Text)]
        );
        assert_eq!(
            styles("12) item"),
            vec![(0, 4, MdStyle::ListMarker), (4, 8, MdStyle::Text)]
        );
        assert_eq!(
            styles("- [ ] todo"),
            vec![
                (0, 2, MdStyle::ListMarker),
                (2, 6, MdStyle::TaskOpen),
                (6, 10, MdStyle::Text)
            ]
        );
        assert_eq!(
            styles("- [x] done"),
            vec![
                (0, 2, MdStyle::ListMarker),
                (2, 6, MdStyle::TaskDone),
                // Struck through: the point of ⌘⏎ is that you can see it.
                (6, 10, MdStyle::Strikethrough)
            ]
        );
        assert_eq!(styles("- "), vec![(0, 2, MdStyle::ListMarker)]);
        assert_eq!(styles("-"), vec![(0, 1, MdStyle::Text)]);
    }

    /// A run of indented lines is one block; the app's own two-space outline is
    /// not code, however deep it goes.
    #[test]
    fn indented_code_leaves_outlines_alone() {
        assert!(is_indented_code("    print(1)", true));
        assert!(is_indented_code("\tprint(1)", true));
        // Not the first line of a run.
        assert!(!is_indented_code("    print(1)", false));
        // Two spaces a level is this app's indent, not code.
        assert!(!is_indented_code("  overview", true));
        // A list item at any depth stays a list item.
        assert!(!is_indented_code("    - a nested bullet", true));
        assert!(!is_indented_code("      1. a nested number", true));
        assert!(!is_indented_code("    > a quote", true));
        assert!(!is_indented_code("    # a heading", true));
        assert!(!is_indented_code("    ```", true));
        assert!(!is_indented_code("        ", true), "blank is not code");
    }

    #[test]
    fn quote_depth_counts_sigils() {
        assert_eq!(quote_depth("no quote"), 0);
        assert_eq!(quote_depth("> one"), 1);
        assert_eq!(quote_depth("> > two"), 2);
        assert_eq!(quote_depth(">>> three"), 3);
        assert_eq!(quote_depth("  > indented one"), 1);
    }

    /// `==marked==` is the extension a note app is expected to have.
    #[test]
    fn highlight_marks() {
        let s = styles("say ==this== please");
        assert!(
            s.iter().any(|x| x.2 == MdStyle::Highlight),
            "no highlight span in {s:?}"
        );
        // A single `=` is arithmetic, not markup.
        let plain = styles("a = b");
        assert!(plain.iter().all(|x| x.2 != MdStyle::Highlight));
    }

    /// A run of `=` underlines the line above into a heading; a run of `-` is
    /// this app's note separator and deliberately stays one.
    #[test]
    fn only_equals_underlines_a_heading() {
        assert!(
            !takes_setext_underline("---"),
            "a rule is not something an underline turns into a heading"
        );
        assert!(!takes_setext_underline("***"));
        assert_eq!(setext_level("==="), Some(1));
        assert_eq!(setext_level("  ====  "), Some(1));
        assert_eq!(setext_level("---"), None, "a dash run is a rule here");
        assert_eq!(setext_level(""), None);
        assert_eq!(setext_level("=a="), None);

        assert!(takes_setext_underline("A title"));
        assert!(!takes_setext_underline(""));
        assert!(!takes_setext_underline("# already a heading"));
        assert!(!takes_setext_underline("- a list item"));
        assert!(!takes_setext_underline("> a quote"));
        assert!(!takes_setext_underline("```"));
    }

    #[test]
    fn inline_code() {
        assert_eq!(
            styles("`x`"),
            vec![
                // One span: the chip includes its own ticks, so its edge is not
                // ragged around the first and last character.
                (0, 3, MdStyle::Code)
            ]
        );
        assert_eq!(styles("`x"), vec![(0, 2, MdStyle::Text)]);
    }

    #[test]
    fn emphasis() {
        assert_eq!(
            styles("*i*"),
            vec![
                (0, 1, MdStyle::Marker),
                (1, 2, MdStyle::Italic),
                (2, 3, MdStyle::Marker)
            ]
        );
        assert_eq!(
            styles("**b**"),
            vec![
                (0, 2, MdStyle::Marker),
                (2, 3, MdStyle::Bold),
                (3, 5, MdStyle::Marker)
            ]
        );
        assert_eq!(
            styles("***bi***"),
            vec![
                (0, 3, MdStyle::Marker),
                (3, 5, MdStyle::BoldItalic),
                (5, 8, MdStyle::Marker)
            ]
        );
        assert_eq!(
            styles("~~s~~"),
            vec![
                (0, 2, MdStyle::Marker),
                (2, 3, MdStyle::Strikethrough),
                (3, 5, MdStyle::Marker)
            ]
        );
        // snake_case must stay plain
        assert_eq!(styles("snake_case_x"), vec![(0, 12, MdStyle::Text)]);
        assert!(styles("a_b_c").iter().all(|s| s.2 == MdStyle::Text));
        // but a word-boundary underscore emphasises
        assert!(styles("_x_").iter().any(|s| s.2 == MdStyle::Italic));
        // lone markers stay text
        assert_eq!(styles("*"), vec![(0, 1, MdStyle::Text)]);
        assert_eq!(styles("a * b"), vec![(0, 5, MdStyle::Text)]);
    }

    #[test]
    fn links() {
        assert_eq!(
            styles("[a](b)"),
            vec![
                (0, 1, MdStyle::Marker),
                (1, 2, MdStyle::LinkText),
                // The URL is plumbing, like the brackets around it: the label
                // is what a person reads, and it should not be the quieter of
                // the two. Being the same style, it merges with them.
                (2, 6, MdStyle::Marker)
            ]
        );
        assert_eq!(
            styles("![a](b)"),
            vec![
                (0, 2, MdStyle::Marker),
                (2, 3, MdStyle::LinkText),
                // As above: the url is markup now, so it merges with the
                // punctuation either side of it.
                (3, 7, MdStyle::Marker)
            ]
        );
        assert_eq!(styles("[a]("), vec![(0, 4, MdStyle::Text)]);
        assert_eq!(styles("[a]"), vec![(0, 3, MdStyle::Text)]);
    }

    #[test]
    fn inline_markup_nests() {
        // Emphasis, code and links parse inside a heading; only the prose takes
        // the heading style, the `**` no longer show through as literal text.
        let s = styles("# **bold**");
        assert_eq!(s[0].2, MdStyle::HeadingMarker(1));
        assert!(s.iter().any(|x| x.2 == MdStyle::Bold), "heading bold: {s:?}");
        // The `**` are dimmed markup now, not literal prose at heading size.
        assert!(s.iter().any(|x| x.2 == MdStyle::Marker), "dim markers: {s:?}");
        // A code span keeps its own style inside a heading.
        assert!(styles("# a `code` b")
            .iter()
            .any(|x| x.2 == MdStyle::Code));

        // Code nested inside emphasis keeps its chip rather than turning bold.
        let s = styles("**`code`**");
        assert_eq!(s[0].2, MdStyle::Marker);
        assert!(s.iter().any(|x| x.2 == MdStyle::Code), "code in bold: {s:?}");
        assert_eq!(s.last().unwrap().2, MdStyle::Marker);

        // Emphasis around code: the prose italicises, the code stays code.
        let s = styles("*a `b` c*");
        assert!(s.iter().any(|x| x.2 == MdStyle::Italic));
        assert!(s.iter().any(|x| x.2 == MdStyle::Code));

        // A link label emphasises its inner span.
        assert!(styles("[*a*](b)")
            .iter()
            .any(|x| x.2 == MdStyle::Italic));

        // Code is opaque: markup inside a code span stays literal code.
        assert_eq!(styles("`**not bold**`"), vec![(0, 14, MdStyle::Code)]);

        // A quoted heading nests too, and still exposes its HeadingMarker so the
        // renderer enlarges it (item 37).
        let s = styles("> # **q**");
        assert!(s.iter().any(|x| matches!(x.2, MdStyle::HeadingMarker(1))));
        assert!(s.iter().any(|x| x.2 == MdStyle::Bold));
    }

    #[test]
    fn a_done_task_strikes_all_of_its_content() {
        // Not only the plain prose: the code span is struck as well, so the
        // whole item reads as done. Bullet and box keep their own styling.
        assert_eq!(
            styles("- [x] fix `bug`"),
            vec![
                (0, 2, MdStyle::ListMarker),
                (2, 6, MdStyle::TaskDone),
                (6, 15, MdStyle::Strikethrough),
            ]
        );
        // Emphasis and code both collapse into the strike.
        let s = styles("- [x] **a** `b`");
        assert_eq!(s[0].2, MdStyle::ListMarker);
        assert_eq!(s[1].2, MdStyle::TaskDone);
        assert!(s[2..].iter().all(|x| x.2 == MdStyle::Strikethrough));
        assert_eq!(s.len(), 3, "content should merge to one strike: {s:?}");
        // An open task is untouched — its code still reads as code.
        assert!(styles("- [ ] fix `bug`")
            .iter()
            .any(|x| x.2 == MdStyle::Code));
    }

    #[test]
    fn autolinks() {
        assert_eq!(styles("http://x.y"), vec![(0, 10, MdStyle::LinkUrl)]);
        let s = styles("see https://a.b/c. ok");
        assert_eq!(s[0], (0, 4, MdStyle::Text));
        assert_eq!(s[1], (4, 17, MdStyle::LinkUrl));
        assert_eq!(s[2], (17, 21, MdStyle::Text));
        assert!(styles("nohttp://x.y").iter().all(|s| s.2 == MdStyle::Text));
    }

    #[test]
    fn link_at_offsets() {
        // A bare autolink is clickable across its whole run, but not the text
        // around it.
        let line = "see http://a.b/c ok";
        assert_eq!(link_at(line, 0), None);
        assert_eq!(link_at(line, 4).as_deref(), Some("http://a.b/c"));
        assert_eq!(link_at(line, 15).as_deref(), Some("http://a.b/c"));
        assert_eq!(link_at(line, 17), None);

        // An inline `[text](url)` opens the url from anywhere in the construct —
        // the visible text or the parenthesised url.
        let line = "a [docs](https://x.y/z) b";
        assert_eq!(link_at(line, 0), None); // "a "
        assert_eq!(link_at(line, 4).as_deref(), Some("https://x.y/z")); // in "docs"
        assert_eq!(link_at(line, 12).as_deref(), Some("https://x.y/z")); // in the url
        assert_eq!(link_at(line, 22).as_deref(), Some("https://x.y/z")); // the ')'
        assert_eq!(link_at(line, 24), None); // "b"

        // A relative or scheme-less url still resolves; the opener adds https://.
        assert_eq!(link_at("[x](/rel)", 1).as_deref(), Some("/rel"));
        // Not a link: an empty target, or plain text.
        assert_eq!(link_at("[x]()", 1), None);
        assert_eq!(link_at("plain text", 3), None);
    }

    #[test]
    fn document_tracks_fence_state() {
        let lines = ["# t", "```", "# not a heading", "```", "# t2"];
        let doc = highlight_document(&lines);
        assert_eq!(doc.len(), 5);
        assert_eq!(doc[0][0].style, MdStyle::HeadingMarker(1));
        assert_eq!(doc[1][0].style, MdStyle::Fence);
        assert_eq!(doc[2][0].style, MdStyle::CodeBlock);
        assert_eq!(doc[2].len(), 1);
        assert_eq!(doc[3][0].style, MdStyle::Fence);
        assert_eq!(doc[4][0].style, MdStyle::HeadingMarker(1));
    }

    #[test]
    fn document_invariants() {
        let doc_lines: Vec<&str> = CASES.to_vec();
        let doc = highlight_document(&doc_lines);
        assert_eq!(doc.len(), doc_lines.len());
        for (line, spans) in doc_lines.iter().zip(doc.iter()) {
            if line.is_empty() {
                assert!(spans.is_empty());
                continue;
            }
            assert_eq!(spans[0].start, 0);
            assert_eq!(spans[spans.len() - 1].end, line.len());
            for w in spans.windows(2) {
                assert_eq!(w[0].end, w[1].start);
            }
        }
    }

    #[test]
    fn empty_document() {
        assert!(highlight_document(&[]).is_empty());
        assert_eq!(highlight_document(&[""]), vec![Vec::<Span>::new()]);
    }

    /// Enter at the end of `line` continues it with `expect`.
    #[track_caller]
    fn continues(line: &str, expect: &str) {
        assert_eq!(
            continuation(line, line.len()),
            Some(Continuation::Next(expect.to_string())),
            "continuing {line:?}"
        );
    }

    /// Enter at the end of `line` leaves the list rather than continuing it.
    #[track_caller]
    fn clears(line: &str) {
        assert_eq!(
            continuation(line, line.len()),
            Some(Continuation::Clear),
            "clearing {line:?}"
        );
    }

    #[test]
    fn bullets_carry_over() {
        continues("- milk", "- ");
        continues("* milk", "* ");
        continues("+ milk", "+ ");
        continues("   - indented", "   - ");
        continues("\t- tabbed", "\t- ");
    }

    #[test]
    fn ordered_lists_count_up() {
        continues("1. first", "2. ");
        continues("9. ninth", "10. ");
        continues("1) paren", "2) ");
        continues("  12. indented", "  13. ");
    }

    #[test]
    fn tasks_carry_over_unticked() {
        continues("- [ ] open", "- [ ] ");
        continues("- [x] done", "- [ ] ");
        continues("- [X] done", "- [ ] ");
        continues("2. [x] numbered task", "3. [ ] ");
    }

    #[test]
    fn quotes_carry_over_and_keep_their_list() {
        continues("> quoted", "> ");
        continues(">> twice", ">> ");
        continues("> - quoted bullet", "> - ");
        continues("> 1. quoted item", "> 2. ");
    }

    #[test]
    fn an_empty_item_ends_the_list() {
        clears("- ");
        clears("1. ");
        clears("- [ ] ");
        clears("- [x] ");
        clears("> ");
        clears("> - ");
        // Whitespace after the marker is still an empty item.
        clears("-    ");
    }

    /// Enter at the end of `line` steps a nested empty item out one level.
    #[track_caller]
    fn outdents(line: &str, expect: &str) {
        assert_eq!(
            continuation(line, line.len()),
            Some(Continuation::Outdent(expect.to_string())),
            "outdenting {line:?}"
        );
    }

    #[test]
    fn an_empty_nested_item_outdents_one_level_before_it_clears() {
        // A single level of nesting: outdent to the margin, then clear.
        outdents("  - ", "- ");
        assert_eq!(continuation("- ", 2), Some(Continuation::Clear));
        // Two levels come out one press at a time.
        outdents("    - ", "  - ");
        outdents("  1. ", "1. ");
        // A hand-made tab is one whole level.
        outdents("\t- ", "- ");
        // A task box nested one level.
        outdents("  - [ ] ", "- [ ] ");
        // Trailing whitespace is still an empty item; it outdents too.
        outdents("  -    ", "-    ");
        // A partial indent still steps out to the margin.
        outdents(" - ", "- ");
        // A non-empty nested item carries its marker as before, never outdents.
        continues("  - milk", "  - ");
    }

    #[test]
    fn enter_keeps_the_indentation_you_are_working_at() {
        assert_eq!(
            continuation("    a nested thought", 20),
            Some(Continuation::Next("    ".to_string()))
        );
        assert_eq!(
            continuation("\tby a tab", 9),
            Some(Continuation::Next("\t".to_string()))
        );
        // At the margin there is nothing to carry.
        assert_eq!(continuation("at the margin", 13), None);
        // A blank line is not an indent to preserve.
        assert_eq!(continuation("     ", 5), None);
        // Inside the indentation itself, Enter just breaks the line.
        assert_eq!(continuation("    text", 2), None);
    }

    #[test]
    fn a_thematic_break_is_not_a_list_to_continue() {
        for rule in ["---", "- - -", "***", "* * *", "___", "  ---  "] {
            assert_eq!(continuation(rule, rule.len()), None, "rule {rule:?}");
        }
    }

    #[test]
    fn plain_lines_are_left_alone() {
        assert_eq!(continuation("", 0), None);
        assert_eq!(continuation("just text", 9), None);
        assert_eq!(continuation("# Heading", 9), None);
        assert_eq!(continuation("-no space", 9), None);
        assert_eq!(continuation("2024. not a list", 16), Some(Continuation::Next("2025. ".into())));
    }

    #[test]
    fn the_caret_inside_the_marker_just_breaks_the_line() {
        assert_eq!(continuation("- milk", 0), None);
        assert_eq!(continuation("- milk", 1), None);
        // At the end of the marker there is an item to continue.
        continues_at("- milk", 2, "- ");
        assert_eq!(continuation("- [ ] task", 3), None);
        continues_at("- [ ] task", 6, "- [ ] ");
    }

    #[track_caller]
    fn continues_at(line: &str, caret: usize, expect: &str) {
        assert_eq!(
            continuation(line, caret),
            Some(Continuation::Next(expect.to_string())),
            "continuing {line:?} at {caret}"
        );
    }

    /// Splitting an item mid-text carries the marker; the tail moves down with
    /// it, which is what every editor does.
    #[test]
    fn splitting_an_item_carries_the_marker() {
        continues_at("- milk and eggs", 7, "- ");
        continues_at("1. first and second", 9, "2. ");
    }
}
