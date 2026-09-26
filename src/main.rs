//! GravityNote GPUI — one fast markdown note window.
//!
//! Stack: Rust + GPUI (Zed's Apache-2.0 GPU UI framework via crates.io).
//! No vendored or copied GPL Zed editor sources — original app code only.
//!
//! The document is a single markdown file. Notes within it are separated by a
//! thematic break (`---`); each separator row draws a hairline with a small `^`
//! button on the right that promotes the note beneath it to the top.
//!
//! # Staying fast on a huge note
//!
//! The target is twenty years of daily note-taking — roughly 10 MB and 200,000
//! lines. Three rules keep that interactive:
//!
//! 1. **Nothing in the frame path is O(document).** Rows are virtualized with
//!    [`list`], so only the visible lines are shaped, highlighted, and laid out.
//!    Markdown highlighting runs per visible line, never over the whole buffer.
//!    Rows have variable height because lines soft-wrap, so the list is the
//!    sum-tree [`ListState`] kind rather than the uniform-height kind.
//! 2. **Offset math goes through [`LineIndex`]**, which answers line lookups and
//!    UTF-16 conversions in O(log n). The macOS IME asks for UTF-16 offsets on
//!    every keystroke; walking the buffer for those was the worst hot spot.
//! 3. **Derived state is cached and invalidated incrementally.** The fenced-code
//!    state per line is recomputed forward from the edited line and stops as
//!    soon as it reconverges, so an ordinary keystroke touches one line.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::Local;
use gpui::{
    actions, anchored, canvas, deferred, div, fill, img, list, point, prelude::*, px, relative,
    rgb,
    size, AnyElement, App,
    Application,
    AsyncApp, Bounds, ClickEvent, ClipboardItem, Context, DispatchPhase, ElementId,
    ElementInputHandler, Entity,
    EntityInputHandler, ExternalPaths, Div, FocusHandle, Focusable, Font, GlobalElementId, Hsla,
    ImageSource, KeyBinding, LayoutId,
    ListAlignment, ListOffset, ListState, Menu, MenuItem, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, OsAction,
    Pixels, Point, ScrollWheelEvent, SharedString, StrikethroughStyle, Style, StyledText, svg, TextLayout, TextRun,
    TitlebarOptions, UTF16Selection, UnderlineStyle, Window, WindowBackgroundAppearance,
    WindowBounds, WindowGlassAppearance, WindowOptions,
};

use gravitynote::caret;
use std::path::{Path, PathBuf};

use gravitynote::dev;
use gravitynote::image_bank;
use gravitynote::images;
use gravitynote::index::{self, LineIndex};
use gravitynote::lists;
use gravitynote::markdown::{self, MdStyle};
use gravitynote::note::{
    self, default_note_path, load_note, LoadOutcome, Note,
};
use gravitynote::fences::FenceMap;
use gravitynote::find::Find;
use gravitynote::history::{self, Edit, History};
use gravitynote::persist::{self, Persist};
use gravitynote::rows;
use gravitynote::selection;
use gravitynote::settings::{self, Hotkey, Settings};
use gravitynote::login_item;
use gravitynote::platform::{self, PlatformEvent, PlatformHandle};
use gravitynote::theme::{self, attrs_for, theme};

/// Embedded Lilex (OFL) so the note always renders with the intended mono face.
/// All faces are bundled — markdown emphasis needs real bold and italic.
const LILEX_REGULAR: &[u8] = include_bytes!("../assets/fonts/Lilex-Regular.ttf");
const LILEX_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Lilex-SemiBold.ttf");
const LILEX_BOLD: &[u8] = include_bytes!("../assets/fonts/Lilex-Bold.ttf");
const LILEX_ITALIC: &[u8] = include_bytes!("../assets/fonts/Lilex-Italic.ttf");
const LILEX_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/Lilex-BoldItalic.ttf");

/// The handful of vector glyphs the chrome draws — the find field's magnifier
/// and its prev/next chevrons. GPUI tints an SVG by its coverage, so the fill in
/// the file is irrelevant; `.text_color` decides the colour on screen.
///
/// They are embedded rather than read from disk so the app is a single binary
/// with nothing to find at runtime, exactly as the fonts already are.
struct Icons;

impl gpui::AssetSource for Icons {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "icons/search.svg" => Some(Cow::Borrowed(
                include_bytes!("../assets/icons/search.svg").as_slice(),
            )),
            "icons/chevron-up.svg" => Some(Cow::Borrowed(
                include_bytes!("../assets/icons/chevron-up.svg").as_slice(),
            )),
            "icons/chevron-down.svg" => Some(Cow::Borrowed(
                include_bytes!("../assets/icons/chevron-down.svg").as_slice(),
            )),
            _ => None,
        })
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(vec![
            "icons/search.svg".into(),
            "icons/chevron-up.svg".into(),
            "icons/chevron-down.svg".into(),
        ])
    }
}

/// Autosave settles this long after the last keystroke.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// A live find search settles this long after the last query keystroke. A
/// document-wide scan is cheap once but must not run per character on a
/// twenty-year note; the field updates now and the matches follow.
const FIND_DEBOUNCE: Duration = Duration::from_millis(120);
/// How often the app polls the platform channel and services autosave/backups.
/// The menu-bar item and the global show/hide chord arrive on that channel, so
/// this is also the worst case for ⌃A waking the window.
const TICK: Duration = Duration::from_millis(200);

/// How often the note file is stat'd to notice an edit made by something else.
/// Five times a second is a syscall rate set by the tick loop rather than by
/// anything the answer is used for; a sync service landing a file is not less
/// noticed for being seen a second later.
const DISK_CHECK: Duration = Duration::from_secs(1);
/// How far beyond the viewport the list keeps rows measured, so scrolling does
/// not stutter while heights are discovered.
const LIST_OVERDRAW: Pixels = px(800.);
/// Width of the caret.
const CARET_WIDTH: Pixels = px(2.);
/// How long an unfocused window sleeps between caret checks.
///
/// It draws no caret while it is not the front window, and coming back is
/// reported by `observe_window_activation`, which repaints straight away — so
/// this poll is a backstop, not the mechanism. It used to run four times a
/// second forever, which is a wakeup every 250 ms for the whole time an app
/// that is designed to sit hidden all day sits hidden all day.
///
/// It is not longer than a second because the timer is also what *restarts* the
/// rhythm: the activation observer paints a solid caret immediately, but the
/// fade does not resume until this fires, and a caret that sits perfectly still
/// for ten seconds after you come back reads as a frozen window.
const CARET_IDLE_POLL: Duration = Duration::from_secs(1);
/// A drag-selection that has left the viewport scrolls on its own clock, so it
/// keeps going while the pointer is held still outside the window.
const DRAG_SCROLL_TICK: Duration = Duration::from_millis(16);
/// Pixels scrolled per tick, per *squared* pixel the pointer is past the edge.
/// The curve is quadratic and unclamped: nudging just past the edge stays a
/// controllable line-by-line crawl, but flinging the pointer far below the
/// window accelerates hard, so the far end of a 214,000-line note is reachable
/// in one gesture rather than a minute of waiting for a capped speed.
const DRAG_SCROLL_GAIN: f32 = 0.01;
/// The slowest a drag scrolls: about a line a second, a readable crawl.
const DRAG_SCROLL_FLOOR: Pixels = px(0.4);
// ── The frame ───────────────────────────────────────────────────────────────
//
// The page is measured in the text's own size, not in pixels. Everything
// *inside* the note already scales — heading sizes, leading, the space above a
// heading — and a frame fixed in pixels around it means ⌘+ resizes the text
// inside a chrome that stays put, rather than zooming one designed page.
//
// A reading measure is the clearest case: it exists to stop lines being hard to
// track back from, and that is counted in characters. Fixed at 720 px it gave
// 100 characters at the smallest text size — where lines are hardest to track —
// and 37 at the largest, where someone has asked for fewer, larger words.
//
// The multipliers reproduce the old fixed pixel values at a 15pt text size.

/// The size of anything that is chrome rather than note: the find bar, the
/// notice. It tracks the text without matching it — at 30pt note text a fixed
/// 13pt bar reads as a different application, and at 11pt an unclamped 0.75×
/// notice is 8pt, which is not readable at all.
fn chrome_text(text_size: f32) -> f32 {
    (text_size * 0.85).clamp(11., 20.)
}

/// Characters per line at any text size. Lilex advances exactly 0.6 em.
const MEASURE_CHARS: f32 = 74.;


// The multipliers below are quarter-rows: a row is `text_size * LINE_HEIGHT`,
// so 2.0 is 1¼ rows, 1.2 is ¾, 0.8 is ½. They started life as the old pixel
// constants divided by 15, which made them proportional without making them
// rational — every one landed on a fraction of a row no other number used.

/// Space either side of the text column. 1¼ rows.
fn side_inset(text_size: f32) -> Pixels {
    px(text_size * 2.0)
}

/// How far a row's background reaches past the text: ½ a row.
fn row_bleed(text_size: f32) -> Pixels {
    px(text_size * 0.8)
}

/// The column's own padding: the side inset less what each row bleeds back.
/// ¾ of a row.
fn column_inset(text_size: f32) -> Pixels {
    px(text_size * 1.2)
}

/// The strip the traffic lights float over: what they physically occupy, which
/// is what the hover target has to cover however small the text is.
///
/// 2½ rows, floored at the buttons' own height. There is deliberately no
/// ceiling: one pinned the inset for eight of the twenty text sizes, which is
/// the page failing to zoom — the thing this was rewritten to stop.
fn top_inset(text_size: f32) -> Pixels {
    px((text_size * 4.0).max(44.))
}

/// Space above the first line. There is no title bar and no header — the window
/// is a sheet of paper, so this is breathing room at rest, not a band: it rides
/// on the first row and scrolls away with it, exactly the way the last line's
/// room at the bottom does.
///
/// Half the traffic-light strip, but never less than the bar those lights sit
/// on — *while that bar is full width*. The bar is opaque, and a first line
/// starting above its lower edge would be sliced in half the moment the pointer
/// went up there. Text passing under the bar once the note is *scrolled* is fine
/// — that is the same thing that already happens at the bottom edge — but the
/// line the note opens on should not be.
///
/// In a window too short to give 42 points away (the app shrinks to 120 points
/// tall, where that is a third of it) the reserve shrinks and [`lights_are_wide`]
/// stops drawing the bar across the note. The lights then sit on a patch as wide
/// as they are, which is the same arrangement the find bar uses.
fn top_pad(text_size: f32, window_height: Pixels) -> Pixels {
    // ...but never more than a seventh of the window. At the 180x120 the app
    // shrinks to, a fixed 42-point reserve is a third of the height given over
    // to somewhere for three buttons to land — and at that size the note is the
    // only reason the window is open.
    let ceiling = (f32::from(window_height) / 7.).max(text_size);
    px((text_size * 2.0).max(TITLEBAR_H).min(ceiling))
}

/// Whether the strip the traffic lights sit on may be painted across the whole
/// window. Only when the first line clears it: otherwise the bar would cover the
/// text the moment the pointer went looking for the buttons — and block clicks
/// into it, since the bar takes the mouse as well as the pixels.
fn lights_are_wide(text_size: f32, window_height: Pixels) -> bool {
    f32::from(top_pad(text_size, window_height)) >= TITLEBAR_H
}

/// The bar the macOS window buttons sit on, in points. Fixed by the system, not
/// by the text size: the buttons do not scale with the note. GPUI pins their
/// top-left at (14, 14) and they are 14pt tall, so this leaves the same 14
/// points of air underneath them as above.
const TITLEBAR_H: f32 = 42.;

/// How much room the three window buttons need at the left of the strip they
/// sit on: their cluster plus the air around it. The find bar keeps this clear,
/// so the lights have somewhere to appear while a search is open — without it
/// the bar covered them, and with a search open there was no way to close,
/// minimise or zoom the window by mouse at all.
const TRAFFIC_LIGHT_GUTTER: f32 = 78.;

/// Space between the find bar and the first line of the note. ¾ of a row.
fn find_bar_gap(text_size: f32) -> Pixels {
    px(text_size * 1.2)
}

/// Comfortable reading measure. Beyond this the text stops growing with the
/// window and centres instead.
fn reading_width(text_size: f32) -> Pixels {
    px(text_size * 0.6 * MEASURE_CHARS) + side_inset(text_size) * 2.
}
/// How many marks the match rail can carry. About one per pixel of a tall
/// window, which is as fine as the eye or the rail can resolve.
const MATCH_TICK_BUCKETS: usize = 900;

/// Leading, as a multiple of the text size. The row height everything else is
/// measured against.
const LINE_HEIGHT: f32 = 1.6;

/// Heading size as a multiple of the body size, and the space above it in whole
/// and half *rows*. Keeping both relative means the hierarchy moves together
/// when the reader changes the text size; counting the space in rows rather
/// than in body sizes means a heading lands back on the same rhythm as the text
/// around it instead of a fraction off it.
fn heading_scale(level: u8) -> (f32, f32) {
    match level {
        1 => (1.45, 1.0),
        2 => (1.24, 0.75),
        3 => (1.10, 0.5),
        // Levels 4–6 used to all render at body size, distinguished only by
        // weight — indistinguishable from bold text. Give them a gentle,
        // decreasing lift so the hierarchy stays legible all the way down.
        4 => (1.05, 0.5),
        5 => (1.0, 0.25),
        _ => (0.92, 0.25),
    }
}

// ── Actions (keyboard dispatch) ──────────────────────────────────────────────

actions!(
    gravitynote,
    [
        Quit,
        NewNote,
        BringNoteUp,
        MoveNoteUp,
        BackupNow,
        Indent,
        Outdent,
        ToggleTask,
        ToggleDone,
        ToggleWindow,
        ToggleOpenAtLogin,
        OpenSettings,
        Minimize,
        ZoomWindow,
        ToggleFullScreen,
        HideApp,
        HideOthers,
        ShowAllApps,
        AboutApp,
        RevealInFinder,
        ExportNote,
        OpenBackups,
        ReclaimImages,
        ChooseNoteFolder,
        OpenPalette,
        PaletteStep,
        PaletteStepBack,
        PaletteRun,
        ShowHelp,
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        PageUp,
        PageDown,
        ParagraphUp,
        ParagraphDown,
        SelectParagraphUp,
        SelectParagraphDown,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectHome,
        SelectEnd,
        SelectAll,
        Home,
        End,
        DocumentStart,
        DocumentEnd,
        SelectToDocumentStart,
        SelectToDocumentEnd,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordBack,
        DeleteWordForward,
        DeleteToLineStart,
        KillToEnd,
        Yank,
        OpenLine,
        CenterCaret,
        Transpose,
        MoveLineUp,
        MoveLineDown,
        DuplicateLines,
        Undo,
        Redo,
        TextBigger,
        TextSmaller,
        TextSizeReset,
        OpenFind,
        CloseFind,
        FindNext,
        FindPrevious,
        // The query field's own caret motion, live only while the find bar owns
        // the keyboard (key_context "Find"). They move the caret in the query,
        // never the note; the note's identical keys sit in the "Note" context
        // and stop dispatching while find is open.
        FindCharLeft,
        FindCharRight,
        FindWordLeft,
        FindWordRight,
        FindLineStart,
        FindLineEnd,
        FindSelectCharLeft,
        FindSelectCharRight,
        FindSelectWordLeft,
        FindSelectWordRight,
        FindSelectLineStart,
        FindSelectLineEnd,
        FindDeleteForward,
        FindDeleteWordBack,
        FindDeleteWordForward,
        FindDeleteToStart,
        FindFocusNextField,
        ToggleReplace,
        ReplaceAndFind,
        ReplaceAll,
        UseSelectionForFind,
        JumpToSelection,
        Enter,
        Paste,
        Cut,
        Copy,
    ]
);

// ── Root view ────────────────────────────────────────────────────────────────

/// A row's text and its markdown spans, as of one revision of the buffer.
///
/// Cached per visible line: the caret's fade repaints the whole window while
/// nothing changes, and re-copying and re-highlighting every line on each of
/// those frames is work with a known answer. The revision and the byte range
/// together are the proof that the answer still holds — same buffer, same
/// slice, same text.
#[derive(Clone)]
struct RowText {
    revision: u64,
    range: (usize, usize),
    /// Highlighting depends on whether the line sits inside a fence, which can
    /// change without the line itself changing.
    in_fence: bool,
    /// Shared rather than owned: a hit hands the same allocation back rather
    /// than copying a row's spans on every frame of a caret fade.
    spans: std::rc::Rc<[markdown::Span]>,
    display: SharedString,
}

/// How many rows the cache holds before it is emptied. A screenful is a few
/// dozen; this is a scroll's worth, and clearing is cheaper than tracking.
const ROW_CACHE_CAP: usize = 512;

/// One mark on the scroll rail: where in the document it sits (a fraction of
/// the note's length), which match it jumps to when clicked, and whether that
/// bucket holds the current hit — so the current match reads on the rail the
/// way it does in the body.
#[derive(Clone, Copy)]
struct RailTick {
    at: f32,
    match_index: usize,
    current: bool,
}

/// A one-line hover tooltip. The crates.io GPUI has no `ui::Tooltip`, so the app
/// draws its own small bubble — a pale card with a hairline border, the way a
/// macOS help tag looks. Used to name the find bar's terse `Aa` / `W` toggles.
struct TextTooltip(SharedString);

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Deferred so it paints above the field, and anchored by the caller.
        div()
            .bg(rgb(theme().bg))
            .border_1()
            .border_color(rgb(theme().find_field_border))
            .rounded_md()
            .shadow_sm()
            .px_2()
            .py_1()
            .text_size(px(12.))
            .text_color(rgb(theme().fg))
            .child(self.0.clone())
    }
}

/// The wash that dims the note behind a panel. Ink at 15% over paper works in
/// the light; over a near-black page it is invisible, so the dark side dims with
/// black instead of with its own foreground.
fn scrim() -> Hsla {
    if theme::is_dark() {
        gpui::rgba(0x00000066).into()
    } else {
        gpui::rgba(0x1c1f2426).into()
    }
}

/// The command palette's state: what has been typed, and which row it points at.
#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
}

/// One command the palette can run.
struct Command {
    /// What it is called, which is also what is searched.
    name: &'static str,
    /// The keystroke it also answers to, for the right-hand column.
    key: &'static str,
    run: fn(&mut NoteApp, &mut Window, &mut Context<NoteApp>),
}

/// Every command worth reaching for by name. The menus stay the discoverable
/// route; this is the fast one.
static COMMANDS: &[Command] = &[
    Command { name: "New Note", key: "⌘N", run: |a, w, cx| a.new_note(&NewNote, w, cx) },
    Command { name: "Bring Note to Top", key: "⌘⌃⇧↑", run: |a, w, cx| a.bring_note_up(&BringNoteUp, w, cx) },
    Command { name: "Move Note Up", key: "⌘⌃↑", run: |a, w, cx| a.move_note_up(&MoveNoteUp, w, cx) },
    Command { name: "Find", key: "⌘F", run: |a, w, cx| a.open_find(&OpenFind, w, cx) },
    Command { name: "Find and Replace", key: "⌥⌘F", run: |a, w, cx| a.toggle_replace(&ToggleReplace, w, cx) },
    Command { name: "Indent", key: "⇥", run: |a, w, cx| a.indent(&Indent, w, cx) },
    Command { name: "Outdent", key: "⇧⇥", run: |a, w, cx| a.outdent(&Outdent, w, cx) },
    Command { name: "Make a Task", key: "⌘⇧T", run: |a, w, cx| a.toggle_task(&ToggleTask, w, cx) },
    Command { name: "Complete Task", key: "⌘⏎", run: |a, w, cx| a.toggle_done(&ToggleDone, w, cx) },
    Command { name: "Move Line Up", key: "⌥⌘↑", run: |a, w, cx| a.move_line_up(&MoveLineUp, w, cx) },
    Command { name: "Move Line Down", key: "⌥⌘↓", run: |a, w, cx| a.move_line_down(&MoveLineDown, w, cx) },
    Command { name: "Duplicate Lines", key: "⌘D", run: |a, w, cx| a.duplicate_lines(&DuplicateLines, w, cx) },
    Command { name: "Undo", key: "⌘Z", run: |a, w, cx| a.undo(&Undo, w, cx) },
    Command { name: "Redo", key: "⌘⇧Z", run: |a, w, cx| a.redo(&Redo, w, cx) },
    Command { name: "Bigger Text", key: "⌘+", run: |a, w, cx| a.text_bigger(&TextBigger, w, cx) },
    Command { name: "Smaller Text", key: "⌘−", run: |a, w, cx| a.text_smaller(&TextSmaller, w, cx) },
    Command { name: "Default Text Size", key: "⌘0", run: |a, w, cx| a.text_size_reset(&TextSizeReset, w, cx) },
    Command { name: "Settings", key: "⌘,", run: |a, w, cx| a.open_settings(&OpenSettings, w, cx) },
    Command { name: "Back Up Now", key: "⌘S", run: |a, w, cx| a.backup_now(&BackupNow, w, cx) },
    Command { name: "Restore from Backup", key: "", run: |a, w, cx| a.open_backups(&OpenBackups, w, cx) },
    Command { name: "Reclaim Unused Images", key: "", run: |a, w, cx| a.reclaim_images(&ReclaimImages, w, cx) },
    Command { name: "Change Note Folder", key: "", run: |a, w, cx| a.choose_note_folder(&ChooseNoteFolder, w, cx) },
    Command { name: "Export", key: "", run: |a, w, cx| a.export_note(&ExportNote, w, cx) },
    Command { name: "Reveal in Finder", key: "", run: |a, w, cx| a.reveal_in_finder(&RevealInFinder, w, cx) },
    Command { name: "Open at Login", key: "", run: |a, w, cx| a.toggle_open_at_login(&ToggleOpenAtLogin, w, cx) },
    Command { name: "Minimize", key: "⌘M", run: |a, w, cx| a.minimize(&Minimize, w, cx) },
    Command { name: "Zoom", key: "", run: |a, w, cx| a.zoom_window(&ZoomWindow, w, cx) },
    Command { name: "Enter Full Screen", key: "⌃⌘F", run: |a, w, cx| a.toggle_full_screen(&ToggleFullScreen, w, cx) },
    Command { name: "Hide GravityNote", key: "⌘H", run: |a, w, cx| a.hide_app(&HideApp, w, cx) },
    Command { name: "About GravityNote", key: "", run: |a, w, cx| a.about_app(&AboutApp, w, cx) },
    Command { name: "Quit GravityNote", key: "⌘Q", run: |a, w, cx| a.quit(&Quit, w, cx) },
];

/// Whether every character of `query` appears in `text`, in order — the loose
/// match a palette wants, so "nn" finds "New Note".
fn subsequence(text: &str, query: &str) -> bool {
    let mut haystack = text.chars();
    query
        .chars()
        .all(|needle| haystack.any(|c| c == needle))
}

/// Which of the find bar's four chips was clicked.
#[derive(Clone, Copy)]
enum Toggle {
    MatchCase,
    WholeWord,
    Regex,
    NoteOnly,
}

/// A caret motion within the query field. `forward` is rightward/toward the
/// end; the enum keeps the twelve query-caret actions delegating to one method.
#[derive(Clone, Copy)]
enum QueryMotion {
    Char(bool),
    Word(bool),
    Edge(bool),
}

/// The unit a drag-selection extends by. Captured at mouse-down from the click
/// count and held for the length of the drag, so a double-click-drag keeps whole
/// words and a triple-click-drag whole paragraphs.
#[derive(Clone, Copy, PartialEq)]
enum DragGranularity {
    Char,
    Word,
    Paragraph,
}

/// An image being resized by its grip.
///
/// The note is left alone until the drag ends. Rewriting `|420x263` on every
/// mouse-move would put a hundred edits in the undo history for one gesture,
/// and each one would move the caret onto the line — which is the line's cue to
/// show its markdown instead of the picture, so the image would vanish from
/// under the pointer that was resizing it.
#[derive(Clone)]
struct ImageDrag {
    /// The line being resized, as it was at mouse-down.
    line: usize,
    /// Which image that line held. A line index is not a stable name for
    /// anything: the tick timer can adopt a note rewritten on disk mid-drag,
    /// and ⌘Z is dispatched while a mouse button is down like any other key.
    /// Either can shift every index below it, and committing then would resize
    /// a *different* picture and record an edit nobody made. Checked on commit.
    name: String,
    /// The displayed width when the grip was grabbed, in points.
    start_w: f32,
    /// Where the pointer was then, so the drag tracks it without jumping.
    start_x: Pixels,
    /// Width over height, kept so the box stays in proportion.
    aspect: f32,
    /// The width the drag has reached, in points. What the row draws.
    width: f32,
}

/// Where an import's bytes are coming from: the pasteboard, or files dropped on
/// the window. Both end in the same place — the store — so they share a path.
enum ImportSource {
    Bytes(Vec<u8>),
    Files(Vec<PathBuf>),
}

/// The first image on the pasteboard, if there is one.
///
/// macOS offers plain text in preference to an image when both are present, so
/// this finds nothing for anything copied out of a browser. A screenshot, which
/// is the common case, arrives as an image entry.
fn clipboard_image(item: ClipboardItem) -> Option<Vec<u8>> {
    item.into_entries().find_map(|entry| match entry {
        gpui::ClipboardEntry::Image(image) => Some(image.bytes),
        _ => None,
    })
}

/// The scroll thumb, expressed both as track fractions (what `render` draws) and
/// as the line-space mapping the thumb drag and track-click paging need. Rows
/// are variable height and only the visible ones are measured, so this is a
/// line-count estimate, not a pixel one — see [`NoteApp::scroll_metrics`].
struct ScrollMetrics {
    /// The thumb's top as a fraction of the rail, in `0.0..=1.0 - height`.
    top: f32,
    /// The thumb's height as a fraction of the rail.
    height: f32,
    /// The first-visible line index when the last line sits at the bottom — the
    /// denominator that maps a track fraction onto a scroll position.
    max_top_line: f32,
}

/// Maps the Settings matrix to the reference's native full-window material.
/// The NSWindow owns the outside silhouette, so the inner glass radius remains
/// zero exactly as prescribed by `gpui-liquid-glass`.
const fn window_background_for_glass(
    style: settings::GlassStyle,
) -> WindowBackgroundAppearance {
    let coral = gpui::Rgba {
        r: 1.0,
        g: 0.22,
        b: 0.28,
        a: 1.0,
    };
    match style {
        settings::GlassStyle::Regular => WindowBackgroundAppearance::LiquidGlass(
            WindowGlassAppearance::regular().corner_radius(px(0.0)),
        ),
        settings::GlassStyle::Clear => WindowBackgroundAppearance::LiquidGlass(
            WindowGlassAppearance::clear().corner_radius(px(0.0)),
        ),
        settings::GlassStyle::RegularTinted => WindowBackgroundAppearance::LiquidGlass(
            WindowGlassAppearance::regular()
                .tint(coral)
                .corner_radius(px(0.0)),
        ),
        settings::GlassStyle::ClearTinted => WindowBackgroundAppearance::LiquidGlass(
            WindowGlassAppearance::clear()
                .tint(coral)
                .corner_radius(px(0.0)),
        ),
        settings::GlassStyle::Identity => WindowBackgroundAppearance::Transparent,
    }
}

/// Code is an ordinary local contrast surface above the window material. Keep
/// the original opaque palette in Identity mode; over glass, preserve enough of
/// the compositor image to avoid cutting a flat paper rectangle out of it.
fn code_surface(hex: u32, glass: bool, alpha: f32) -> Hsla {
    let mut colour = rgb(hex);
    if glass {
        colour.a = alpha;
    }
    colour.into()
}

struct NoteApp {
    note: Note,
    /// O(log n) line and UTF-16 lookups. Rebuilt on every text change.
    index: LineIndex,
    /// Which lines sit inside a ``` block. Patched per edit rather than
    /// recomputed; see [`FenceMap`].
    fences: FenceMap,
    /// Undo/redo over edit spans, with consecutive typing coalesced.
    history: History,
    /// The caret's current opacity, and when it last moved or typed. It holds
    /// solid for a beat after activity, the way every Mac editor does, then
    /// pulses. See [`caret_alpha`].
    caret_alpha: f32,
    caret_since: Instant,
    /// Persisted app preferences.
    settings: Settings,
    /// Open find session. `None` when the find bar is closed.
    find: Option<Find>,
    /// Whether the find bar, when open, holds the keyboard. A body click hands
    /// the keyboard back to the note while leaving the bar and its highlights on
    /// screen (macOS's non-modal find): the bar is still `Some`, but this is
    /// `false`, key input goes to the note, and the query caret is hidden. Only
    /// ever `true` while `find.is_some()`. Clicking the bar or ⌘F re-focuses it.
    find_focused: bool,

    /// Whether the in-app Settings panel is showing.
    settings_open: bool,
    /// Whether the Backups panel is showing, and what it listed when it opened.
    backups_open: bool,
    backups: Vec<persist::BackupInfo>,
    /// Whether the panel is waiting to capture the next key press as the new
    /// global show/hide chord.
    recording_shortcut: bool,

    focus_handle: FocusHandle,
    /// Variable-height virtualized list — one item per logical line. Wrapped
    /// lines are taller, so heights are measured and cached by the list itself.
    list_state: ListState,

    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// The last text ⌃K killed, for ⌃Y to yank back — a one-slot kill ring, the
    /// minimum that makes the emacs kill/yank pair whole.
    kill_ring: String,
    is_selecting: bool,

    store: Persist,
    /// When false, refuse autosave so a failed load never overwrites the file.
    allow_autosave: bool,
    /// Whether `note.md` exists on disk yet. Until it does there is nothing
    /// worth backing up — a backup of a file that was never written is noise.
    note_on_disk: bool,
    /// Set on every edit; the tick loop flushes once it has settled.
    dirty_since: Option<Instant>,
    /// Whether a background write is in flight. One at a time: the next tick
    /// starts the next one, so two saves can never race to rename over the
    /// same file. See [`NoteApp::save_soon`].
    saving: bool,
    /// The disk fingerprint of an external change we tried and failed to back
    /// up. While it matches the file's current state, `reconcile_disk` keeps
    /// blocking the flush without re-reading the whole file or re-alerting every
    /// tick — the backoff for finding number 2. Cleared once the disk state
    /// moves on or the conflict resolves.
    reconcile_backoff: Option<persist::FileStamp>,
    /// When the note file was last stat'd for an external change. See
    /// [`DISK_CHECK`].
    disk_checked: Option<Instant>,
    /// Last persistence error message (shown in the header when set).
    /// The one line of chrome the window ever shows. See [`Notice`].
    notice: Option<Notice>,

    /// Text layouts for the *visible* rows only, keyed by line index. Recorded
    /// during prepaint (so they are always measured) and used for hit-testing,
    /// caret placement, and telling the IME where the caret is on screen.
    line_layouts: HashMap<usize, rows::Row<TextLayout>>,

    /// What each visible row's text and markdown spans were last frame, so an
    /// idle window is not re-copying and re-highlighting every line it shows.
    /// See [`RowText`]: the buffer's revision plus the line's byte range is what
    /// makes a hit provably the same text.
    row_cache: HashMap<usize, RowText>,
    /// Bumped whenever the buffer changes. The only thing [`Self::row_cache`]
    /// needs to know about an edit.
    revision: u64,

    /// Which side of a soft wrap the caret is on, and the column it is aiming
    /// for while stepping rows. Both are set only by row-wise motion and
    /// cleared by every other caret move. See [`rows::Caret`].
    caret: rows::Caret,

    /// Where the text sits on screen, recorded during prepaint. A drag needs it
    /// to tell whether the pointer has left the viewport.
    viewport: Option<Bounds<Pixels>>,

    /// Decoded images, downscaled and bounded. See [`image_bank`].
    bank: image_bank::Bank,
    /// The command palette, when it is open. See [`NoteApp::open_palette`].
    palette: Option<Palette>,
    /// When the "Reclaim Unused Images" command last said what it found. The
    /// press that deletes has to be a second, deliberate one.
    reclaim_offered: Option<Instant>,
    /// The directory the image store sits in — the note's own directory,
    /// resolved once so a row never has to ask the filesystem where it is.
    images_dir: PathBuf,
    /// The image being resized, while the grip is held. The note is not
    /// rewritten until the drag ends: a resize is one edit, not one per frame.
    image_drag: Option<ImageDrag>,
    /// The pointer, while a drag-selection is running. Held so the scroll can
    /// keep going when the pointer stops moving but stays outside the viewport.
    drag_position: Option<Point<Pixels>>,
    /// Whether the drag-scroll loop is already running.
    drag_scrolling: bool,
    /// The unit a drag-selection extends by, captured at mouse-down. A single
    /// click extends by character; a double- or triple-click keeps whole words
    /// or paragraphs while you drag, the way macOS does.
    drag_granularity: DragGranularity,
    /// The word or paragraph the multi-click drag began on. A granular drag
    /// keeps this selected and swallows whole units toward the pointer.
    drag_anchor: Range<usize>,

    /// The scroll rail's measured pixel bounds, recorded during prepaint. The
    /// thumb drag and track-click paging need it to map a pointer onto the
    /// document. `None` until the rail has been painted at least once.
    scroll_rail: Option<Bounds<Pixels>>,
    /// The grab offset while dragging the thumb: the pixels between the pointer
    /// and the thumb's top when the drag began, so the thumb tracks the pointer
    /// without jumping. `Some` only while a thumb drag is in progress.
    scroll_drag: Option<Pixels>,
    /// How many rows were inside the viewport last frame. Set in `render` and
    /// reused by the scrollbar math so the thumb it draws and the thumb a drag
    /// moves agree.
    visible_lines: usize,

    /// Where a right-click opened the context menu, in window coordinates.
    /// `None` when the menu is closed. See [`NoteApp::render_context_menu`].
    context_menu: Option<Point<Pixels>>,

    /// Whether the traffic lights are showing. They follow the pointer into the
    /// top strip and leave with it.
    window_buttons_visible: bool,

    /// Where the matches sit in the document, bucketed to the rail's own
    /// resolution, for the marks on the scroll rail. Derived when the query or
    /// the text changes — never in the frame path, where three letters into a
    /// search of a twenty-year note means nine hundred thousand hits.
    match_rail: Vec<RailTick>,
    /// Scratch for [`NoteApp::update_match_rail`]: which match each bucket of
    /// the rail holds. Kept between calls so a keystroke does not allocate it.
    rail_buckets: Vec<usize>,
    /// The query the find bar closed with, so reopening resumes it.
    last_query: String,
    /// The selection when the find bar was opened, restored if it is closed with
    /// Escape — the way Safari puts you back where you were rather than stranding
    /// you on the last match. `(start, end, reversed)`. Cleared once find is
    /// dismissed any other way (a body click, which places its own caret).
    find_return: Option<(usize, usize, bool)>,
    /// Bumped on every query edit. A debounced full-document scan captures the
    /// value it was scheduled at and only runs if it still matches, so a fast
    /// typist collapses a burst of keystrokes into one scan instead of stacking
    /// a search of a twenty-year note on the main thread per character.
    find_search_gen: u64,
    /// Whether a debounced search is still owed. ⏎/⌘G pressed before it settles
    /// should land on the nearest match rather than step past the one the user
    /// has not been shown yet, so stepping flushes the pending scan first.
    find_search_pending: bool,
    /// When the note was last edited under an open find bar. The matches were
    /// shifted along with the text at the time; the tick loop searches the
    /// document again once the typing settles. See [`NoteApp::after_edit`].
    find_dirty_since: Option<Instant>,
    /// Whether the search is limited to the note the caret is in. The range
    /// itself is re-derived per search; see [`NoteApp::sync_find_scope`].
    find_note_only: bool,

    /// Whether the window had focus last time the caret was advanced.
    window_was_active: bool,

    /// The menu-bar item and global hotkey. Shared so the tick loop keeps it
    /// alive while this view mutates it to re-register the show/hide chord.
    /// `None` until [`NoteApp::set_platform`] hands it over after install, and
    /// when the platform layer could not start at all.
    platform: Option<Rc<RefCell<PlatformHandle>>>,
}

/// A line of feedback at the foot of the window — the only chrome that ever
/// appears there.
///
/// Two kinds, because they answer different questions. An alert reports
/// something the user has to know about their note and stays until it is
/// resolved; a remark confirms something they just asked for and gets out of
/// the way on its own.
struct Notice {
    text: String,
    alert: bool,
    /// Whether the condition behind it is still true and re-checked elsewhere.
    /// A sticky alert cannot be waved away: "autosave is paused" is not news,
    /// it is the state the app is in, and hiding it would leave someone typing
    /// into a note that is not being written with nothing on screen to say so.
    sticky: bool,
    since: Instant,
}

impl Notice {
    /// Something the user has to know about their note. Stays put.
    fn alert(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            alert: true,
            sticky: false,
            since: Instant::now(),
        }
    }

    /// An alert that stands for a condition the app keeps checking, so there is
    /// nothing for a ✕ to mean.
    fn sticky(text: impl Into<String>) -> Self {
        Self {
            sticky: true,
            ..Self::alert(text)
        }
    }

    /// Confirmation of something the user just asked for. Fades.
    fn remark(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            alert: false,
            sticky: false,
            since: Instant::now(),
        }
    }

    fn expired(&self) -> bool {
        !self.alert && self.since.elapsed() >= NOTICE_LINGER
    }
}

/// How long the offer to delete unused images stands before it has to be made
/// again. Long enough to read what it says, short enough that a press minutes
/// later is a fresh question.
const RECLAIM_CONFIRM: Duration = Duration::from_secs(20);

/// How long a remark stays before it fades. Long enough to read, short enough
/// not to become furniture.
const NOTICE_LINGER: Duration = Duration::from_secs(4);



const WELCOME: &str = "# My notes\n\nStart typing here. Everything saves automatically.";

impl NoteApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut settings = Settings::load(&settings::settings_path());
        if let Some(folder) = settings.note_folder.as_mut() {
            *folder = gravitynote::sandbox::restore(folder);
        }
        // The note lives where the setting says, even when that folder is not
        // mounted yet — the app says so rather than silently opening the old
        // copy in Application Support and writing to it.
        let missing_folder = settings
            .note_folder
            .as_ref()
            .filter(|folder| !folder.is_dir())
            .cloned();
        let path = match &settings.note_folder {
            Some(folder) => folder.join("note.md"),
            None => default_note_path(),
        };
        let images_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut notice: Option<Notice> = missing_folder.as_ref().map(|folder| {
            Notice::sticky(format!(
                "The note folder {} is not there — nothing will be saved until it is back. \
                 Change Note Folder… picks another.",
                folder.display()
            ))
        });
        let (note, allow_autosave, note_on_disk) = match load_note(&path) {
            // An unmounted volume is *missing*, not empty. Saving would create
            // the mount point as an ordinary folder on the boot disk and write
            // the welcome note into it — and the real volume would then arrive
            // beside it under another name, with the note apparently gone.
            LoadOutcome::Missing if missing_folder.is_some() => (Note::new(), false, false),
            LoadOutcome::Missing => (Note::from_text(WELCOME), true, false),
            LoadOutcome::Loaded(n) => (n, true, true),
            LoadOutcome::Lossy(n) => {
                // A lossy decode is a failed read. What is on screen is not
                // what is in the file — every invalid byte became U+FFFD — so
                // saving is refused until an edit says the replacement is
                // wanted, exactly as for an outright read error. Quitting or
                // hiding used to be enough to write the damage back.
                eprintln!("gravitynote: note file was not valid UTF-8; loaded lossily");
                notice = Some(Notice::sticky(concat!(
                    "Note is not valid UTF-8. Shown with the bad bytes ",
                    "replaced; saving is off until you edit it."
                )));
                (n, false, true)
            }
            LoadOutcome::IoError { message } => {
                eprintln!("gravitynote: failed to load note: {message}");
                notice = Some(Notice::sticky(format!(
                    "Could not load note (won't overwrite): {message}"
                )));
                (Note::new(), false, true)
            }
        };

        let index = LineIndex::new(note.text());
        let mut app = Self {
            note,
            index,
            fences: FenceMap::default(),
            history: History::new(),
            caret_alpha: 1.0,
            caret_since: Instant::now(),
            settings,
            find: None,
            find_focused: false,
            settings_open: false,
            backups_open: false,
            backups: Vec::new(),
            recording_shortcut: false,
            focus_handle: cx.focus_handle(),
            list_state: ListState::new(0, ListAlignment::Top, LIST_OVERDRAW),
            // Start at the top: that is where new notes land, and on a
            // 200,000-line document the end is not a useful place to wake up.
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            kill_ring: String::new(),
            is_selecting: false,
            store: Persist::new(path),
            allow_autosave,
            note_on_disk,
            // A first run has no file yet: write the welcome note out rather
            // than leaving it only in memory (and backing up a file that does
            // not exist).
            dirty_since: (!note_on_disk).then(Instant::now),
            saving: false,
            reconcile_backoff: None,
            disk_checked: None,
            notice,
            line_layouts: HashMap::new(),
            row_cache: HashMap::new(),
            revision: 0,
            caret: rows::Caret::at(0),
            viewport: None,
            bank: image_bank::Bank::new(),
            // The store sits beside the note, so this is the note's directory.
            // Resolved once: a row asking the filesystem where it is would be
            // work on the frame path.
            images_dir,
            palette: None,
            reclaim_offered: None,
            image_drag: None,
            drag_position: None,
            drag_scrolling: false,
            drag_granularity: DragGranularity::Char,
            drag_anchor: 0..0,
            scroll_rail: None,
            scroll_drag: None,
            visible_lines: 1,
            context_menu: None,
            // `show_handles` already pins the real lights on screen at startup;
            // the bar they sit on has to come with them or the forced state is
            // only half of what it is there to let someone look at.
            window_buttons_visible: dev::show_handles(),
            match_rail: Vec::new(),
            rail_buckets: Vec::new(),
            last_query: String::new(),
            find_return: None,
            find_search_gen: 0,
            find_search_pending: false,
            find_dirty_since: None,
            find_note_only: false,
            window_was_active: true,
            platform: None,
        };
        app.fences = FenceMap::new(app.note.text(), &app.index);
        app.list_state.reset(app.index.line_count());
        // Reopen where you left off: the caret and the reading position saved on
        // the last close, both clamped to whatever the note is now (it may have
        // been edited elsewhere between sessions).
        let caret = app.note.clamp_offset(app.settings.caret);
        app.selected_range = caret..caret;
        app.caret = rows::Caret::at(caret);
        let top = app
            .settings
            .scroll_top
            .min(app.index.line_count().saturating_sub(1));
        app.list_state.scroll_to(ListOffset {
            item_ix: top,
            offset_in_item: px(0.),
        });
        // A cleanly loaded note is exactly what is on disk: record it as the
        // baseline so autosave stays quiet until an edit, and so the
        // external-change check has a fingerprint to compare against. (A missing
        // file or a refused load has nothing to adopt.)
        if note_on_disk && allow_autosave {
            app.store.adopt_disk(app.note.text());
        }
        app
    }

    // ── Derived state ────────────────────────────────────────────────────────

    /// Re-derive everything after a localized edit: `removed` used to sit at
    /// `start`, `inserted` is there now.
    ///
    /// Every derived structure is patched rather than rebuilt — the line index
    /// via `splice` (95x cheaper than a rebuild on a 10 MB note), the fence map
    /// by splicing the affected rows and walking forward until the state
    /// reconverges, and the list's cached row heights by splicing the same
    /// range. A one-character keystroke therefore touches one row.
    fn after_edit(&mut self, start: usize, removed: &str, inserted: &str) {
        let start = start.min(self.index.len());
        let first_line = self.index.line_at(start);
        let old_line_count = self.index.line_count();
        let removed_lines = removed.matches('\n').count();
        let inserted_lines = inserted.matches('\n').count();

        self.index.splice(self.note.text(), start, removed, inserted);

        // The list caches a measured height per row; tell it which rows moved.
        let replaced = first_line..(first_line + removed_lines + 1).min(old_line_count);
        self.list_state.splice(replaced, inserted_lines + 1);

        let mut fences = std::mem::take(&mut self.fences);
        fences.splice(
            self.note.text(),
            &self.index,
            first_line,
            removed_lines,
            inserted_lines,
        );
        self.fences = fences;

        // The matches move with the text they sit on, and the document is
        // searched again once the typing settles. Scanning per keystroke is
        // 11 ms on a twenty-year note, which is a stutter on every character
        // typed while the find bar is open.
        if let Some(find) = self.find.as_mut() {
            find.shift_after_edit(start, removed.len(), inserted.len());
            self.update_match_rail();
            self.find_dirty_since = Some(Instant::now());
        }
        self.revision = self.revision.wrapping_add(1);
        self.wake_caret();
        self.touch();
    }

    /// Re-derive everything after the whole buffer was rewritten (a note moved,
    /// a new note inserted). Rare enough to afford full rebuilds.
    fn after_structural_change(&mut self) {
        // `reset` below throws away every measured row height, and with it the
        // scroll position — so the line being read has to be put back. Without
        // this, ⌘Z and ⌘⌃↑ jump to the top of the whole document, which in a
        // file that is deliberately one long note is a long way from where you
        // were. The commands that *want* the top ask for it themselves.
        let top = self.list_state.logical_scroll_top().item_ix;
        // Every byte moved, so a composition in flight no longer names the text
        // it was composing. Left alone, the next keystroke of it replaces a span
        // somewhere else in the document — and the IME draws its candidate
        // window over the wrong line. `after_history` has always done this; the
        // note-moving commands are the same shape and did not.
        self.marked_range = None;
        self.index = LineIndex::new(self.note.text());
        let mut fences = std::mem::take(&mut self.fences);
        fences.rebuild(self.note.text(), &self.index);
        self.fences = fences;
        self.list_state.reset(self.index.line_count());
        self.list_state.scroll_to(ListOffset {
            item_ix: top.min(self.index.line_count().saturating_sub(1)),
            offset_in_item: px(0.),
        });
        self.refresh_find();
        self.revision = self.revision.wrapping_add(1);
        self.touch();
    }

    // ── Persistence ──────────────────────────────────────────────────────────

    /// Mark the buffer as changed; the tick loop writes once typing settles.
    fn touch(&mut self) {
        self.enable_autosave_after_edit();
        self.dirty_since = Some(Instant::now());
    }

    /// Write immediately, blocking until the bytes are on disk.
    ///
    /// For the paths that end a session — quitting, hiding, the red button —
    /// where there is no next frame to finish an asynchronous write on.
    /// Everything else goes through [`Self::save_soon`].
    fn flush(&mut self) {
        if !self.may_save() {
            return;
        }
        let outcome = self.store.save_reporting(self.note.text());
        self.record_save(outcome);
    }

    /// Whether a save may start: not after a refused load, and never over an
    /// external change we have not reconciled.
    ///
    /// `reconcile_disk` (on the tick, and on quit) is what preserves the other
    /// copy and re-stamps the file; until it has, defer rather than overwrite —
    /// including on a teardown flush, so the copy on disk survives even when
    /// there is no next tick. `dirty_since` stays set so the next reconcile
    /// picks it up and then saves.
    fn may_save(&mut self) -> bool {
        if !self.allow_autosave {
            self.dirty_since = None;
            return false;
        }
        self.store.disk_changed().is_none()
    }

    /// Start a write on the background executor, and record the result when it
    /// lands.
    ///
    /// The note is written whole, and nine megabytes takes about ten
    /// milliseconds — a stalled frame every time typing settled, and unbounded
    /// on a network volume. The bytes are copied out here and the write happens
    /// elsewhere, so the only thing on the main thread is the copy. A write
    /// already in flight is left to finish: `dirty_since` stays set and the next
    /// tick starts the next one, so saves never overtake each other.
    fn save_soon(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.may_save() {
            return;
        }
        let Some(job) = self.store.begin_save(self.note.text()) else {
            // Already on disk.
            self.dirty_since = None;
            return;
        };
        self.saving = true;
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { job.run() }).await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                this.store.finish_save(&outcome);
                this.record_save(outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// What a finished write means for the rest of the app.
    fn record_save(&mut self, outcome: persist::SaveOutcome) {
        // A job that stood down for newer text wrote nothing: it neither answers
        // an alert nor proves the note is on disk.
        if !outcome.published() {
            if let Err(err) = outcome.result {
                let msg = format!("Save failed: {err}");
                eprintln!("gravitynote: {msg}");
                self.notice = Some(Notice::alert(msg));
            }
            return;
        }
        match outcome.result {
            Ok(()) => {
                // Only if the buffer still holds what was written. A background
                // write carries a snapshot, and everything typed while it was in
                // flight is still owed a save — clearing the debt here left
                // those keystrokes in memory only, with nothing scheduled to
                // write them.
                if !self.store.is_dirty(self.note.text()) {
                    self.dirty_since = None;
                }
                // A save that worked answers whatever the last alert was about —
                // but not a sticky one, which describes a state the app is still
                // in and is checked somewhere else. Clearing those took the
                // "choose again to delete" offer off the screen while the press
                // that deletes was still armed.
                if self.notice.as_ref().is_some_and(|n| n.alert && !n.sticky) {
                    self.notice = None;
                }
                self.note_on_disk = true;
            }
            Err(err) => {
                // Keep `dirty_since` set so the debounce retries a transient
                // failure (a full disk that clears, say) rather than waiting for
                // the next keystroke to reopen the question.
                let msg = format!("Save failed: {err}");
                eprintln!("gravitynote: {msg}");
                self.notice = Some(Notice::alert(msg));
            }
        }
    }

    /// Persist the window's current frame so it reopens where it was left.
    ///
    /// Called on the paths that end a session — hide, red-button close, quit.
    /// GPUI 0.2.2 exposes no move/resize callback to catch every nudge, and the
    /// frame only has to be right at the next launch, so sampling it as the
    /// window goes away is both sufficient and cheap. `window.bounds()` is in the
    /// global display coordinate space, which is exactly what reopening with
    /// `WindowBounds::Windowed` wants back.
    fn save_window_frame(&mut self, window: &Window) {
        let b = window.bounds();
        self.settings.window = Some(settings::WindowFrame {
            x: f32::from(b.origin.x),
            y: f32::from(b.origin.y),
            w: f32::from(b.size.width),
            h: f32::from(b.size.height),
        });
        // The reading position rides along on the same save points, so a relaunch
        // reopens where you were, not at the top.
        self.settings.caret = self.cursor_offset();
        self.settings.scroll_top = self.list_state.logical_scroll_top().item_ix;
        if let Err(err) = self.settings.save(&settings::settings_path()) {
            eprintln!("gravitynote: could not save window frame: {err}");
        }
    }

    /// First user edit after a failed load re-enables saving.
    fn enable_autosave_after_edit(&mut self) {
        if !self.allow_autosave {
            self.allow_autosave = true;
            self.notice = Some(Notice::remark(
                "Editing enabled save (previous file was not overwritten).",
            ));
        }
    }

    /// Called from the tick loop: settle the autosave and take hourly backups.
    fn tick(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.notice.as_ref().is_some_and(Notice::expired) {
            self.notice = None;
            cx.notify();
        }
        // The document was edited under an open find bar and the typing has
        // settled: search it again, which puts back any match the edit made.
        // Until now this ran per keystroke, and on a twenty-year note that is
        // an 11 ms scan between one character and the next.
        if self
            .find_dirty_since
            .is_some_and(|t| t.elapsed() >= FIND_DEBOUNCE)
        {
            self.find_dirty_since = None;
            self.refresh_find();
            cx.notify();
        }
        // Reconcile with the disk before autosave, so a note synced between
        // machines is picked up rather than clobbered. When this reports a
        // conflict it could not safely resolve, hold the flush this tick.
        let block_flush = self.reconcile_disk(false, cx);
        let settled = self
            .dirty_since
            .is_some_and(|t| t.elapsed() >= AUTOSAVE_DEBOUNCE);
        if settled && !block_flush {
            self.save_soon(cx);
            cx.notify();
        }
        if self.allow_autosave && self.note_on_disk {
            if let Err(err) = self.store.maybe_backup(self.note.text(), Local::now()) {
                eprintln!("gravitynote: backup failed: {err}");
            }
        }
    }

    /// Reconcile the buffer with `note.md` when the file changed underneath us
    /// — a sync service or another editor rewrote it. Returns `true` when the
    /// caller must skip this tick's autosave to avoid destroying data.
    ///
    /// This is a data-safety path, so it errs toward never destroying either
    /// copy:
    ///
    /// * **Clean buffer** (matches what we last wrote): adopt the disk copy, so
    ///   the two stay in step. This is the common sync case.
    /// * **Unsaved edits, different disk copy**: the edit and the external copy
    ///   disagree and this app has no dialog to choose between them. Preserve
    ///   the external copy in a backup, then let the in-memory edits win — the
    ///   version the user is looking at is the one they keep, and the one they
    ///   didn't make is on disk in `backups/`. If that backup cannot be written,
    ///   pause autosave rather than overwrite an external change we failed to
    ///   keep.
    /// * **Identical bytes, new mtime**: a sync wrote our own content back;
    ///   adopt the new stamp and move on, with nothing on screen changing.
    fn reconcile_disk(&mut self, force: bool, cx: &mut Context<Self>) -> bool {
        // A note we refused to load (bad UTF-8, I/O error) already has an alert
        // up and autosave off; leave it be. Nothing to reconcile before the
        // note exists on disk either.
        if !(self.allow_autosave && self.note_on_disk) {
            return false;
        }
        // Stat the file on its own clock rather than the tick's, unless there is
        // an unresolved conflict — that one is asked about every tick until it
        // clears, which is what `reconcile_backoff` is for.
        if !force
            && self.reconcile_backoff.is_none()
            && self
                .disk_checked
                .is_some_and(|at| at.elapsed() < DISK_CHECK)
        {
            return false;
        }
        self.disk_checked = Some(Instant::now());
        let Some(current) = self.store.disk_changed() else {
            self.reconcile_backoff = None;
            return false;
        };
        // We already tried and failed to back up this exact disk state. Don't
        // re-read the whole file (up to ~9 MB) or re-raise the alert on every
        // 200 ms tick — just keep blocking the flush until the file moves on.
        if self.reconcile_backoff == Some(current) {
            return true;
        }
        // A fresh disk state gets one reconcile attempt; the backoff is only
        // re-armed if that attempt fails below.
        self.reconcile_backoff = None;

        let disk = match load_note(self.store.path()) {
            LoadOutcome::Loaded(note) => note,
            // Vanished, unreadable, or not UTF-8 right now. Keep showing the
            // last good buffer, but re-stamp so we don't re-read a bad file
            // every tick; the next autosave rewrites it.
            _ => {
                self.store.sync_stamp();
                return false;
            }
        };

        if disk.text() == self.note.text() {
            // Same content, new modification time — a sync echoed our own bytes
            // back. Adopt the stamp so the check goes quiet; nothing moves.
            self.store.adopt_disk(disk.text());
            self.dirty_since = None;
            return false;
        }

        if self.store.is_dirty(self.note.text()) {
            match self.store.backup_now(disk.text(), Local::now()) {
                Ok(_) => {
                    // External copy preserved; let the buffer win from here, but
                    // never silently — say what happened.
                    self.store.sync_stamp();
                    self.notice = Some(Notice::alert(
                        "note.md changed on disk; that version was saved to \
                         backups. Your unsaved edits are kept and will save.",
                    ));
                    cx.notify();
                    false
                }
                Err(err) => {
                    eprintln!("gravitynote: could not back up external change: {err}");
                    // Arm the backoff so the next ticks don't re-read and
                    // re-alert while the disk state is unchanged.
                    self.reconcile_backoff = Some(current);
                    self.notice = Some(Notice::sticky(
                        "note.md changed on disk. Autosave is paused until the \
                         other version can be backed up, so nothing is lost.",
                    ));
                    cx.notify();
                    true
                }
            }
        } else {
            self.adopt_reloaded_note(disk, cx);
            false
        }
    }

    /// Replace the buffer with a note just read from disk, resetting the derived
    /// state the way a fresh load does. The caret and selection are dropped to
    /// the top — the document changed out from under them — but the scroll
    /// position is preserved by [`Self::after_structural_change`].
    fn adopt_reloaded_note(&mut self, note: Note, cx: &mut Context<Self>) {
        self.note = note;
        // The old undo stack described a different document; its spans no longer
        // name anything real.
        self.history = History::new();
        self.marked_range = None;
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.is_selecting = false;
        // A resize in flight names a line of the document that just went away.
        self.image_drag = None;
        self.caret = rows::Caret::at(0);
        self.after_structural_change();
        // The buffer now equals the disk copy: record it as the clean baseline
        // so the reload is not immediately written back, and drop the dirty flag
        // that `after_structural_change` set via `touch`.
        self.store.adopt_disk(self.note.text());
        self.dirty_since = None;
        // If the find bar is open its matches and rail ticks point into the old
        // buffer; re-scan against the reloaded text so the highlights and the
        // rail don't reference bytes that moved.
        if self.find.is_some() {
            self.refresh_find();
        }
        self.notice = Some(Notice::remark("Reloaded — note.md changed on disk."));
        cx.notify();
    }

    /// Turn "open at login" on or off. The system owns the setting, so the menu
    /// is rebuilt from it rather than from anything remembered here.
    fn toggle_open_at_login(
        &mut self,
        _: &ToggleOpenAtLogin,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let want = !login_item::enabled();
        self.notice = Some(match login_item::set(want) {
            Ok(()) if want => Notice::remark("GravityNote will open at login."),
            Ok(()) => Notice::remark("GravityNote will no longer open at login."),
            Err(err) => Notice::alert(format!("Could not change open at login: {err}")),
        });
        // Repaint the menu so its "Open at Login" label picks up the new state.
        cx.set_menus(menus());
        cx.notify();
    }

    /// Take ownership of the platform handle once it has been installed. Called
    /// once, right after `platform::install`, because the view exists before the
    /// handle does. Install already registered the saved chord, so this only
    /// surfaces any warning it produced.
    fn set_platform(&mut self, platform: Rc<RefCell<PlatformHandle>>, cx: &mut Context<Self>) {
        if let Some(warning) = platform.borrow().warning() {
            self.notice = Some(Notice::alert(warning.to_string()));
        }
        self.platform = Some(platform);
        cx.notify();
    }

    /// Whether a panel is laid over the note. Both are modal: while one is up,
    /// no command edits the note behind it — including the ones reached as menu
    /// key equivalents, which bypass the key context entirely.
    fn modal_open(&self) -> bool {
        self.settings_open || self.backups_open || self.palette.is_some()
    }

    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        // ⌘, toggles: a second press closes the panel it opened. (The backups
        // list is a different panel; ⌘, over it opens Settings.)
        if self.settings_open {
            self.close_settings(cx);
            return;
        }
        // Opening Settings closes the find bar: they share the top strip and the
        // one input path, and a panel over a live search would fight it.
        if self.find.is_some() {
            self.dismiss_find();
        }
        // A right-click menu, the backups list and the Settings panel must not
        // stack.
        self.context_menu = None;
        self.backups_open = false;
        self.settings_open = true;
        self.recording_shortcut = false;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.recording_shortcut = false;
        cx.notify();
    }

    /// Persist the chosen show/hide chord (or `None` to turn the global hotkey
    /// off), re-register it with the platform layer, and report the result.
    fn set_toggle_shortcut(&mut self, shortcut: Option<Hotkey>, cx: &mut Context<Self>) {
        self.settings.toggle_shortcut = shortcut.clone();
        if let Err(err) = self.settings.save(&settings::settings_path()) {
            eprintln!("gravitynote: could not save settings: {err}");
        }
        if let Some(platform) = &self.platform {
            platform.borrow_mut().set_shortcut(shortcut.clone());
            self.notice = Some(match platform.borrow().warning() {
                Some(warning) => Notice::alert(warning.to_string()),
                None => Notice::remark(match &shortcut {
                    Some(hotkey) => format!("Show / Hide is now {}", hotkey.label()),
                    None => "Global show / hide shortcut is off".to_string(),
                }),
            });
        }
        cx.notify();
    }

    /// Finish recording a chord from a raw key press in the settings panel.
    ///
    /// Returns whether the press was consumed. A bare key (no modifier) or a key
    /// the app cannot register globally is rejected with a notice rather than
    /// saved, so the recorder can never leave the hotkey in a useless state.
    fn record_shortcut(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let m = &keystroke.modifiers;
        // A lone modifier press (⌘ on its own) is not a chord yet — keep waiting.
        let key = keystroke.key.as_str();
        if key.is_empty() || matches!(key, "cmd" | "ctrl" | "alt" | "shift" | "fn") {
            return;
        }
        // Escape cancels recording without changing anything.
        if key == "escape" {
            self.recording_shortcut = false;
            cx.notify();
            return;
        }
        let hotkey = Hotkey::new(key.to_lowercase(), m.platform, m.control, m.alt, m.shift);
        self.recording_shortcut = false;
        if !hotkey.has_modifier() {
            self.notice = Some(Notice::alert(
                "A global shortcut needs at least one of ⌘ ⌃ ⌥ ⇧. Try again.",
            ));
            cx.notify();
            return;
        }
        if hotkey.carbon().is_none() {
            self.notice = Some(Notice::alert(format!(
                "{} is not a key GravityNote can register globally. Try another.",
                hotkey.label()
            )));
            cx.notify();
            return;
        }
        // A global chord intercepts the key everywhere, so a common one shadows
        // that command in every app while GravityNote runs. Allow it — it is the
        // user's choice — but say so.
        let conflict = common_shortcut_conflict(&hotkey);
        let label = hotkey.label();
        self.set_toggle_shortcut(Some(hotkey), cx);
        if let Some(what) = conflict {
            self.notice = Some(Notice::remark(format!(
                "{label} is now Show / Hide — it will override “{what}” everywhere while GravityNote is running."
            )));
        }
    }

    fn restore_settings_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.reset_text_size();
        self.settings.appearance = settings::Appearance::default();
        self.settings.glass = settings::GlassStyle::default();
        window.set_background_appearance(window_background_for_glass(self.settings.glass));
        self.apply_text_size(cx);
        self.set_toggle_shortcut(settings::default_toggle_shortcut(), cx);
        self.notice = Some(Notice::remark("Settings restored to defaults."));
        cx.notify();
    }

    fn backup_now(&mut self, _: &BackupNow, _: &mut Window, cx: &mut Context<Self>) {
        self.flush();
        match self.store.backup_now(self.note.text(), Local::now()) {
            Ok(Some(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "backup".into());
                self.notice = Some(Notice::remark(format!("Backed up to {name}")));
            }
            // The newest backup already holds this exact text — mashing ⌘S
            // should say so rather than pack the ring with same-minute copies.
            Ok(None) => {
                self.notice = Some(Notice::remark(
                    "Nothing has changed since the last backup.",
                ));
            }
            Err(err) => self.notice = Some(Notice::alert(format!("Backup failed: {err}"))),
        }
        cx.notify();
    }

    // ── Cursor / selection primitives ────────────────────────────────────────

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// Hold the caret solid and restart its rhythm — called whenever the caret
    /// moves or the buffer changes.
    fn wake_caret(&mut self) {
        self.caret_alpha = 1.0;
        self.caret_since = Instant::now();
    }

    /// Advance the caret's fade. Returns whether it moved far enough to be
    /// worth a repaint, and how long until it is worth asking again.
    fn advance_caret(&mut self, window_active: bool) -> (bool, Duration) {
        // Coming back to the window restarts the rhythm: the caret is solid
        // when you return to it, not halfway through a fade.
        if window_active && !self.window_was_active {
            self.wake_caret();
        }
        self.window_was_active = window_active;

        // An unfocused window shows no caret at all, and needs no fine clock
        // to keep not showing one.
        let (want, next) = if window_active {
            caret::phase(self.caret_since.elapsed())
        } else {
            // Long enough that an unfocused window draws nothing and costs
            // almost nothing, short enough that coming back to it by ⌘Tab does
            // not leave the caret missing for a noticeable moment.
            (0.0, CARET_IDLE_POLL)
        };
        let moved = (want - self.caret_alpha).abs() >= 1. / 255.;
        self.caret_alpha = want;
        (moved, next)
    }

    /// Scroll the caret's line into view by the shortest distance.
    fn follow_caret(&mut self) {
        // Keep a couple of lines of context around the caret rather than pinning
        // it flush to the edge it came in from. Revealing line+MARGIN then
        // line−MARGIN scrolls only when the caret is within MARGIN of an edge, so
        // typing in the middle of the screen never jumps — but typing on the last
        // visible line scrolls up to leave room below, the way every good editor
        // does. The second reveal wins when both cannot fit, which keeps the caret
        // itself on screen on a very short viewport.
        const MARGIN: usize = 2;
        let line = self.index.line_at(self.cursor_offset());
        let last = self.index.line_count().saturating_sub(1);
        self.list_state
            .scroll_to_reveal_item((line + MARGIN).min(last));
        self.list_state
            .scroll_to_reveal_item(line.saturating_sub(MARGIN));
    }

    /// Bring the caret into view with room around it.
    ///
    /// Revealing a line puts it flush against whichever edge it came in from.
    /// That is right for typing — you were already looking there — and wrong for
    /// a search hit, which lands with no context on the side you arrived from.
    fn reveal_caret_with_context(&mut self) {
        const CONTEXT_LINES: usize = 3;
        let line = self.index.line_at(self.cursor_offset());
        let last = self.index.line_count().saturating_sub(1);
        self.list_state
            .scroll_to_reveal_item((line + CONTEXT_LINES).min(last));
        self.list_state
            .scroll_to_reveal_item(line.saturating_sub(CONTEXT_LINES));
    }

    fn scroll_to_top(&mut self) {
        self.list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    }

    /// Set the selection without disturbing the undo group. Used by the edit
    /// commands that select-then-replace (backspace, word delete) — those are a
    /// continuation of typing, not a caret jump.
    fn set_selection(&mut self, start: usize, end: usize) {
        let (start, end) = self.note.clamp_range(start, end);
        self.selected_range = start..end;
        self.selection_reversed = false;
        self.reset_row_motion();
        self.wake_caret();
    }

    /// Every caret change that is not a row step lands on the plain side of a
    /// wrap and forfeits the goal column. The row-wise moves restore both after
    /// moving, which keeps every other path honest by default.
    fn reset_row_motion(&mut self) {
        self.caret = rows::Caret::at(self.cursor_offset());
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.note.clamp_offset(offset);
        self.reset_row_motion();
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.wake_caret();
        // A caret jump ends the current undo group: typing here should not merge
        // with typing somewhere else.
        self.history.break_group();
        self.follow_caret();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.extend_selection(offset);
        self.follow_caret();
        cx.notify();
    }

    /// Move the far end of the selection, leaving the view where it is.
    ///
    /// Split out from [`Self::select_to`] for the drag case: there the pointer
    /// decides what is on screen, and scrolling to reveal the caret would undo
    /// the scroll the drag just asked for.
    fn extend_selection(&mut self, offset: usize) {
        let offset = self.note.clamp_offset(offset);
        self.reset_row_motion();
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        let (a, b) = self
            .note
            .clamp_range(self.selected_range.start, self.selected_range.end);
        self.selected_range = a..b;
        self.wake_caret();
        self.history.break_group();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        index::prev_grapheme(self.note.text(), &self.index, offset)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        index::next_grapheme(self.note.text(), &self.index, offset)
    }

    fn previous_word(&self, offset: usize) -> usize {
        selection::prev_word_start(self.note.text(), offset)
    }

    fn next_word(&self, offset: usize) -> usize {
        selection::next_word_end(self.note.text(), offset)
    }

    /// The wrap affinity a horizontal move to `offset` should carry.
    ///
    /// `move_to`/`select_to` reset the caret to plain [`Affinity::RowEnd`], which
    /// is wrong when the move lands exactly on a soft-wrap boundary: the offset
    /// names both the tail of the row above and the head of the row below, and
    /// the layout resolves the bare offset to the tail — so ←/→/word motion onto
    /// a wrap drew the caret at the end of the row above and looked stuck. A
    /// boundary belongs at the head of the wrapped row, the same place a click
    /// there lands. Off-screen lines have no geometry and cannot wrap-render, so
    /// they stay [`Affinity::RowEnd`]. See [`rows::is_wrap_boundary`].
    fn horizontal_affinity(&self, offset: usize) -> rows::Affinity {
        let line = self.index.line_at(offset);
        let Some(row) = self.line_layouts.get(&line) else {
            return rows::Affinity::RowEnd;
        };
        let next = index::next_grapheme(self.note.text(), &self.index, offset);
        if rows::is_wrap_boundary(row, offset, next) {
            rows::Affinity::RowStart
        } else {
            rows::Affinity::RowEnd
        }
    }

    /// Point the caret at the right side of a wrap after a horizontal move.
    fn set_horizontal_affinity(&mut self) {
        self.caret.affinity = self.horizontal_affinity(self.cursor_offset());
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let text = self.note.text();
        self.index.to_utf16(text, range.start)..self.index.to_utf16(text, range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        let text = self.note.text();
        self.index.from_utf16(text, range_utf16.start)
            ..self.index.from_utf16(text, range_utf16.end)
    }

    // ── Note-level actions ───────────────────────────────────────────────────

    fn new_note(&mut self, _: &NewNote, _: &mut Window, cx: &mut Context<Self>) {
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so this command has to refuse for
        // itself while the find bar has the keyboard or the Settings panel is up.
        // A body click that blurred the bar hands editing back, so this runs again.
        if self.find_focused || self.modal_open() {
            return;
        }
        let before_selection = (self.selected_range.start, self.selected_range.end);
        let was_empty = self.note.is_empty();
        let cursor = self.note.new_block_at_top();
        if !was_empty {
            // Starting a note prepends a rule and nothing else, so it is an
            // ordinary insertion at byte 0 — the index, the fence map and the
            // measured row heights are patched, not thrown away.
            let inserted = self.note.text()[..note::NEW_BLOCK_PREFIX.len()].to_string();
            self.history.break_group();
            self.history.record(
                Edit {
                    start: 0,
                    removed: String::new(),
                    inserted,
                    before: before_selection,
                    after: (cursor, cursor),
                },
                Instant::now(),
            );
            self.history.break_group();
            self.after_edit(0, "", note::NEW_BLOCK_PREFIX);
        }
        self.move_to(cursor, cx);
        self.save_soon(cx);
        self.scroll_to_top();
        cx.notify();
    }

    /// Promote the note containing the cursor to the top of the document.
    fn bring_note_up(&mut self, _: &BringNoteUp, _: &mut Window, cx: &mut Context<Self>) {
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so this command has to refuse for
        // itself while the find bar has the keyboard or the Settings panel is up.
        // A body click that blurred the bar hands editing back, so this runs again.
        if self.find_focused || self.modal_open() {
            return;
        }
        let offset = self.cursor_offset();
        let (idx, start) = self.note.block_at(&self.index, offset);
        self.promote_block(idx, offset.saturating_sub(start), cx);
    }

    /// Swap the note containing the cursor with the one above it.
    fn move_note_up(&mut self, _: &MoveNoteUp, _: &mut Window, cx: &mut Context<Self>) {
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so this command has to refuse for
        // itself while the find bar has the keyboard or the Settings panel is up.
        // A body click that blurred the bar hands editing back, so this runs again.
        if self.find_focused || self.modal_open() {
            return;
        }
        let offset = self.cursor_offset();
        let (idx, start) = self.note.block_at(&self.index, offset);
        let within = offset.saturating_sub(start);
        if let Some(moved) = self.note.plan_move_block_up_one(&self.index, idx) {
            self.commit_block_move(moved, within, cx);
        }
        cx.notify();
    }

    /// Move block `idx` to the top, putting the caret `within` bytes into it.
    fn promote_block(&mut self, idx: usize, within: usize, cx: &mut Context<Self>) {
        if let Some(moved) = self.note.plan_bring_block_up(&self.index, idx) {
            self.commit_block_move(moved, within, cx);
            self.scroll_to_top();
        }
        cx.notify();
    }

    /// Land a note that has changed places: patch the derived state, record the
    /// move as the two edits it was, and put the caret back inside the note.
    ///
    /// A move used to rewrite the buffer and be recorded as "the single run in
    /// which the old and new documents differ" — which, for a note coming from
    /// the bottom of a twenty-year file, is everything above it, twice. It is a
    /// removal here and an insertion there, and saying so keeps both the undo
    /// entry and the work the size of the note that moved.
    fn commit_block_move(&mut self, moved: note::BlockMove, within: usize, cx: &mut Context<Self>) {
        let before_selection = (self.selected_range.start, self.selected_range.end);
        let caret = moved.moved_to + within.min(moved.insert_text.len());
        // Apply the two halves one at a time, each through the same incremental
        // path an ordinary edit takes, so the index, the fence map and the
        // measured row heights are patched rather than rebuilt. They have to be
        // separate: a splice describes one edit against the buffer as it stands.
        let cut_end = moved.cut_at + moved.cut_text.len();
        self.note.replace_range(moved.cut_at, cut_end, "");
        self.after_edit(moved.cut_at, &moved.cut_text, "");
        self.note
            .replace_range(moved.insert_at, moved.insert_at, &moved.insert_text);
        self.after_edit(moved.insert_at, "", &moved.insert_text);
        self.history.record_compound(
            vec![
                Edit {
                    start: moved.cut_at,
                    removed: moved.cut_text,
                    inserted: String::new(),
                    before: before_selection,
                    after: (moved.cut_at, moved.cut_at),
                },
                Edit {
                    start: moved.insert_at,
                    removed: String::new(),
                    inserted: moved.insert_text,
                    before: (moved.cut_at, moved.cut_at),
                    after: (caret, caret),
                },
            ],
            Instant::now(),
        );
        // Every offset below the move has shifted, so a composition in flight
        // no longer names the text it was composing.
        self.marked_range = None;
        self.move_to(caret, cx);
        self.save_soon(cx);
    }

    /// Promote the note that sits *below* separator line `sep_line`.
    fn promote_block_below_separator(&mut self, sep_line: usize, cx: &mut Context<Self>) {
        // Clicking the chip is a deliberate ask, like the status item's "New
        // Note" — so the find bar gets out of the way rather than the command
        // being dropped. Leaving find open would also strand it: the selection
        // it marks the current hit with is about to be moved.
        self.dismiss_find();
        let offset = self.index.line_start(sep_line);
        let (idx, _) = self.note.block_at(&self.index, offset);
        self.promote_block(idx, 0, cx);
    }

    // ── Window-level actions ─────────────────────────────────────────────────

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        self.flush();
        cx.quit();
    }

    fn toggle_window(&mut self, _: &ToggleWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.set_visible(!window.is_window_active(), window, cx);
    }

    // ── Window & app (the standard macOS menu commands) ─────────────────────

    fn minimize(&mut self, _: &Minimize, window: &mut Window, _cx: &mut Context<Self>) {
        window.minimize_window();
    }

    fn zoom_window(&mut self, _: &ZoomWindow, window: &mut Window, _cx: &mut Context<Self>) {
        window.zoom_window();
    }

    fn toggle_full_screen(&mut self, _: &ToggleFullScreen, window: &mut Window, _cx: &mut Context<Self>) {
        window.toggle_fullscreen();
    }

    fn hide_app(&mut self, _: &HideApp, _window: &mut Window, cx: &mut Context<Self>) {
        cx.hide();
    }

    fn hide_others(&mut self, _: &HideOthers, _window: &mut Window, _cx: &mut Context<Self>) {
        platform::hide_others();
    }

    fn show_all_apps(&mut self, _: &ShowAllApps, _window: &mut Window, _cx: &mut Context<Self>) {
        platform::show_all();
    }

    fn about_app(&mut self, _: &AboutApp, _window: &mut Window, _cx: &mut Context<Self>) {
        platform::show_about_panel();
    }

    /// Reveal the note file (and thus its folder, holding the backups) in Finder.
    fn reveal_in_finder(&mut self, _: &RevealInFinder, _window: &mut Window, cx: &mut Context<Self>) {
        cx.reveal_path(self.store.path());
    }

    /// Export a copy of the note to a location the user picks. The note is one
    /// markdown file; this writes its current text out under a new name without
    /// touching the working file.
    fn export_note(&mut self, _: &ExportNote, _window: &mut Window, cx: &mut Context<Self>) {
        // Image references are relative to the store, which the copy is not
        // going to sit next to. Point them at the files themselves, so the
        // exported note resolves wherever it lands rather than carrying links
        // that only worked from one directory.
        let text = images::absolutise(self.note.text(), &self.images_dir);
        let dir = self
            .store
            .path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let receiver = cx.prompt_for_new_path(&dir, Some("note.md"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            // A full disk, a read-only volume, a folder that went away: the
            // export used to fail in complete silence — no file, no message.
            let written = cx
                .background_executor()
                .spawn(async move {
                    let result = std::fs::write(&path, text);
                    (path, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.notice = Some(match written {
                    (path, Ok(())) => Notice::remark(format!(
                        "Exported to {}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    )),
                    (_, Err(err)) => Notice::alert(format!("Export failed: {err}")),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Show the backups, so the note you lost on Tuesday is reachable without
    /// going through the Finder. Reads the list — and the head of each file —
    /// when it opens, which is the only moment it can be out of date.
    fn open_backups(&mut self, _: &OpenBackups, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            cx.notify();
            return;
        }
        if self.backups_open {
            self.backups_open = false;
            cx.notify();
            return;
        }
        if self.find.is_some() {
            self.dismiss_find();
        }
        self.context_menu = None;
        self.settings_open = false;
        // Settings may have been mid-recording; that capture layer swallows
        // every key while it is armed, including the Escape that would close
        // this panel.
        self.recording_shortcut = false;
        self.backups = self.store.backup_list();
        self.backups_open = true;
        cx.notify();
    }

    /// Put a backup back, keeping what is on screen now.
    ///
    /// The current note is written to its own backup *first*, so "restore" can
    /// never be the thing that loses the writing you did this morning: the step
    /// is always reversible by restoring the backup this makes.
    fn restore_backup(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let restored = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                self.notice = Some(Notice::alert(format!("Could not read that backup: {err}")));
                self.backups_open = false;
                cx.notify();
                return;
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let safety = match self.store.backup_now(self.note.text(), Local::now()) {
            Ok(Some(made)) => made
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            // Already backed up byte for byte; nothing to preserve.
            Ok(None) => String::new(),
            Err(err) => {
                self.notice = Some(Notice::alert(format!(
                    "Not restoring: the note as it stands could not be backed up first ({err})"
                )));
                self.backups_open = false;
                cx.notify();
                return;
            }
        };

        self.backups_open = false;
        self.adopt_reloaded_note(Note::from_text(restored), cx);
        // `adopt_reloaded_note` records the buffer as what is on disk, which is
        // right for a reload of the note's own file and wrong here: this text
        // came from a backup, and `note.md` still holds the old note. Saying so
        // is what makes the save below actually write — without it the restore
        // was only ever on screen, and the next launch undid it.
        self.store.mark_dirty();
        self.touch();
        self.save_soon(cx);
        self.notice = Some(Notice::remark(if safety.is_empty() {
            format!("Restored {name}.")
        } else {
            format!("Restored {name}. The note as it was is backed up as {safety}.")
        }));
        cx.notify();
    }

    // ── The command palette ─────────────────────────────────────────────────

    /// ⌘⇧P. Every command the menus hold, filtered as you type.
    ///
    /// The menu bar is the only route to most of sixty-odd commands, and finding
    /// one means remembering which of three menus it is under. This is the same
    /// list, searched instead of navigated.
    fn open_palette(&mut self, _: &OpenPalette, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.palette = None;
            cx.notify();
            return;
        }
        if self.find.is_some() {
            self.close_find_bar(cx);
        }
        self.settings_open = false;
        self.backups_open = false;
        self.context_menu = None;
        self.palette = Some(Palette::default());
        cx.notify();
    }

    fn palette_step(&mut self, _: &PaletteStep, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_palette(1, cx);
    }

    fn palette_step_back(&mut self, _: &PaletteStepBack, _w: &mut Window, cx: &mut Context<Self>) {
        self.move_palette(-1, cx);
    }

    fn move_palette(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.palette_matches().len();
        if let Some(palette) = self.palette.as_mut() {
            if count == 0 {
                palette.selected = 0;
            } else {
                let next = palette.selected as isize + by;
                // Wraps, so holding ↓ walks the list round rather than sticking.
                palette.selected = next.rem_euclid(count as isize) as usize;
            }
        }
        cx.notify();
    }

    /// Run whatever is selected, and put the palette away first — the command
    /// acts on the note, and half of them refuse while a panel is up.
    fn palette_run(&mut self, _: &PaletteRun, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        let Some(command) = self
            .palette
            .as_ref()
            .and_then(|p| matches.get(p.selected).copied())
        else {
            return;
        };
        self.palette = None;
        cx.notify();
        (command.run)(self, window, cx);
    }

    /// The commands whose names contain the query, in order. The query is
    /// matched loosely — the letters in order, not necessarily together — so
    /// "nn" finds "New Note" and "rev" finds "Reveal in Finder".
    fn palette_matches(&self) -> Vec<&'static Command> {
        let query = self
            .palette
            .as_ref()
            .map(|p| p.query.to_ascii_lowercase())
            .unwrap_or_default();
        COMMANDS
            .iter()
            .filter(|command| subsequence(&command.name.to_ascii_lowercase(), &query))
            .collect()
    }

    /// Report — and, on a second press, delete — the images in the store that
    /// the note no longer refers to.
    ///
    /// Two presses rather than a dialog: the first says what would go and how
    /// much it is worth, the second does it. Deleting files is not something to
    /// do on the same click that discovered them.
    fn reclaim_images(&mut self, _: &ReclaimImages, _window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // Every backup counts as a reader. The store is append-only precisely so
        // that a backup taken last month still shows the picture it was taken
        // with; deleting a file the note dropped this morning would rot every one
        // of them into a dead link.
        let mut referenced = std::collections::HashSet::new();
        images::referenced_names(self.note.text(), &mut referenced);
        for path in self.store.backups() {
            // One at a time, and only the names kept: a hundred backups of a
            // nine-megabyte note is a gigabyte of text nobody needs to hold.
            if let Ok(text) = std::fs::read_to_string(&path) {
                images::referenced_names(&text, &mut referenced);
            }
        }
        let orphans = images::unreferenced(&self.images_dir, &referenced);
        if orphans.is_empty() {
            self.notice = Some(Notice::remark(
                "Every image in the store is still used by the note.",
            ));
            cx.notify();
            return;
        }
        let total: u64 = orphans.iter().map(|(_, bytes)| bytes).sum();
        let count = orphans.len();
        let offered = self
            .reclaim_offered
            .is_some_and(|at| at.elapsed() < RECLAIM_CONFIRM);
        if !offered {
            self.reclaim_offered = Some(Instant::now());
            // Sticky: an ordinary alert is cleared by the next successful save,
            // and an offer that vanishes leaves the second press looking like
            // the first.
            self.notice = Some(Notice::sticky(format!(
                "{count} image{} no longer used by the note or any backup, {}. \
                 Choose Reclaim Unused Images again to delete them.",
                if count == 1 { "" } else { "s" },
                human_bytes(total)
            )));
            cx.notify();
            return;
        }
        self.reclaim_offered = None;
        let mut failed = 0usize;
        for (path, _) in &orphans {
            if std::fs::remove_file(path).is_err() {
                failed += 1;
            }
        }
        self.notice = Some(if failed == 0 {
            Notice::remark(format!("Reclaimed {}.", human_bytes(total)))
        } else {
            Notice::alert(format!("{failed} of {count} images could not be deleted."))
        });
        cx.notify();
    }

    /// Choose the folder the note lives in.
    ///
    /// The whole of "sync", for anyone who keeps their notes in iCloud Drive or
    /// Dropbox: the file is plain markdown in a normal directory, and this says
    /// which directory. The note is written to the new folder before the app
    /// starts reading from it, so nothing is lost if the folder is empty — and
    /// an existing `note.md` there is adopted rather than overwritten.
    fn choose_note_folder(&mut self, _: &ChooseNoteFolder, _w: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        let options = gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        };
        let chosen = cx.prompt_for_paths(options);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.move_note_to(folder, cx));
        })
        .detach();
    }

    /// Point the app at `folder`, keeping whichever note is the real one.
    fn move_note_to(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let target = folder.join("note.md");
        if target == *self.store.path() {
            self.notice = Some(Notice::remark("The note is already in that folder."));
            cx.notify();
            return;
        }
        // A write of the *old* file is still in the air, and it carries the old
        // path: letting it land after the switch would mark the new store clean
        // without anything ever being written there. Asking again in a moment is
        // cheap; losing the note is not.
        if self.saving {
            self.notice = Some(Notice::alert(
                "A save is in progress — try changing the folder again in a moment.",
            ));
            cx.notify();
            return;
        }
        // Write what is on screen out before switching, so nothing is left only
        // in the old file. `flush` is the blocking save, and it refuses when the
        // old file changed underneath us — in which case the move waits too.
        self.flush();
        if self.dirty_since.is_some() {
            self.notice = Some(Notice::alert(
                "The note could not be saved where it is now, so it was not moved.",
            ));
            cx.notify();
            return;
        }
        // Prove the folder is writable before anything is committed to it: a
        // read-only volume would otherwise be remembered, and the next launch
        // would find no note there and open the welcome text.
        let probe = folder.join(".gravitynote-write-test");
        if let Err(err) = std::fs::write(&probe, b"") {
            self.notice = Some(Notice::alert(format!("Cannot write to that folder: {err}")));
            cx.notify();
            return;
        }
        let _ = std::fs::remove_file(&probe);
        // A note already there wins — but only if it can be read. Refusing here
        // leaves everything as it was; repointing at a file we cannot read would
        // mean the next keystroke wrote over it.
        let adopting = target.exists();
        let adopted = if adopting {
            match load_note(&target) {
                LoadOutcome::Loaded(note) => Some(note),
                _ => {
                    self.notice = Some(Notice::alert(format!(
                        "There is a note.md in {} that cannot be read. Nothing was moved.",
                        folder.display()
                    )));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        // Remember the choice *before* anything is moved, and put the setting
        // back if it cannot be written: a folder that lives only in memory is
        // saved by the next window-frame write on quit, and the next launch
        // then opens the welcome note in a folder the note was never moved to.
        if let Err(err) = gravitynote::sandbox::remember(&folder) {
            self.notice = Some(Notice::alert(format!("Could not keep access to that folder: {err}")));
            cx.notify();
            return;
        }
        let previous = self.settings.note_folder.take();
        self.settings.note_folder = Some(folder.clone());
        if let Err(err) = self.settings.save(&settings::settings_path()) {
            self.settings.note_folder = previous;
            self.notice = Some(Notice::alert(format!("Could not save the folder: {err}")));
            cx.notify();
            return;
        }
        // The pictures live beside the note, and the references to them are
        // relative — so they travel with it, or every image in the note breaks.
        // After the setting, because a failure here can be told about and fixed
        // by hand, while a store moved out from under a note that did not move
        // cannot.
        if let Err(err) = images::move_store(&self.images_dir, &folder) {
            self.notice = Some(Notice::alert(format!(
                "The note moved, but its images could not: {err}. They are still in {}.",
                self.images_dir.display()
            )));
        }
        self.store = Persist::new(target.clone());
        self.images_dir = folder;
        if let Some(note) = adopted {
            // There is already a note there; it wins, and the old one is still
            // where it was.
            self.adopt_reloaded_note(note, cx);
            self.notice = Some(Notice::remark(format!(
                "Now using the note already in {}.",
                target.display()
            )));
        } else {
            self.store.mark_dirty();
            self.touch();
            self.save_soon(cx);
            self.notice = Some(Notice::remark(format!("The note now lives in {}.", target.display())));
        }
        self.bank = image_bank::Bank::new();
        cx.notify();
    }

    /// A short orientation, since there is no bundled help book. The menu-bar
    /// Help search field (which macOS attaches to this menu) finds every command.
    fn show_help(&mut self, _: &ShowHelp, _window: &mut Window, cx: &mut Context<Self>) {
        self.notice = Some(Notice::remark(
            "Commands live in the menu bar. ⌘F finds, ⌥⌘F replaces, ⌘, opens Settings, ⌃A shows/hides the window.",
        ));
        cx.notify();
    }

    /// Bring the window forward, or hide the whole app.
    fn set_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        if visible {
            cx.activate(true);
            window.activate_window();
            window.focus(&self.focus_handle);
            // ⌃A is the app's signature move, and it must land on a window that
            // looks alive: a solid caret straight away rather than up to a full
            // cycle of nothing, and no chrome left over from last time.
            self.wake_caret();
            self.window_was_active = true;
            self.set_window_buttons_visible(false, cx);
        } else {
            self.flush();
            // Remember where the window sat, so unhiding — or the next launch —
            // brings it back to the same place rather than re-centering.
            self.save_window_frame(window);
            // A drag in progress ends here. Without this the scroll loop keeps
            // running against a hidden window, extending the selection to the
            // end of the note, because the mouse-up never arrives.
            self.is_selecting = false;
            self.drag_position = None;
            cx.hide();
        }
    }

    fn on_platform_event(
        &mut self,
        event: PlatformEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PlatformEvent::Toggle => self.set_visible(!window.is_window_active(), window, cx),
            PlatformEvent::Show => self.set_visible(true, window, cx),
            PlatformEvent::NewNote => {
                self.set_visible(true, window, cx);
                // A click on the status item is not a stray key equivalent: it
                // asks for a new note, so the find bar gets out of the way
                // rather than swallowing it.
                self.dismiss_find();
                self.new_note(&NewNote, window, cx);
            }
            PlatformEvent::BackupNow => self.backup_now(&BackupNow, window, cx),
            PlatformEvent::Quit => {
                self.flush();
                cx.quit();
            }
        }
        cx.notify();
    }

    // ── Editing actions ──────────────────────────────────────────────────────

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
        self.set_horizontal_affinity();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
        self.set_horizontal_affinity();
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        // Collapse to the near edge first, like plain ←: a forward selection sent
        // ⌥← word-left from its *end*, landing inside itself.
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.start
        };
        self.move_to(self.previous_word(from), cx);
        self.set_horizontal_affinity();
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.end
        };
        self.move_to(self.next_word(from), cx);
        self.set_horizontal_affinity();
    }

    /// Show or hide the window's traffic lights, at most once per change.
    fn set_window_buttons_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        // `show_handles` is there to hold a hover-only control still for as long
        // as it takes to look at it, so it has to survive the pointer leaving —
        // which is the one thing that was always about to happen next.
        let visible = visible || dev::show_handles();
        if self.window_buttons_visible == visible {
            return;
        }
        self.window_buttons_visible = visible;
        platform::set_window_buttons_visible(visible);
        cx.notify();
    }

    /// Move the caret one visual row, extending the selection or not.
    fn step_row(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let caret = self.caret_state();
        let moved = rows::step(self.note.text(), &self.index, &self.line_layouts, caret, down);
        self.apply_caret(moved, extend, cx);
    }

    /// Move the caret to the start or end of its visual row.
    fn go_to_row_edge(&mut self, to_end: bool, extend: bool, cx: &mut Context<Self>) {
        let caret = self.caret_state();
        let moved =
            rows::row_edge(self.note.text(), &self.index, &self.line_layouts, caret, to_end);
        self.apply_caret(moved, extend, cx);
    }

    /// Where the caret is now, as `rows` understands it.
    fn caret_state(&self) -> rows::Caret {
        rows::Caret {
            offset: self.cursor_offset(),
            ..self.caret
        }
    }

    /// Commit a caret the row arithmetic worked out. `move_to` and `select_to`
    /// reset the row-motion state, so it is restored afterwards — which keeps
    /// every other caret path honest by default.
    fn apply_caret(&mut self, caret: rows::Caret, extend: bool, cx: &mut Context<Self>) {
        if extend {
            self.select_to(caret.offset, cx);
        } else {
            self.move_to(caret.offset, cx);
        }
        self.caret = caret;
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.collapse_for_vertical(false);
        self.step_row(false, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.collapse_for_vertical(true);
        self.step_row(true, false, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_page(false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_page(true, cx);
    }

    fn paragraph_up(&mut self, _: &ParagraphUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.paragraph_target(false), cx);
    }

    fn paragraph_down(&mut self, _: &ParagraphDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.paragraph_target(true), cx);
    }

    fn select_paragraph_up(&mut self, _: &SelectParagraphUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.paragraph_target(false), cx);
    }

    fn select_paragraph_down(
        &mut self,
        _: &SelectParagraphDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.paragraph_target(true), cx);
    }

    /// Where ⌥↑ / ⌥↓ land: the boundary of the prose paragraph the caret is in,
    /// a paragraph being a run of non-blank lines. ⌥↓ jumps to the first line of
    /// the next paragraph (or the document end); ⌥↑ to the start of the current
    /// paragraph, or the previous one's start when already there. Blank lines are
    /// the gaps between paragraphs — the same notion the reader sees.
    fn paragraph_target(&self, down: bool) -> usize {
        let text = self.note.text();
        let count = self.index.line_count();
        let is_blank = |l: usize| {
            self.index
                .line_range(l)
                .map_or(true, |(a, b)| text[a..b].trim().is_empty())
        };
        let caret = self.cursor_offset();
        let line = self.index.line_at(caret);
        if down {
            let mut l = line;
            while l < count && !is_blank(l) {
                l += 1;
            }
            while l < count && is_blank(l) {
                l += 1;
            }
            if l >= count {
                self.note.len()
            } else {
                self.index.line_start(l)
            }
        } else {
            let mut start = line;
            while start > 0 && !is_blank(start - 1) {
                start -= 1;
            }
            let para_start = self.index.line_start(start);
            if caret > para_start {
                para_start
            } else {
                let mut l = start;
                while l > 0 && is_blank(l - 1) {
                    l -= 1;
                }
                while l > 0 && !is_blank(l - 1) {
                    l -= 1;
                }
                self.index.line_start(l)
            }
        }
    }

    /// Scroll one near-screenful, the way Page Up / Page Down do everywhere else.
    /// The caret stays put — this is a reading motion, like the scroll wheel — and
    /// the next edit reveals it again through `follow_caret`. Off the caret path,
    /// so the virtualized layout never has to reach for off-screen rows.
    fn scroll_page(&mut self, down: bool, cx: &mut Context<Self>) {
        // A line of overlap kept for context, matching the rail's own paging.
        let page = self
            .viewport
            .map(|v| v.size.height)
            .unwrap_or_else(|| px(self.settings.text_size * LINE_HEIGHT * 20.))
            * 0.9;
        self.list_state.scroll_by(if down { page } else { -page });
        cx.notify();
    }

    /// A non-extending ↑/↓ over a selection leaves from the selection's near
    /// edge — top for ↑, bottom for ↓ — the way macOS does, instead of stepping
    /// from the active end, which for a forward selection sits at the bottom and
    /// sent ↑ one row *into* the selection.
    fn collapse_for_vertical(&mut self, down: bool) {
        if self.selected_range.is_empty() {
            return;
        }
        let edge = if down {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.selected_range = edge..edge;
        self.selection_reversed = false;
        self.caret = rows::Caret::at(edge);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        self.set_horizontal_affinity();
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
        self.set_horizontal_affinity();
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_row(false, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_row(true, true, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_row_edge(false, true, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_row_edge(true, true, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so while the find bar holds the
        // keyboard ⌘A selects the query, not the note behind it. A blurred bar
        // leaves ⌘A to the note.
        if self.find_focused {
            if let Some(find) = self.find.as_mut() {
                find.select_all();
                self.wake_caret();
                cx.notify();
            }
            return;
        }
        self.selected_range = 0..self.note.len();
        self.selection_reversed = false;
        self.reset_row_motion();
        self.history.break_group();
        self.wake_caret();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        // ⌘← / Home go straight to the start of the visual row (column 0), the
        // way macOS's own moveToLeftEndOfLine does — no smart-home two-step. Home
        // and ⇧Home therefore agree on an indented line, and on a wrapped line
        // both stop at the head of the row on screen, not the logical line.
        self.go_to_row_edge(false, false, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_row_edge(true, false, cx);
    }

    fn document_start(&mut self, _: &DocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.note.len(), cx);
    }

    fn select_to_document_start(
        &mut self,
        _: &SelectToDocumentStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    fn select_to_document_end(
        &mut self,
        _: &SelectToDocumentEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.note.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.as_mut() {
            palette.query.pop();
            palette.selected = 0;
            cx.notify();
            return;
        }
        if self.find_focused {
            // Deletes the selection whole when the query is offered, else the
            // character before the caret. The scan follows once typing settles.
            if let Some(find) = self.find.as_mut() {
                find.delete_backward();
            }
            self.schedule_query_search(cx);
            return;
        }
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            // A rule above the caret goes as one thing: deleting it character
            // by character would reveal `---` the user never typed.
            if self.delete_rule_before(cursor, window, cx) {
                return;
            }
            let prev = self.previous_boundary(cursor);
            if cursor == prev {
                return;
            }
            // Delete the span without selecting it first: the selection at the
            // time is what undo restores, and a caret is what was there. Making
            // it a selection meant undoing five backspaces left one character
            // highlighted in the middle of the five it had just restored.
            let range = self.range_to_utf16(&(prev..cursor));
            self.replace_text_in_range(Some(range), "", window, cx);
            return;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_key(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            if let Some(rule) =
                note::separator_after(self.note.text(), &self.index, &self.fences, cursor)
            {
                let utf16 = self.range_to_utf16(&rule);
                self.replace_text_in_range(Some(utf16), "", window, cx);
                return;
            }
            let next = self.next_boundary(cursor);
            if cursor == next {
                return;
            }
            self.set_selection(cursor, next);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Delete the given range, unless the user already has a selection — then
    /// the selection wins, which is what every Mac text field does.
    fn delete_range_or_selection(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            if range.is_empty() {
                return;
            }
            let utf16 = self.range_to_utf16(&range);
            self.replace_text_in_range(Some(utf16), "", window, cx);
            return;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_back(&mut self, _: &DeleteWordBack, window: &mut Window, cx: &mut Context<Self>) {
        let caret = self.cursor_offset();
        // Only when there is nothing selected: a selection is what the user
        // asked to delete, and it wins over anything this command would find.
        if self.selected_range.is_empty() && self.delete_rule_before(caret, window, cx) {
            return;
        }
        let mut range = selection::delete_word_back_range(self.note.text(), caret);
        range.start = note::clamp_within_note(
            self.note.text(),
            &self.index,
            &self.fences,
            range.start,
            caret,
        );
        self.delete_range_or_selection(range, window, cx);
    }

    /// Delete the whole rule above `caret`, if that is what a backwards delete
    /// there means. Returns whether it did.
    fn delete_rule_before(
        &mut self,
        caret: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(rule) =
            note::separator_before(self.note.text(), &self.index, &self.fences, caret)
        else {
            return false;
        };
        // An explicit range, so undo restores a caret rather than handing back
        // the invisible rule *selected* — where the next keystroke would
        // silently delete it again.
        let utf16 = self.range_to_utf16(&rule);
        self.replace_text_in_range(Some(utf16), "", window, cx);
        true
    }

    fn delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caret = self.cursor_offset();
        let mut range = selection::delete_word_forward_range(self.note.text(), caret);
        // The mirror of the backward guard: a forward word-delete at the end of
        // a note must not reach through the `---` into the note below and merge
        // them. Only when there is nothing selected — a selection wins.
        if self.selected_range.is_empty() {
            range.end = note::clamp_within_note_forward(
                self.note.text(),
                &self.index,
                &self.fences,
                caret,
                range.end,
            );
        }
        self.delete_range_or_selection(range, window, cx);
    }

    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // To the start of the *visual* row, for the same reason ⌘← moves there:
        // on a wrapped paragraph the logical start is off the top of the
        // screen, and one keystroke taking four hundred characters you cannot
        // see is not what this asks for.
        let caret = self.cursor_offset();
        let row_start = rows::row_edge(
            self.note.text(),
            &self.index,
            &self.line_layouts,
            self.caret_state(),
            false,
        )
        .offset;
        let range = if row_start < caret {
            row_start..caret
        } else {
            selection::delete_to_line_start_range(self.note.text(), caret)
        };
        self.delete_range_or_selection(range, window, cx);
    }

    /// ⌃K. Delete from the caret to the end of its visual row — the emacs kill
    /// every Cocoa text field carries. There is no kill ring; it just deletes.
    fn kill_to_end(&mut self, _: &KillToEnd, window: &mut Window, cx: &mut Context<Self>) {
        let caret = self.cursor_offset();
        // To the end of the *visual* row, the mirror of ⌘⌫'s row start: on a
        // wrapped paragraph the logical line end is off the bottom of the
        // screen, and one keystroke taking text you cannot see is not the ask.
        let row_end = rows::row_edge(
            self.note.text(),
            &self.index,
            &self.line_layouts,
            self.caret_state(),
            true,
        )
        .offset;
        let range = if row_end > caret {
            caret..row_end
        } else {
            // Already at the row's end: take the line break so ⌃K on an empty
            // line joins the one below — but never across a `---`, which would
            // silently merge two notes.
            let end = note::clamp_within_note_forward(
                self.note.text(),
                &self.index,
                &self.fences,
                caret,
                self.next_boundary(caret),
            );
            caret..end
        };
        // Remember what we killed so ⌃Y can yank it back — from whatever
        // `delete_range_or_selection` will actually remove: the selection when
        // there is one, otherwise the row-end range computed above.
        let killed = if !self.selected_range.is_empty() {
            self.selected_range.clone()
        } else {
            range.clone()
        };
        if !killed.is_empty() {
            self.kill_ring = self.note.text()[killed].to_string();
        }
        self.delete_range_or_selection(range, window, cx);
    }

    /// ⌃Y. Yank the last ⌃K kill in at the caret (replacing any selection), the
    /// partner to ⌃K that a bare kill without it leaves stranded.
    fn yank(&mut self, _: &Yank, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused || self.kill_ring.is_empty() {
            return;
        }
        let text = self.kill_ring.clone();
        self.history.break_group();
        self.replace_text_in_range(None, &text, window, cx);
        self.history.break_group();
    }

    /// ⌃O. Open a line: insert a newline at the caret but leave the caret before
    /// it, so the line splits without the caret following the break down.
    fn open_line(&mut self, _: &OpenLine, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused {
            return;
        }
        let at = self.selected_range.start.min(self.selected_range.end);
        self.replace_text_in_range(None, "\n", window, cx);
        self.move_to(at, cx);
    }

    /// ⌃L. Centre the caret's line in the viewport — the "recenter" every emacs
    /// and many macOS fields carry, for pulling the line you are on to the middle.
    fn center_caret(&mut self, _: &CenterCaret, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.index.line_at(self.cursor_offset());
        let last = self.index.line_count().saturating_sub(1);
        // Reveal a symmetric window of half-a-screen either side, which lands the
        // caret's line in the middle. Both reveals are needed: one pins the top
        // edge, the other the bottom.
        let half = (self.visible_lines / 2).max(1);
        self.list_state
            .scroll_to_reveal_item((line + half).min(last));
        self.list_state.scroll_to_reveal_item(line.saturating_sub(half));
        cx.notify();
    }

    /// ⌃T. Swap the two characters around the caret, the transpose Cocoa fields
    /// have. The pure logic lives in [`selection::transpose`]; here it is only
    /// mapped from line-local to document offsets.
    fn transpose(&mut self, _: &Transpose, window: &mut Window, cx: &mut Context<Self>) {
        // A selection means something else is in play; Cocoa's ⌃T no-ops then.
        if !self.selected_range.is_empty() {
            return;
        }
        let caret = self.cursor_offset();
        let line = self.index.line_at(caret);
        let Some((start, end)) = self.index.line_range(line) else {
            return;
        };
        let Some((range, replacement)) =
            selection::transpose(&self.note.text()[start..end], caret - start)
        else {
            return;
        };
        // The replacement is exactly as long as the range, so replacing leaves
        // the caret at `start + range.end` — where a mid-line ⌃T should end up,
        // and unchanged for the end-of-line swap.
        let utf16 = self.range_to_utf16(&(start + range.start..start + range.end));
        self.replace_text_in_range(Some(utf16), &replacement, window, cx);
    }

    /// ⌥⌘↑ / ⌥⌘↓ — swap the line, or the selected lines, with the one above or
    /// below. One edit, so one ⌘Z takes it back, and the selection travels with
    /// the text so the command can be held down.
    fn move_line_up(&mut self, _: &MoveLineUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(false, window, cx);
    }

    fn move_line_down(&mut self, _: &MoveLineDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(true, window, cx);
    }

    fn move_lines(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused || self.modal_open() {
            return;
        }
        let (first, last) = self.selected_lines();
        // A rule is a note boundary drawn as a hairline, and moving one moves
        // the partition rather than the text — the first line of the note below
        // quietly becomes the last line of the note above, with a hairline
        // shifting by a row as the only sign. ⌘X refuses on a rule for the same
        // reason.
        if (first..=last).any(|l| self.is_rule_line(l)) {
            return;
        }
        let last_line = self.index.line_count().saturating_sub(1);
        let neighbour = match if down { last.checked_add(1) } else { first.checked_sub(1) } {
            Some(line) if line <= last_line => line,
            // Already at the top or the bottom of the document.
            _ => return,
        };
        let (block_start, block_end) = (
            self.index.line_start(first),
            self.index.line_end(last),
        );
        let (other_start, other_end) = (
            self.index.line_start(neighbour),
            self.index.line_end(neighbour),
        );
        let text = self.note.text();
        let moved = text[block_start..block_end].to_string();
        let other = text[other_start..other_end].to_string();
        let (start, end, replacement) = if down {
            (block_start, other_end, format!("{other}\n{moved}"))
        } else {
            (other_start, block_end, format!("{moved}\n{other}"))
        };
        let removed = text[start..end].to_string();
        if removed == replacement {
            // Two identical neighbours — a pair of blank lines, most often. The
            // text does not change, but the caret still travels, or holding the
            // chord stops dead at every paragraph gap.
            // Both directions land at the head of the neighbour's line, which
            // is what the column is measured from.
            let landed = other_start;
            let column = self.cursor_offset().saturating_sub(block_start);
            let at = (landed + column).min(self.index.line_end(neighbour));
            self.move_to(at, cx);
            return;
        }
        // Where the moved run lands, so the selection follows it.
        let landed = if down {
            start + other.len() + 1
        } else {
            start
        };
        let had_selection = !self.selected_range.is_empty();

        self.history.break_group();
        let before = (self.selected_range.start, self.selected_range.end);
        self.note.replace_range(start, end, &replacement);
        let after = if had_selection {
            (landed, landed + moved.len())
        } else {
            // Keep the caret's column, measured from the line it was on.
            let column = before.0.saturating_sub(block_start);
            let at = (landed + column).min(landed + moved.len());
            (at, at)
        };
        self.selected_range = after.0..after.1;
        self.selection_reversed = false;
        self.reset_row_motion();
        self.marked_range = None;
        self.history.record(
            Edit {
                start,
                removed: removed.clone(),
                inserted: replacement.clone(),
                before,
                after,
            },
            Instant::now(),
        );
        self.history.break_group();
        self.after_edit(start, &removed, &replacement);
        self.follow_caret();
        let _ = window;
        cx.notify();
    }

    /// ⌘D — a second copy of the line, or of the selected lines, underneath.
    fn duplicate_lines(&mut self, _: &DuplicateLines, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused || self.modal_open() {
            return;
        }
        let (first, last) = self.selected_lines();
        // Copying a rule would conjure an empty note between two real ones.
        if (first..=last).any(|l| self.is_rule_line(l)) {
            return;
        }
        let end = self.index.line_end(last);
        let copied = self.note.text()[self.index.line_start(first)..end].to_string();
        let insertion = format!("\n{copied}");
        let had_selection = !self.selected_range.is_empty();
        self.history.break_group();
        let range = self.range_to_utf16(&(end..end));
        self.replace_text_in_range(Some(range), &insertion, window, cx);
        self.history.break_group();
        // The copy is left selected when the original was, so a second ⌘D
        // duplicates the same lines again rather than only the last of them.
        if had_selection {
            self.set_selection(end + 1, end + insertion.len());
        }
        cx.notify();
    }

    /// Whether `line` is one of the rules the document is divided by.
    fn is_rule_line(&self, line: usize) -> bool {
        note::is_boundary_line(
            self.note.text(),
            &self.index,
            line,
            self.fences.in_closed_fence(line),
        )
    }

    /// The line range the selection touches, as whole lines.
    fn selected_lines(&self) -> (usize, usize) {
        let (from, to) = (self.selected_range.start, self.selected_range.end);
        let first = self.index.line_at(from);
        let mut last = self.index.line_at(to);
        // A selection ending exactly at a line's start has not reached into it.
        if last > first && self.index.line_start(last) == to {
            last -= 1;
        }
        (first, last)
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word(self.cursor_offset()), cx);
        self.set_horizontal_affinity();
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word(self.cursor_offset()), cx);
        self.set_horizontal_affinity();
    }

    // ── Find ─────────────────────────────────────────────────────────────────

    /// ⌘F. The query it starts with, in order of what you most likely meant:
    /// the text you have selected, then the last thing you searched for.
    /// Pressing it again with the bar already open clears the query, which is
    /// how you start a different search without holding backspace.
    fn open_find(&mut self, _: &OpenFind, _: &mut Window, cx: &mut Context<Self>) {
        // With the bar already open, keep the query and offer it (select-all),
        // the way pressing ⌘F again in every macOS find field does — the next
        // keystroke replaces it, ⏎ keeps it. It used to wipe the query, which
        // meant a second ⌘F silently lost the search you were in.
        let query = if let Some(find) = self.find.as_ref() {
            find.query().to_string()
        } else {
            // Opening fresh: remember where the caret was so Escape can put it
            // back. A re-⌘F while the bar is open keeps the original mark.
            self.find_return = Some((
                self.selected_range.start,
                self.selected_range.end,
                self.selection_reversed,
            ));
            self.selected_text_safe()
                .filter(|text| !text.contains('\n') && !text.trim().is_empty())
                .unwrap_or_else(|| self.last_query.clone())
        };

        // Anchor at the *start* of the selection: anchoring at the caret, which
        // sits at its end, skips the very word you asked to search for.
        let caret = self.selected_range.start;
        // Opening searches now rather than through the debounce: the bar is
        // meant to land already showing the match, and a stale timer from a
        // previous session must not fire over this fresh query.
        self.find_search_gen = self.find_search_gen.wrapping_add(1);
        // A synchronous open leaves nothing pending; without this a reopen inside
        // the debounce window left `find_search_pending` set, so the first ⏎/⌘G
        // re-ran the search instead of stepping to the next match. `set_query`
        // below searches the whole document, so the tick owes nothing either.
        self.find_search_pending = false;
        self.find_dirty_since = None;
        let had_query = !query.is_empty();
        // ⌘F always takes the keyboard, whether opening the bar or refocusing one
        // a body click had blurred.
        self.find_focused = true;
        self.find.get_or_insert_with(Find::new);
        self.sync_find_scope();
        let text = self.note.text();
        let find = self.find.as_mut().expect("just inserted");
        find.set_query(text, query, caret);
        // A Mac find field opens with its text selected, so the first keystroke
        // replaces it and ⏎ keeps it. The whole query is the selection.
        if had_query {
            find.offer();
        }

        // Opening find puts the lights away; the strip over their gutter brings
        // them back when the pointer goes looking, the same as it does with no
        // bar up.
        self.set_window_buttons_visible(false, cx);
        self.update_match_rail();

        // `set_query` has already searched; going through `run_query` would
        // search the whole document a second time for the same answer.
        if let Some(hit) = self.find.as_ref().and_then(Find::current) {
            self.set_selection(hit.start, hit.end);
            self.reveal_caret_with_context();
        }
        cx.notify();
    }

    // ── List editing ─────────────────────────────────────────────────────────

    fn indent(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        // Indent is a Note menu item too, so its key equivalent bypasses the key
        // context; refuse while the find bar holds the keyboard, or the mid-line
        // path would insert spaces into the query. (Tab itself is Note-context
        // only, so this only guards the menu.)
        if self.find_focused {
            return;
        }
        // Across a block, blank lines are gaps between items and keep their
        // margin.
        if !self.selected_range.is_empty() {
            self.transform_lines(lists::indent_in_block, window, cx);
            return;
        }
        // On one line, Tab still indents when the caret sits in the line's
        // leading whitespace (or at the first character, or on a blank line) —
        // the outline gesture, including starting a nested item. In the middle of
        // the text, Tab instead inserts a level's worth of whitespace at the
        // caret, so it is not stuck only ever re-indenting the whole line and
        // there is a way to put a tab-stop in mid-line.
        let caret = self.cursor_offset();
        let line = self.index.line_at(caret);
        let in_indent = self.index.line_range(line).is_some_and(|(start, end)| {
            let text = self.note.text();
            let content = start + text[start..end].find(|c: char| !c.is_whitespace()).unwrap_or(end - start);
            caret <= content
        });
        if in_indent {
            self.transform_lines(lists::indent, window, cx);
        } else {
            self.replace_text_in_range(None, "  ", window, cx);
        }
    }

    fn outdent(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(lists::outdent, window, cx);
    }

    fn toggle_task(&mut self, _: &ToggleTask, window: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(lists::toggle_task, window, cx);
    }

    fn toggle_done(&mut self, _: &ToggleDone, window: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(lists::toggle_done, window, cx);
    }

    /// Rewrite every line the selection touches, and keep the selection over
    /// them so the command can be pressed again.
    ///
    /// One edit for the whole range, so one ⌘Z takes it back however many lines
    /// it covered — indenting six items is one thing you did, not six.
    fn transform_lines(
        &mut self,
        transform: impl Fn(&str) -> String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so this command has to refuse for
        // itself while the find bar has the keyboard or the Settings panel is up.
        // A body click that blurred the bar hands editing back, so this runs again.
        if self.find_focused || self.modal_open() {
            return;
        }
        let (from, to) = (self.selected_range.start, self.selected_range.end);
        let first = self.index.line_at(from);
        let mut last = self.index.line_at(to);
        // A selection that ends exactly where a line begins has not reached
        // into that line.
        if last > first && self.index.line_start(last) == to {
            last -= 1;
        }

        let (start, _) = self.index.line_range(first).unwrap_or((from, from));
        let (_, end) = self.index.line_range(last).unwrap_or((to, to));
        let mut rewritten = String::new();
        for line in first..=last {
            if line > first {
                rewritten.push('\n');
            }
            let (a, b) = self.index.line_range(line).unwrap_or((0, 0));
            rewritten.push_str(&transform(&self.note.text()[a..b]));
        }
        if rewritten == self.note.text()[start..end] {
            return;
        }

        let had_selection = !self.selected_range.is_empty();
        // The caret should stay on the character it was on, not follow the
        // marker that grew in front of it. Every transform here changes the
        // head of the line, so the character it sat on moved by the difference
        // in length.
        let caret_line = self.index.line_at(self.cursor_offset());
        let caret_within = self.cursor_offset() - self.index.line_start(caret_line);
        let (a, b) = self.index.line_range(caret_line).unwrap_or((0, 0));
        let grew = transform(&self.note.text()[a..b]).len() as isize - (b - a) as isize;
        let caret_within = (caret_within as isize + grew).max(0) as usize;

        let range = self.range_to_utf16(&(start..end));
        self.replace_text_in_range(Some(range), &rewritten, window, cx);

        if had_selection {
            // Reselect the lines, so the command can be pressed again.
            self.set_selection(start, start + rewritten.len());
        } else {
            let line_start = self.index.line_start(caret_line.min(self.index.line_count() - 1));
            let line_end = self.index.line_end(caret_line.min(self.index.line_count() - 1));
            let at = (line_start + caret_within).min(line_end);
            self.set_selection(at, at);
        }
        cx.notify();
    }

    /// Re-run the current query and reveal the hit. Find is incremental: the
    /// view follows the query as it is typed, so the selection always marks
    /// where you are — which is why hits need no separate "current" colour.
    fn run_query(&mut self, cx: &mut Context<Self>) {
        self.sync_find_scope();
        // This is the authoritative scan, so any debounce timer still in flight
        // is now stale: bumping the generation stops it re-running over this,
        // and the tick loop is owed nothing either. Leaving that debt set meant
        // a second whole-document scan a moment later, which also re-anchored
        // the current match to the caret — so the count crept forward by one
        // with nothing on screen moving, and the next ⌘G skipped a hit.
        self.find_search_gen = self.find_search_gen.wrapping_add(1);
        self.find_search_pending = false;
        self.find_dirty_since = None;
        let caret = self.cursor_offset();
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.rerun(self.note.text(), caret);
        let hit = find.current();
        self.update_match_rail();
        if let Some(hit) = hit {
            self.set_selection(hit.start, hit.end);
            self.reveal_caret_with_context();
        }
        cx.notify();
    }

    /// Redraw the edited query now, and settle the full search once the
    /// keystrokes stop. On a twenty-year note a single search is affordable but
    /// one per keystroke is not, so the count, highlights and rail follow the
    /// last edit rather than every intermediate one. The final state is always
    /// the final query: the timer scheduled by the last edit is the only one
    /// whose generation still matches when it fires.
    fn schedule_query_search(&mut self, cx: &mut Context<Self>) {
        self.find_search_gen = self.find_search_gen.wrapping_add(1);
        self.find_search_pending = true;
        let generation = self.find_search_gen;
        self.wake_caret();
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIND_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.find_search_gen == generation {
                    this.run_query(cx);
                }
            });
        })
        .detach();
    }

    /// A live search may still be waiting out its debounce when ⏎, ⌘G or Escape
    /// arrives. Those act on the matches, so the matches have to be current:
    /// flush the pending scan before stepping or closing.
    fn flush_pending_search(&mut self, cx: &mut Context<Self>) {
        // `run_query` bumps the generation, so a later flush or timer is a no-op.
        if self.find.is_some() {
            self.run_query(cx);
        }
    }

    fn toggle_match_case(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            let on = !find.match_case();
            find.set_match_case(on);
        }
        self.run_query(cx);
    }

    /// `.*` — read the query as a pattern rather than as literal text.
    fn toggle_regex(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            let on = !find.is_regex();
            find.set_regex(on);
        }
        self.run_query(cx);
    }

    /// Narrow the search to the note the caret is in, or widen it back to the
    /// whole document.
    ///
    /// A mode rather than a saved range: "this note" means the note the caret is
    /// in *now*, re-derived before every search. A range stored once goes stale
    /// the moment anything above it is edited — and a stale one is both a wrong
    /// answer and, when the bytes it names have moved, a slice through the
    /// middle of a character.
    fn toggle_note_scope(&mut self, cx: &mut Context<Self>) {
        self.find_note_only = !self.find_note_only;
        self.sync_find_scope();
        self.run_query(cx);
    }

    /// Point the search at the caret's note, if that is the mode it is in.
    /// Called before every search.
    fn sync_find_scope(&mut self) {
        let scope = self.find_note_only.then(|| self.current_note_range());
        if let Some(find) = self.find.as_mut() {
            find.set_scope(scope);
        }
    }

    /// The byte range of the note the caret sits in.
    fn current_note_range(&self) -> Range<usize> {
        // One scan for the boundaries, not two: `block_at` and
        // `separator_line_indices` each walk the whole document.
        let line = self.index.line_at(self.cursor_offset());
        let boundaries = self.note.separator_line_indices();
        let index = boundaries.iter().take_while(|&&b| b <= line).count();
        let start = match index.checked_sub(1) {
            Some(previous) => self.index.line_start(boundaries[previous] + 1),
            None => 0,
        }
        .min(self.note.len());
        let end = match boundaries.get(index) {
            Some(&next) => self.index.line_start(next).saturating_sub(1),
            None => self.note.len(),
        };
        start..end.max(start)
    }

    fn toggle_whole_word(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            let on = !find.whole_word();
            find.set_whole_word(on);
        }
        self.run_query(cx);
    }

    /// Jump the current hit to a given match — what clicking a rail tick does.
    fn jump_to_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let hit = self.find.as_mut().and_then(|find| find.set_current(index));
        if let Some(hit) = hit {
            self.set_selection(hit.start, hit.end);
            self.reveal_caret_with_context();
            // The current mark moved, so the rail needs recolouring.
            self.update_match_rail();
        }
        cx.notify();
    }

    // ── The query field's caret (only while the find bar has the keyboard) ───

    /// Move the query caret, or extend its selection. None of these search: the
    /// caret moving does not change what matches, only where the next edit lands.
    fn move_query_caret(&mut self, motion: QueryMotion, extend: bool, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            match motion {
                QueryMotion::Char(forward) => find.move_char(forward, extend),
                QueryMotion::Word(forward) => find.move_word(forward, extend),
                QueryMotion::Edge(forward) => find.move_to_edge(forward, extend),
            }
        }
        self.wake_caret();
        cx.notify();
    }

    fn find_char_left(&mut self, _: &FindCharLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Char(false), false, cx);
    }

    fn find_char_right(&mut self, _: &FindCharRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Char(true), false, cx);
    }

    fn find_word_left(&mut self, _: &FindWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Word(false), false, cx);
    }

    fn find_word_right(&mut self, _: &FindWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Word(true), false, cx);
    }

    fn find_line_start(&mut self, _: &FindLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Edge(false), false, cx);
    }

    fn find_line_end(&mut self, _: &FindLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Edge(true), false, cx);
    }

    fn find_select_char_left(&mut self, _: &FindSelectCharLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Char(false), true, cx);
    }

    fn find_select_char_right(&mut self, _: &FindSelectCharRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Char(true), true, cx);
    }

    fn find_select_word_left(&mut self, _: &FindSelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Word(false), true, cx);
    }

    fn find_select_word_right(&mut self, _: &FindSelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Word(true), true, cx);
    }

    fn find_select_line_start(&mut self, _: &FindSelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Edge(false), true, cx);
    }

    fn find_select_line_end(&mut self, _: &FindSelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_query_caret(QueryMotion::Edge(true), true, cx);
    }

    /// The query field's deletions beyond ⌫: ⌦, ⌥⌫, ⌥⌦, ⌘⌫ — the same set the
    /// note has. Each edits the query and re-runs the debounced search.
    fn find_delete_forward(&mut self, _: &FindDeleteForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.delete_forward();
        }
        self.schedule_query_search(cx);
    }

    fn find_delete_word_back(&mut self, _: &FindDeleteWordBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.delete_word_backward();
        }
        self.schedule_query_search(cx);
    }

    fn find_delete_word_forward(&mut self, _: &FindDeleteWordForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.delete_word_forward();
        }
        self.schedule_query_search(cx);
    }

    fn find_delete_to_start(&mut self, _: &FindDeleteToStart, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.delete_to_start();
        }
        self.schedule_query_search(cx);
    }

    /// ⌘E — Use Selection for Find. Set the search to the current selection (or
    /// the word under the caret) and light its matches, without taking the
    /// keyboard: the bar appears blurred, so ⌘G steps through from the note.
    fn use_selection_for_find(&mut self, _: &UseSelectionForFind, _: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused {
            return;
        }
        let query = self
            .selected_text_safe()
            .filter(|s| !s.contains('\n') && !s.trim().is_empty())
            .unwrap_or_else(|| {
                let o = self.cursor_offset();
                self.note.text()[selection::word_range(self.note.text(), o)].to_string()
            });
        if query.trim().is_empty() {
            return;
        }
        self.last_query = query.clone();
        let caret = self.cursor_offset();
        let text = self.note.text();
        self.find_search_gen = self.find_search_gen.wrapping_add(1);
        self.find_search_pending = false;
        self.find_dirty_since = None;
        let find = self.find.get_or_insert_with(Find::new);
        find.set_query(text, query, caret);
        // ⌘E does not steal the keyboard; the bar shows blurred and ⌘G navigates.
        self.find_focused = false;
        self.set_window_buttons_visible(false, cx);
        self.update_match_rail();
        cx.notify();
    }

    /// ⌘J — Jump to Selection. Scroll the caret/selection back into view.
    fn jump_to_selection(&mut self, _: &JumpToSelection, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_caret_with_context();
        cx.notify();
    }

    /// ⌥⌘F — show or hide the replace row, opening the find bar if it is closed.
    fn toggle_replace(&mut self, _: &ToggleReplace, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.open_find(&OpenFind, window, cx);
        }
        self.find_focused = true;
        if let Some(find) = self.find.as_mut() {
            let on = !find.replacing();
            find.set_replacing(on);
        }
        self.wake_caret();
        cx.notify();
    }

    /// Tab — move the keyboard between the query and replacement fields.
    fn find_focus_next_field(&mut self, _: &FindFocusNextField, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.focus_next_field();
        }
        self.wake_caret();
        cx.notify();
    }

    /// Replace the current match with the replacement text, then move to the
    /// next match — the "Replace" button / ⌘⌥⏎.
    fn replace_and_find(&mut self, _: &ReplaceAndFind, _: &mut Window, cx: &mut Context<Self>) {
        self.flush_pending_search(cx);
        let Some((range, replacement)) = self
            .find
            .as_ref()
            .and_then(|f| f.current().map(|r| (r, f.replacement().to_string())))
        else {
            return;
        };
        // One replacement is one ordinary edit. It used to clone the whole
        // buffer, diff it, and rebuild the index, the fence map and every
        // measured row height — forty milliseconds and two nine-megabyte
        // allocations to change one word, so replacing occurrence by occurrence
        // through a long note ground to a halt.
        let before_sel = (self.selected_range.start, self.selected_range.end);
        let removed = self.note.text()[range.start..range.end].to_string();
        self.note.replace_range(range.start, range.end, &replacement);
        let after = range.start + replacement.len();
        self.selected_range = after..after;
        self.selection_reversed = false;
        self.marked_range = None;
        self.history.break_group();
        self.history.record(
            Edit {
                start: range.start,
                removed: removed.clone(),
                inserted: replacement.clone(),
                before: before_sel,
                after: (after, after),
            },
            Instant::now(),
        );
        self.history.break_group();
        self.after_edit(range.start, &removed, &replacement);
        // Pressing Replace asks a question about the matches — which is the next
        // one — so this is one of the few edits that searches the document
        // straight away instead of waiting out the debounce.
        self.refresh_find();
        // Land on the next match at or after where the replacement ended.
        if let Some(hit) = self.find.as_ref().and_then(Find::current) {
            self.set_selection(hit.start, hit.end);
            self.reveal_caret_with_context();
        }
        cx.notify();
    }

    /// Replace every match in one undo step — the "All" button.
    fn replace_all(&mut self, _: &ReplaceAll, _: &mut Window, cx: &mut Context<Self>) {
        self.flush_pending_search(cx);
        let Some((matches, replacement)) = self
            .find
            .as_ref()
            .filter(|f| !f.matches().is_empty())
            .map(|f| (f.matches().to_vec(), f.replacement().to_string()))
        else {
            return;
        };
        let count = matches.len();
        let before_sel = (self.selected_range.start, self.selected_range.end);
        // Apply last-to-first so each earlier match keeps its offsets, and
        // record one edit per match rather than "the run in which the old and
        // new documents differ" — which, for a query that hits the first line
        // and the last, is two copies of the whole note.
        let mut edits: Vec<Edit> = Vec::with_capacity(count);
        for m in matches.iter().rev() {
            let removed = self.note.text()[m.start..m.end].to_string();
            self.note.replace_range(m.start, m.end, &replacement);
            edits.push(Edit {
                start: m.start,
                removed,
                inserted: replacement.clone(),
                before: before_sel,
                after: (m.start + replacement.len(), m.start + replacement.len()),
            });
        }
        // The old caret offset may now fall mid-character after length-changing
        // replacements shifted the bytes; snap it back onto a char boundary.
        let caret = self.note.clamp_offset(self.selected_range.start);
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        // The first edit applied is the last match, so its `before` selection is
        // what an undo of the whole group restores; the caret afterwards is the
        // one just clamped.
        if let Some(last) = edits.last_mut() {
            last.after = (caret, caret);
        }
        self.history.record_compound(edits, Instant::now());
        self.marked_range = None;
        self.after_structural_change();
        self.refresh_find();
        self.notice = Some(Notice::remark(format!(
            "Replaced {count} occurrence{}",
            if count == 1 { "" } else { "s" }
        )));
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, _: &mut Window, cx: &mut Context<Self>) {
        // Escape is shared, most-transient first: an open context menu, then the
        // Settings panel (cancelling a recording along the way), then the find bar.
        if self.context_menu.take().is_some() {
            cx.notify();
            return;
        }
        if self.palette.take().is_some() {
            cx.notify();
            return;
        }
        if self.backups_open {
            self.backups_open = false;
            cx.notify();
            return;
        }
        if self.modal_open() {
            self.close_settings(cx);
            return;
        }
        self.close_find_bar(cx);
    }

    /// Close the find bar and, if it still holds one, honour the Escape "cancel"
    /// by putting the caret back where the search began rather than leaving it
    /// stranded on the last match. Shared by Escape/⌘. and the bar's ✕ button.
    fn close_find_bar(&mut self, cx: &mut Context<Self>) {
        let restore = self.find_return;
        self.dismiss_find();
        if let Some((start, end, reversed)) = restore {
            let (start, end) = self.note.clamp_range(start, end);
            self.selected_range = start..end;
            self.selection_reversed = reversed;
            self.reset_row_motion();
            self.follow_caret();
        }
        cx.notify();
    }

    /// Empty the query but leave the bar open, the way the clear (✕) chip in a
    /// macOS search field does. The caret is where a fresh search should start
    /// from, so the anchor comes from it.
    fn clear_query(&mut self, cx: &mut Context<Self>) {
        self.sync_find_scope();
        let caret = self.cursor_offset();
        let text = self.note.text();
        if let Some(find) = self.find.as_mut() {
            find.set_query(text, String::new(), caret);
        }
        self.run_query(cx);
    }

    /// Re-derive the marks on the scroll rail.
    ///
    /// Bucketed to the rail's own resolution: it is three pixels wide and
    /// cannot show more marks than a tall window has pixels, so the work is
    /// bounded by the rail rather than by the document.
    fn update_match_rail(&mut self) {
        self.match_rail.clear();
        let total_lines = self.index.line_count().max(1);
        let bucket_of = |offset: usize| {
            let at = self.index.line_at(offset) as f32 / total_lines as f32;
            ((at * MATCH_TICK_BUCKETS as f32) as usize).min(MATCH_TICK_BUCKETS - 1)
        };
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let matches = find.matches();
        if matches.is_empty() {
            return;
        }
        let current_bucket = find.current().map(|hit| bucket_of(hit.start));
        // The first match in each bucket is the one a click on that mark jumps
        // to — the nearest hit in that region. `usize::MAX` marks an empty
        // bucket, so the pass over the matches records only the first per bucket.
        // The buffer is kept between calls: this runs on every query keystroke
        // and every edit made with the bar open, and nine hundred words of it is
        // not worth allocating each time.
        let buckets = &mut self.rail_buckets;
        buckets.clear();
        buckets.resize(MATCH_TICK_BUCKETS, usize::MAX);
        for (i, hit) in matches.iter().enumerate() {
            let bucket = bucket_of(hit.start);
            if buckets[bucket] == usize::MAX {
                buckets[bucket] = i;
            }
        }
        self.match_rail.clear();
        self.match_rail
            .extend(buckets.iter().enumerate().filter_map(|(bucket, &match_index)| {
                (match_index != usize::MAX).then_some(RailTick {
                    at: bucket as f32 / MATCH_TICK_BUCKETS as f32,
                    match_index,
                    current: current_bucket == Some(bucket),
                })
            }));
    }

    /// Put the find bar away, keeping its query so that reopening resumes the
    /// search you were in the middle of rather than making you type it again.
    fn dismiss_find(&mut self) {
        if let Some(find) = self.find.take() {
            self.last_query = find.query().to_string();
        }
        self.find_focused = false;
        self.match_rail.clear();
        self.find_dirty_since = None;
        // No search is in flight once the bar is closed; leaving this set made
        // the next reopen's first ⏎/⌘G flush-and-hold instead of stepping.
        self.find_search_pending = false;
        // The return anchor only survives to the Escape that consumes it; any
        // other dismissal (a body click) has already placed its own caret.
        self.find_return = None;
    }

    /// Hand the keyboard back to the note without closing the find bar — macOS's
    /// non-modal find. The bar stays on screen with its query and match
    /// highlights; a click placed the caret in the note, so the next keystroke
    /// edits there. Clicking the bar or ⌘F takes the keyboard back (`focus_find`).
    fn blur_find(&mut self) {
        if self.find.is_none() {
            return;
        }
        self.find_focused = false;
        // Any half-typed IME preedit in the query has no field to live in now.
        if let Some(find) = self.find.as_mut() {
            find.unmark();
        }
        // The caret we are about to place in the body is the return point; the
        // Escape anchor no longer applies.
        self.find_return = None;
    }

    /// Give the keyboard back to the open find bar — a click on the bar, or ⌘F.
    fn focus_find(&mut self, cx: &mut Context<Self>) {
        if self.find.is_some() {
            self.find_focused = true;
            self.wake_caret();
            cx.notify();
        }
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(true, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(false, cx);
    }

    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        // ⏎/⌘G pressed before the debounce settles means the user has not yet
        // been shown the results: land on the nearest match rather than step
        // past it. Once settled, they step as usual.
        if self.find_search_pending {
            self.flush_pending_search(cx);
            return;
        }
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let hit = if forward { find.next() } else { find.previous() };
        if let Some(hit) = hit {
            self.set_selection(hit.start, hit.end);
            self.reveal_caret_with_context();
        }
        cx.notify();
    }

    /// Re-run the query after the buffer changed, so the highlights do not
    /// point at text that has moved.
    fn refresh_find(&mut self) {
        self.find_dirty_since = None;
        self.sync_find_scope();
        let caret = self.cursor_offset();
        let text = self.note.text();
        if let Some(find) = self.find.as_mut() {
            find.rerun(text, caret);
        }
        self.update_match_rail();
    }

    // ── Text size ────────────────────────────────────────────────────────────

    fn text_bigger(&mut self, _: &TextBigger, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.bigger();
        self.apply_text_size(cx);
    }

    fn text_smaller(&mut self, _: &TextSmaller, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.smaller();
        self.apply_text_size(cx);
    }

    fn text_size_reset(&mut self, _: &TextSizeReset, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.reset_text_size();
        self.apply_text_size(cx);
    }

    /// Every row's height changes with the text size, so the list's measured
    /// heights all have to go.
    fn apply_text_size(&mut self, cx: &mut Context<Self>) {
        // `reset` throws the scroll away along with the heights, so the line
        // being read has to be put back. Without this, resizing the text while
        // scrolled anywhere but the top jumps to the start of the document —
        // and `follow_caret` only rescues you if the caret is on screen, which
        // after scrolling with a trackpad it usually is not.
        let top = self.list_state.logical_scroll_top().item_ix;
        self.list_state.reset(self.index.line_count());
        self.list_state.scroll_to(ListOffset {
            item_ix: top.min(self.index.line_count().saturating_sub(1)),
            // The row it belongs to is a different height now, so the fraction
            // scrolled into it means nothing: start the line at the top.
            offset_in_item: px(0.),
        });
        if let Err(err) = self.settings.save(&settings::settings_path()) {
            eprintln!("gravitynote: could not save settings: {err}");
        }
        cx.notify();
    }

    // ── Undo / redo ──────────────────────────────────────────────────────────

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        // ⌘Z is an Edit-menu key equivalent, so it bypasses the key context: the
        // modal Settings panel must block it like every other note command.
        if self.modal_open() {
            return;
        }
        // While the bar holds the keyboard, ⌘Z undoes the query field, not the
        // note behind it. (Menu key equivalents bypass the key context, so this
        // gate stands in for it; a blurred bar hands ⌘Z back to the note.)
        if self.find_focused {
            if self.find.as_mut().is_some_and(Find::undo_query) {
                self.run_query(cx);
            }
            return;
        }
        let restored = self.history.undo(self.note.text_mut());
        self.after_history(restored, cx);
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        if self.find_focused {
            if self.find.as_mut().is_some_and(Find::redo_query) {
                self.run_query(cx);
            }
            return;
        }
        let restored = self.history.redo(self.note.text_mut());
        self.after_history(restored, cx);
    }

    /// Re-derive after undo/redo replayed spans straight into the buffer.
    ///
    /// A replay describes the one span it rewrote, so the ordinary incremental
    /// path handles it: ⌘Z after typing a word patches a couple of rows instead
    /// of rebuilding a 214,000-line index and throwing away every measured row
    /// height. Only a compound group — a note that changed places — reports no
    /// span, and only that rebuilds.
    fn after_history(&mut self, replay: Option<history::Replay>, cx: &mut Context<Self>) {
        let Some(replay) = replay else {
            return;
        };
        let (start, end) = replay.selection;
        match replay.span {
            Some((at, removed, inserted)) => self.after_edit(at, &removed, &inserted),
            None => self.after_structural_change(),
        }
        self.set_selection(start, end);
        self.marked_range = None;
        // The restored state is its own boundary: typing next must not merge
        // into the group we just moved across.
        self.history.break_group();
        self.follow_caret();
        self.save_soon(cx);
        cx.notify();
    }

    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        match self.list_continuation() {
            Some(markdown::Continuation::Next(prefix)) => {
                self.replace_text_in_range(None, &format!("\n{prefix}"), window, cx);
            }
            Some(markdown::Continuation::Clear) => {
                // Leaving the list: empty the line and stay on it, rather than
                // breaking it and writing another marker nobody asked for.
                self.replace_current_line(String::new(), window, cx);
            }
            Some(markdown::Continuation::Outdent(marker)) => {
                // An empty nested item: step out one level, staying on the line,
                // so a second Enter steps out again (and eventually clears at
                // the margin). Losing every level of structure to one press was
                // the surprise this replaces.
                self.replace_current_line(marker, window, cx);
            }
            None => self.replace_text_in_range(None, "\n", window, cx),
        }
    }

    /// Replace the caret's whole logical line with `text`, leaving the caret at
    /// the end of what was written. Shared by the two Enter-on-empty-item paths
    /// (clear the line, or step it out one level).
    fn replace_current_line(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.index.line_at(self.cursor_offset());
        let Some((start, end)) = self.index.line_range(line) else {
            return;
        };
        let range = self.range_to_utf16(&(start..end));
        self.replace_text_in_range(Some(range), &text, window, cx);
    }

    /// The list or quote the caret sits in, if Enter should carry it on.
    ///
    /// A selection is about to be replaced, so there is no single item to
    /// continue; inside a fenced code block a `- ` is code, not a bullet.
    fn list_continuation(&self) -> Option<markdown::Continuation> {
        if !self.selected_range.is_empty() {
            return None;
        }
        let caret = self.cursor_offset();
        let line = self.index.line_at(caret);
        if self.fences.is_open(line) {
            return None;
        }
        let (start, end) = self.index.line_range(line)?;
        markdown::continuation(&self.note.text()[start..end], caret - start)
    }

    /// Files dropped onto the window are inserted at the caret: a text file as
    /// its contents, anything else (or a file too large to inline) as its path.
    /// The note is plain markdown, so pasted text belongs inline, not as an
    /// attachment.
    fn drop_files(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // A drop is an edit of the note; hand the keyboard back if the find bar
        // had it, so the insertion lands in the note at the caret.
        if self.find_focused {
            self.blur_find();
        }
        // An image becomes an image: copied into the store and referenced,
        // rather than pasted in as the letters of its own path. Handled first
        // and separately, because importing reads and hashes the file and that
        // does not belong on the frame this drop arrived on.
        let (images, others): (Vec<PathBuf>, Vec<PathBuf>) = paths
            .paths()
            .iter()
            .cloned()
            .partition(|p| images::is_image_file(p));
        if !images.is_empty() {
            self.import_images(ImportSource::Files(images), window, cx);
        }
        // The rest keep the behaviour they had. Dropping a screenshot next to a
        // text file must not silently swallow the text file.
        if others.is_empty() {
            return;
        }
        // Cap what gets inlined, so dropping a giant file cannot wedge the note.
        const MAX_INLINE: u64 = 2 * 1024 * 1024;
        let mut insert = String::new();
        for path in &others {
            let small = std::fs::metadata(path).map(|m| m.len() <= MAX_INLINE).unwrap_or(false);
            let text = small
                .then(|| std::fs::read(path).ok().and_then(|b| String::from_utf8(b).ok()))
                .flatten();
            match text {
                Some(contents) => insert.push_str(&contents),
                None => insert.push_str(&path.to_string_lossy()),
            }
            insert.push('\n');
        }
        if insert.is_empty() {
            return;
        }
        self.history.break_group();
        self.replace_text_in_range(None, &insert, window, cx);
        self.history.break_group();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // An image on the pasteboard — a screenshot, most of the time — is
        // stored and referenced. Note that macOS hands over plain text in
        // preference to an image when both are offered (gpui's reader returns
        // early on `public.utf8-plain-text`), so something copied out of a
        // browser arrives as its text. Dropping the file covers that.
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let text = item.text();
        // Only reach for an image when there is no text to paste — macOS offers
        // text in preference to an image when both are present, and probing for
        // the image first meant cloning a screenshot's bytes on the main thread
        // only to throw them away.
        if text.is_none() && !self.find_focused {
            if let Some(bytes) = clipboard_image(item) {
                self.import_images(ImportSource::Bytes(bytes), window, cx);
                return;
            }
        }
        let Some(text) = text else {
            return;
        };
        // A paste is its own undo step, whatever its length — merging it with
        // the typing on either side is not what any Mac text view does.
        self.history.break_group();

        // Pasting into a focused find bar searches for what you pasted. A query
        // is one line, so a multi-line paste keeps only the first. A blurred bar
        // leaves ⌘V to the note.
        if self.find_focused {
            let line = text.lines().next().unwrap_or_default().to_string();
            if line.is_empty() {
                return;
            }
            // Insert at the caret, replacing the offered query (or any selection).
            if let Some(find) = self.find.as_mut() {
                find.insert(&line);
            }
            self.schedule_query_search(cx);
            return;
        }
        // Normalise line endings. A `\r\n` would leave a stray `\r` at the end
        // of every line, saved and carried forever; a lone `\r` is not a line
        // break to the note model at all, so a document pasted from an older
        // source would arrive as one enormous line that can never be split.
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text
        };
        // A URL pasted over selected words makes a link out of them. It is the
        // most tedious keystroke sequence in a markdown editor, and the one
        // thing a paste can infer without ever guessing wrong: the selection is
        // the text, the clipboard is the target.
        let text = match self.selected_text_safe() {
            Some(selected) if is_bare_url(&text) && is_link_text(&selected) => {
                format!("[{selected}]({})", text.trim())
            }
            _ => text,
        };
        self.replace_text_in_range(None, &text, window, cx);
        self.history.break_group();
    }

    fn selected_text_safe(&self) -> Option<String> {
        if self.selected_range.is_empty() {
            return None;
        }
        let (a, b) = self
            .note
            .clamp_range(self.selected_range.start, self.selected_range.end);
        if a >= b {
            return None;
        }
        Some(self.note.text()[a..b].to_string())
    }

    /// The caret's whole line plus its trailing newline — what ⌘C/⌘X act on when
    /// nothing is selected, so a paste drops in a full line the way it does in
    /// every code editor. `None` when the caret sits on a `---` separator line:
    /// cutting the invisible rule would merge two notes with no visible cause,
    /// which the delete paths already refuse to do.
    fn current_line_range(&self) -> Option<Range<usize>> {
        let line = self.index.line_at(self.cursor_offset());
        if note::is_boundary_line(self.note.text(), &self.index, line, self.fences.in_closed_fence(line)) {
            return None;
        }
        let start = self.index.line_start(line);
        let end = if line + 1 < self.index.line_count() {
            self.index.line_start(line + 1)
        } else {
            self.index.len()
        };
        Some(start..end)
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so while the find bar has the
        // keyboard ⌘C copies the query, not the note behind it. With nothing
        // selected it copies the whole query, the way ⌘C on an unselected line
        // copies the line. Only while the bar holds the keyboard — a blurred bar
        // leaves ⌘C to the note.
        if let Some(find) = self.find.as_ref().filter(|_| self.find_focused) {
            let text = if find.has_selection() {
                find.selected_text()
            } else {
                find.query()
            };
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            }
            return;
        }
        let text = match self.selected_text_safe() {
            Some(s) => s,
            None => match self.current_line_range() {
                Some(range) => self.note.text()[range].to_string(),
                None => return,
            },
        };
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        // A menu item's key equivalent is dispatched straight to the action,
        // without consulting the key context, so while the find bar owns the
        // keyboard ⌘X cuts the query's selection, mirroring ⌘C copying it. A
        // blurred bar leaves ⌘X to the note.
        if self.find_focused {
            let cut = self.find.as_mut().and_then(|find| {
                find.has_selection().then(|| {
                    let text = find.selected_text().to_string();
                    find.delete_backward();
                    text
                })
            });
            if let Some(text) = cut {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.schedule_query_search(cx);
            }
            return;
        }
        if let Some(s) = self.selected_text_safe() {
            cx.write_to_clipboard(ClipboardItem::new_string(s));
            self.replace_text_in_range(None, "", window, cx);
            return;
        }
        // No selection: cut the whole caret line, newline and all — but not a
        // `---` separator, which `current_line_range` refuses so ⌘X can't merge
        // two notes with nothing on screen to show why.
        let Some(range) = self.current_line_range() else {
            return;
        };
        if range.is_empty() {
            return;
        }
        let text = self.note.text()[range.clone()].to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let utf16 = self.range_to_utf16(&range);
        self.replace_text_in_range(Some(utf16), "", window, cx);
    }

    // ── Mouse ────────────────────────────────────────────────────────────────

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A panel's scrim does not stop the press reaching here, and one of the
        // things this does now is edit the note — a click on the dimmed area
        // over a task box would have ticked it while Settings was open.
        if event.button != MouseButton::Left || self.modal_open() {
            return;
        }
        // ⌘-click on a link opens it in the browser, the way Xcode, VS Code and
        // Terminal do. A plain click still places the caret, so the URL text stays
        // editable — the file is raw markdown and the markup is yours to change.
        if event.modifiers.platform {
            let idx = self.index_for_mouse_position(event.position);
            if let Some(url) = self.link_at_offset(idx) {
                self.open_link(&url, cx);
                cx.stop_propagation();
                return;
            }
        }
        // A click in the note body is a request to edit it. While the find bar
        // holds the keyboard it owns the single input path, so a click used to
        // move a caret you then could not type into. A body click now blurs the
        // bar — it stays on screen with its query and match highlights, but the
        // keyboard returns to the note so the next keystroke lands where you
        // clicked. Only the ✕ (or Escape) closes the bar; clicking it, or ⌘F,
        // takes the keyboard back. The bar's own mouse-down stops propagation, so
        // clicks on it never reach here.
        if self.find_focused {
            self.blur_find();
        }
        // A body click also dismisses the context menu, if one is open.
        self.context_menu = None;
        let idx = self.index_for_mouse_position(event.position);
        // A click on a task's own box ticks it. It is the gesture everyone tries
        // first in a note full of tasks, and ⌘⏎ was the only way to do it.
        if event.click_count == 1 && !event.modifiers.shift && self.toggle_task_box_at(idx, cx) {
            // A press on the box still starts a drag, so dragging off it selects
            // the way a press anywhere else does.
            self.drag_granularity = DragGranularity::Char;
            self.drag_anchor = idx..idx;
            self.is_selecting = true;
            return;
        }
        let affinity = self.affinity_for_position(idx, event.position);
        if event.modifiers.shift {
            self.drag_granularity = DragGranularity::Char;
            self.is_selecting = true;
            self.select_to(idx, cx);
            self.caret.affinity = affinity;
            return;
        }
        // Double-click selects the word, triple-click the paragraph — and then,
        // if you keep the button down and drag, the selection extends by whole
        // words or paragraphs anchored on that first unit, the way macOS does.
        // The granularity captured here is what the drag snaps each extension to.
        //
        // Neither path scrolls the view to the click: you are already looking at
        // where you clicked, and revealing the line shifted a partially-visible
        // edge row fully into view — so a click near the top or bottom jumped the
        // whole document under the pointer.
        match selection::click_range(self.note.text(), idx, event.click_count) {
            Some(range) => {
                self.drag_granularity = if event.click_count == 2 {
                    DragGranularity::Word
                } else {
                    DragGranularity::Paragraph
                };
                self.drag_anchor = range.clone();
                // Keep selecting: a drag after the multi-click now extends by the
                // captured unit rather than being ignored.
                self.is_selecting = true;
                self.history.break_group();
                self.set_selection(range.start, range.end);
                cx.notify();
            }
            None => {
                self.drag_granularity = DragGranularity::Char;
                self.is_selecting = true;
                self.place_caret(idx, cx);
                self.caret.affinity = affinity;
            }
        }
    }

    /// Tick the task box under `offset`, if that is what was clicked. Returns
    /// whether it was.
    ///
    /// The box is `[ ]` or `[x]` at the head of a list item, which the
    /// highlighter already finds — so this asks the one line the click landed on
    /// and does nothing anywhere else.
    fn toggle_task_box_at(&mut self, offset: usize, cx: &mut Context<Self>) -> bool {
        let line = self.index.line_at(offset);
        if self.fences.is_open(line) {
            return false;
        }
        let Some((start, end)) = self.index.line_range(line) else {
            return false;
        };
        let within = offset - start;
        let raw = &self.note.text()[start..end];
        // The span the highlighter marks includes the space after the box, and
        // the space is text: clicking it should put the caret there, not tick
        // anything. The brackets themselves are the control.
        let hit = markdown::highlight_line(raw, false).into_iter().any(|span| {
            matches!(span.style, MdStyle::TaskOpen | MdStyle::TaskDone)
                && raw[span.start..span.end]
                    .find(']')
                    .is_some_and(|close| (span.start..span.start + close + 1).contains(&within))
        });
        if !hit {
            return false;
        }
        let toggled = lists::toggle_done(raw);
        if toggled == raw {
            return false;
        }
        let before = (self.selected_range.start, self.selected_range.end);
        let removed = raw.to_string();
        self.note.replace_range(start, end, &toggled);
        // The caret stays where the click put it, clamped to the line — ticking
        // a box is not a reason to move it, and the marker's width can change.
        let caret = (start + within).min(start + toggled.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.reset_row_motion();
        // The marker can change length, so any composition in flight no longer
        // names the text it was composing.
        self.marked_range = None;
        self.history.break_group();
        self.history.record(
            Edit {
                start,
                removed: removed.clone(),
                inserted: toggled.clone(),
                before,
                after: (caret, caret),
            },
            Instant::now(),
        );
        self.history.break_group();
        self.after_edit(start, &removed, &toggled);
        cx.notify();
        true
    }

    /// Open the note-body context menu at a right-click.
    ///
    /// macOS places the caret at the click first when it lands outside the
    /// selection, so Cut/Copy act on something predictable; a right-click inside
    /// the selection leaves it be. The menu itself is drawn by
    /// [`NoteApp::render_context_menu`].
    fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Right || self.modal_open() {
            return;
        }
        // Like a left click, a right click in the body hands the keyboard back
        // from the find bar (without closing it) so the menu's commands act on
        // the note.
        if self.find_focused {
            self.blur_find();
        }
        let idx = self.index_for_mouse_position(event.position);
        let inside = !self.selected_range.is_empty()
            && self.selected_range.start <= idx
            && idx < self.selected_range.end;
        if !inside {
            let affinity = self.affinity_for_position(idx, event.position);
            self.place_caret(idx, cx);
            self.caret.affinity = affinity;
        }
        self.context_menu = Some(event.position);
        cx.notify();
    }

    /// Open the macOS Look Up panel for the selection, or the word under the
    /// caret when nothing is selected — the right-click menu's "Look Up".
    ///
    /// `dict://` is the scheme the system's own Look Up uses; handing it to
    /// `open` shows the same panel ⌃⌘D would, without a private API.
    fn look_up(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        cx.notify();
        let term = match self.selected_text_safe() {
            Some(s) => s,
            None => {
                let o = self.cursor_offset();
                self.note.text()[selection::word_range(self.note.text(), o)].to_string()
            }
        };
        let term = term.trim();
        if term.is_empty() {
            return;
        }
        // A selection can hold spaces; the scheme wants them percent-encoded.
        open_with_finder(&format!("dict://{}", term.replace(' ', "%20")));
    }

    /// Place the caret at `offset` without scrolling it into view — the caret
    /// placement a mouse click wants. `move_to` reveals the caret, which is right
    /// for keyboard motion but wrong for a click, where the pointer already chose
    /// what is on screen.
    fn place_caret(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.note.clamp_offset(offset);
        self.reset_row_motion();
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.wake_caret();
        self.history.break_group();
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = false;
        self.drag_position = None;
        // A resize is committed here, not while the pointer moves: one gesture
        // is one edit and one undo entry.
        if self.image_drag.is_some() {
            self.end_image_drag(cx);
            cx.notify();
        }
        if self.scroll_drag.take().is_some() {
            self.list_state.scrollbar_drag_ended();
            cx.notify();
        }
    }

    /// Extend a drag, whether it grabbed the scroll thumb or is selecting text.
    /// Driven from a window-level listener rather than the element's own, so it
    /// keeps working once the pointer leaves the text or the thin rail.
    fn on_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.image_drag.is_some() {
            self.drag_image(event.position, cx);
            return;
        }
        if self.scroll_drag.is_some() {
            self.drag_scroll_thumb(event.position, cx);
            return;
        }
        if !self.is_selecting {
            return;
        }
        self.drag_position = Some(event.position);
        self.extend_selection_granular(self.index_for_mouse_position(event.position));
        self.caret.affinity = self.affinity_for_position(self.cursor_offset(), event.position);
        self.start_drag_scroll(cx);
        cx.notify();
    }

    /// Extend a drag-selection to `offset`, snapped to the granularity the drag
    /// began at: whole words after a double-click, whole paragraphs after a
    /// triple-click, single characters otherwise. The originally selected unit
    /// stays covered — [`selection::union_toward`] is the arithmetic.
    fn extend_selection_granular(&mut self, offset: usize) {
        let unit = match self.drag_granularity {
            DragGranularity::Char => return self.extend_selection(offset),
            DragGranularity::Word => selection::word_range(self.note.text(), offset),
            DragGranularity::Paragraph => selection::paragraph_range(self.note.text(), offset),
        };
        let (range, reversed) = selection::union_toward(self.drag_anchor.clone(), unit);
        let (start, end) = self.note.clamp_range(range.start, range.end);
        self.selected_range = start..end;
        self.selection_reversed = reversed;
        self.reset_row_motion();
        self.wake_caret();
        self.history.break_group();
    }

    /// How fast a drag that has left the viewport should scroll, and which way.
    ///
    /// Selecting past the edge of the window is how you take in more than a
    /// screenful, so the text has to come to the pointer. Speed rises with the
    /// distance dragged beyond the edge, which is what makes both a careful
    /// line-by-line crawl and a run to the end of the note reachable.
    fn drag_scroll_speed(&self) -> Option<Pixels> {
        let (viewport, pointer) = (self.viewport?, self.drag_position?);
        let past = if pointer.y < viewport.top() {
            pointer.y - viewport.top()
        } else if pointer.y > viewport.bottom() {
            pointer.y - viewport.bottom()
        } else {
            return None;
        };
        // Quadratic in the distance past the edge, with no reach clamp: a small
        // overshoot crawls, a large one races. The floor keeps leaving the
        // viewport at all always moving rather than looking stuck.
        let dist = f32::from(past.abs());
        let speed = px((DRAG_SCROLL_GAIN * dist * dist).max(f32::from(DRAG_SCROLL_FLOOR)));
        Some(if past < Pixels::ZERO { speed * -1. } else { speed })
    }

    /// Keep scrolling and extending the selection while the pointer sits
    /// outside the viewport, whether or not it keeps moving.
    fn start_drag_scroll(&mut self, cx: &mut Context<Self>) {
        if self.drag_scrolling || self.drag_scroll_speed().is_none() {
            return;
        }
        self.drag_scrolling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAG_SCROLL_TICK).await;
                let running = this.update(cx, |this, cx| {
                    let Some(speed) = this
                        .is_selecting
                        .then(|| this.drag_scroll_speed())
                        .flatten()
                    else {
                        this.drag_scrolling = false;
                        return false;
                    };
                    this.list_state.scroll_by(speed);
                    // The rows under the pointer have moved, so re-reading the
                    // position is what actually extends the selection — snapped
                    // to the drag's granularity, the same as an ordinary move.
                    if let Some(pointer) = this.drag_position {
                        this.extend_selection_granular(this.index_for_mouse_position(pointer));
                    }
                    cx.notify();
                    true
                });
                match running {
                    Ok(true) => continue,
                    _ => break,
                }
            }
        })
        .detach();
    }

    /// The scroll thumb's size and position, plus the mapping the drag needs.
    ///
    /// A pixel-exact thumb would divide the scrolled pixel offset by the content
    /// height. GPUI's list measures only the visible rows (plus overdraw), so its
    /// `max_offset_for_scrollbar` / `scroll_px_offset_for_scrollbar` count every
    /// unmeasured row as zero-height — on a 214,000-line note that undercounts
    /// the document by orders of magnitude, and measuring all of it to fix that
    /// is exactly the O(document) work the app forbids. So the thumb is sized and
    /// placed in *lines*, but anchored so it reaches the bottom exactly when the
    /// last line does: the first-visible line runs `0..=total-visible`, and that
    /// range maps onto the track's `0..=1-height`. The old code divided the
    /// first-visible line by `total` and clamped, which let the thumb hit bottom
    /// while lines still sat below the fold. A sub-line offset from the measured
    /// first row keeps the motion smooth between line steps.
    fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        let total = self.index.line_count();
        let visible = self.visible_lines.max(1);
        if total <= visible {
            return None;
        }
        let height = (visible as f32 / total as f32).clamp(0.03, 1.0);
        let max_top_line = (total as f32 - visible as f32).max(1.);
        let top_offset = self.list_state.logical_scroll_top();
        // How far into the first visible row we have scrolled, as a fraction of
        // it, so the thumb glides rather than snapping a line at a time.
        let frac = self
            .line_layouts
            .get(&top_offset.item_ix)
            .map(|row| row.geometry.bounds().size.height)
            .filter(|h| *h > px(0.))
            .map(|h| f32::from(top_offset.offset_in_item / h).clamp(0., 1.))
            .unwrap_or(0.);
        let first_line = top_offset.item_ix as f32 + frac;
        let top = (first_line / max_top_line).clamp(0., 1.) * (1. - height);
        Some(ScrollMetrics {
            top,
            height,
            max_top_line,
        })
    }

    /// Drag the scroll thumb: map the pointer's position on the rail onto a
    /// scroll position, keeping the point first grabbed under the cursor.
    fn drag_scroll_thumb(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(rail) = self.scroll_rail else { return };
        let Some(grab) = self.scroll_drag else { return };
        let Some(m) = self.scroll_metrics() else { return };
        let usable = rail.size.height * (1. - m.height);
        if usable <= px(0.) {
            return;
        }
        // Where the thumb's top now wants to be, clamped to the track.
        let thumb_top = (pointer.y - grab - rail.top()).clamp(px(0.), usable);
        let scroll_frac = f32::from(thumb_top / usable).clamp(0., 1.);
        let item_ix = (scroll_frac * m.max_top_line).round() as usize;
        self.list_state.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    /// A click on the rail track, above or below the thumb, pages the view one
    /// screenful toward the click — the other half of a real scrollbar.
    fn on_scroll_rail_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(rail) = self.scroll_rail else { return };
        let Some(m) = self.scroll_metrics() else { return };
        let frac = f32::from((event.position.y - rail.top()) / rail.size.height).clamp(0., 1.);
        // A near-full screenful, so paging keeps a line of overlap for context.
        let page = self
            .viewport
            .map(|v| v.size.height)
            .unwrap_or(rail.size.height)
            * 0.9;
        if frac < m.top {
            self.list_state.scroll_by(page * -1.);
        } else if frac > m.top + m.height {
            self.list_state.scroll_by(page);
        } else {
            return;
        }
        cx.notify();
    }

    /// The Settings panel: a card laid over the note.
    ///
    /// Everything a person can change lives here — the text size, the global
    /// show/hide chord (recordable to any chord, or turned off), and whether the
    /// app opens at login. A real surface with real controls, in place of the
    /// old submenu of fake ticked labels.
    /// The command palette: a field, and the commands whose names match it.
    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let base = self.settings.text_size;
        let cs = chrome_text(base);
        let query = self.palette.as_ref().map(|p| p.query.clone()).unwrap_or_default();
        let selected = self.palette.as_ref().map(|p| p.selected).unwrap_or(0);
        let matches = self.palette_matches();
        let mut caret: Hsla = rgb(theme().cursor).into();
        caret.a = self.caret_alpha;

        // A window of rows around the selection rather than all thirty-one:
        // ↓ used to walk the highlight below the fold, and there is no
        // scroll-into-view to call.
        const VISIBLE: usize = 9;
        let first = selected.saturating_sub(VISIBLE - 1).min(
            matches.len().saturating_sub(VISIBLE),
        );
        let rows: Vec<AnyElement> = matches
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE)
            .map(|(i, command)| {
                let on = i == selected;
                div()
                    .id(("command", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .px(px(cs * 0.6))
                    .py(px(cs * 0.35))
                    .rounded(px(cs * 0.4))
                    .when(on, |d| d.bg(rgb(theme().selection)))
                    .hover(|s| s.bg(rgb(theme().find_field_bg)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .text_size(px(cs))
                    .text_color(rgb(theme().fg))
                    .child(command.name)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(cs * 0.9))
                            .text_color(rgb(theme().fg_dim))
                            .child(command.key),
                    )
                    .on_click(cx.listener(move |this, _e: &ClickEvent, window, cx| {
                        let run = this.palette_matches().get(i).map(|c| c.run);
                        this.palette = None;
                        cx.notify();
                        if let Some(run) = run {
                            run(this, window, cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .id("palette-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .flex_col()
            .items_center()
            .bg(scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.palette = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("palette-card")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // High on the window rather than centred: the eye is already
                    // at the top after ⌘⇧P, and the list grows downward.
                    .mt(px(90.))
                    .w(px(440.))
                    .max_w(relative(0.94))
                    .max_h(relative(0.7))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .bg(rgb(theme().bg))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(theme().rule))
                    .shadow_lg()
                    .child(
                        div()
                            .w_full()
                            .h(px(cs * 2.2))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px(px(cs * 0.7))
                            .rounded_full()
                            .bg(rgb(theme().find_field_bg))
                            .border_1()
                            .border_color(rgb(theme().find_field_border))
                            .text_size(px(cs))
                            .child(
                                svg()
                                    .path("icons/search.svg")
                                    .flex_none()
                                    .size(px(cs * 0.95))
                                    .text_color(rgb(theme().fg_dim)),
                            )
                            .when(query.is_empty(), |d| {
                                d.child(div().flex_none().w(CARET_WIDTH).h(px(cs * 1.3)).bg(caret))
                                    .child(
                                        div()
                                            .text_color(rgb(theme().placeholder))
                                            .child("Run a command"),
                                    )
                            })
                            .when(!query.is_empty(), |d| {
                                d.child(div().text_color(rgb(theme().fg)).child(SharedString::from(query.clone())))
                                    .child(div().flex_none().w(CARET_WIDTH).h(px(cs * 1.3)).bg(caret))
                            }),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w_full()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .when(matches.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(cs * 0.9))
                                .text_color(rgb(theme().placeholder))
                                .child("No command by that name."),
                        )
                    })
                    .when(matches.len() > VISIBLE, |d| {
                        d.child(
                            div()
                                .text_size(px(cs * 0.85))
                                .text_color(rgb(theme().placeholder))
                                .child(SharedString::from(format!(
                                    "{} of {} — keep typing",
                                    (first + VISIBLE).min(matches.len()),
                                    matches.len()
                                ))),
                        )
                    }),
            )
    }

    /// The backups, as a list you can read and choose from.
    ///
    /// Forty-odd files sat in `backups/` and the app never mentioned them
    /// except in a notice. Each row says when it was taken, how big it is, and
    /// what it opens with, which is enough to recognise the afternoon you are
    /// looking for.
    fn render_backups_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let base = self.settings.text_size;
        let cs = chrome_text(base);

        let rows: Vec<AnyElement> = self
            .backups
            .iter()
            .enumerate()
            .map(|(i, backup)| {
                let path = backup.path.clone();
                let when: SharedString = backup.at.format("%a %-d %b, %H:%M").to_string().into();
                let size: SharedString = human_bytes(backup.bytes).into();
                let preview: SharedString = backup.preview.clone().into();
                div()
                    .id(("backup", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .px(px(cs * 0.5))
                    .py(px(cs * 0.4))
                    .rounded(px(cs * 0.4))
                    .hover(|s| s.bg(rgb(theme().find_field_bg)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .text_size(px(cs * 0.95))
                                    .text_color(rgb(theme().fg))
                                    .child(when)
                                    .child(div().text_color(rgb(theme().placeholder)).child(size)),
                            )
                            .child(
                                // One line, cut with an ellipsis: a long opening
                                // line must not make its row twice as tall as
                                // its neighbours.
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(cs * 0.85))
                                    .text_color(rgb(theme().fg_dim))
                                    .child(preview),
                            ),
                    )
                    .child(
                        div()
                            .id(("restore", i))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .px(px(cs * 0.7))
                            .h(px(cs * 2.0))
                            .rounded(px(cs * 0.5))
                            .bg(rgb(theme().find_field_bg))
                            .border_1()
                            .border_color(rgb(theme().find_field_border))
                            .text_size(px(cs * 0.9))
                            .text_color(rgb(theme().fg))
                            .hover(|s| s.bg(rgb(theme().btn_bg_hover)).border_color(rgb(theme().cursor)))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .child("Restore")
                            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                this.restore_backup(path.clone(), cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let empty = rows.is_empty();
        div()
            .id("backups-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.backups_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("backups-card")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(520.))
                    .max_w(relative(0.94))
                    .max_h(relative(0.8))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(rgb(theme().bg))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(theme().rule))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(cs * 1.2))
                                    .text_color(rgb(theme().fg))
                                    .child("Backups"),
                            )
                            .child(
                                div()
                                    .id("backups-close")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(cs * 1.6))
                                    .rounded_full()
                                    .text_color(rgb(theme().fg_dim))
                                    .hover(|s| s.bg(rgb(theme().find_field_bg)).text_color(rgb(theme().fg)))
                                    .cursor(gpui::CursorStyle::PointingHand)
                                    .child("✕")
                                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                        this.backups_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().h(px(1.)).w_full().bg(rgb(theme().rule)))
                    .child(
                        div()
                            .id("backups-list")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w_full()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .when(empty, |d| {
                        d.child(
                            div()
                                .text_size(px(cs * 0.9))
                                .text_color(rgb(theme().placeholder))
                                .child("No backups yet. One is taken every hour you edit."),
                        )
                    })
                    .child(div().h(px(1.)).w_full().bg(rgb(theme().rule)))
                    .child(
                        div()
                            .text_size(px(cs * 0.85))
                            .text_color(rgb(theme().placeholder))
                            .child(
                                "Restoring replaces the note — and backs up what is on \
                                 screen first, so it can be undone by restoring that.",
                            ),
                    ),
            )
    }

    fn render_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let base = self.settings.text_size;
        let cs = chrome_text(base);
        let text = px(cs);
        let recording = self.recording_shortcut;
        let shortcut_label = match &self.settings.toggle_shortcut {
            Some(hotkey) => hotkey.label(),
            None => "Off".to_string(),
        };
        let login_on = login_item::enabled();

        // A soft rounded button in the panel's own idiom.
        let chip = |label: SharedString, strong: bool| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .px(px(cs * 0.7))
                .h(px(cs * 2.0))
                .rounded(px(cs * 0.5))
                .bg(rgb(if strong { theme().btn_bg_hover } else { theme().find_field_bg }))
                .border_1()
                .border_color(rgb(theme().find_field_border))
                .text_size(px(cs * 0.95))
                .text_color(rgb(if strong { theme().btn_fg_hover } else { theme().fg }))
                .hover(|s| s.border_color(rgb(theme().cursor)))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(label)
        };

        // One labelled row: a name on the left, its controls on the right.
        let row = |name: &'static str, controls: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_4()
                .w_full()
                .child(
                    div()
                        .flex_none()
                        .text_size(text)
                        .text_color(rgb(theme().fg))
                        .child(name),
                )
                .child(controls)
        };

        let text_size_controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                chip("−".into(), false).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.settings.smaller();
                        this.apply_text_size(cx);
                    }),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(cs * 2.4))
                    .text_size(text)
                    .text_color(rgb(theme().fg_dim))
                    .child(SharedString::from(format!("{}", base as i32)))
                    .flex()
                    .justify_center(),
            )
            .child(
                chip("+".into(), false).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.settings.bigger();
                        this.apply_text_size(cx);
                    }),
                ),
            )
            .into_any_element();

        // One chip that cycles System → Light → Dark. Three states, one word
        // each: a segmented control would be three times the chrome for a
        // setting most people touch once.
        let appearance_control = chip(self.settings.appearance.label().into(), false)
            .id("settings-appearance")
            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                this.settings.appearance = this.settings.appearance.next();
                if let Err(err) = this.settings.save(&settings::settings_path()) {
                    eprintln!("gravitynote: could not save settings: {err}");
                }
                cx.notify();
            }))
            .into_any_element();

        // The material names and order mirror the complete matrix in the
        // sibling GPUI Liquid Glass reference, including its no-effect Identity
        // sentinel.
        let glass_control = chip(self.settings.glass.label().into(), false)
            .id("settings-glass")
            .on_click(cx.listener(|this, _e: &ClickEvent, window, cx| {
                this.settings.glass = this.settings.glass.next();
                window.set_background_appearance(window_background_for_glass(this.settings.glass));
                if let Err(err) = this.settings.save(&settings::settings_path()) {
                    eprintln!("gravitynote: could not save settings: {err}");
                }
                cx.notify();
            }))
            .into_any_element();

        let shortcut_controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                // The current chord, or the recording prompt.
                div()
                    .flex_none()
                    .min_w(px(cs * 4.))
                    .px(px(cs * 0.5))
                    .text_size(text)
                    .text_color(rgb(if recording { theme().cursor } else { theme().fg_dim }))
                    .flex()
                    .justify_center()
                    .child(SharedString::from(if recording {
                        "Press keys…".to_string()
                    } else {
                        shortcut_label
                    })),
            )
            .child(
                chip(if recording { "Cancel" } else { "Record" }.into(), !recording)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.recording_shortcut = !this.recording_shortcut;
                            cx.notify();
                        }),
                    ),
            )
            .child(
                chip("Off".into(), false).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.recording_shortcut = false;
                        this.set_toggle_shortcut(None, cx);
                    }),
                ),
            )
            .into_any_element();

        // A pill toggle that reads on the left when on, off on the right.
        let login_toggle = div()
            .id("settings-login")
            .flex_none()
            .w(px(cs * 2.6))
            .h(px(cs * 1.5))
            .rounded_full()
            .bg(rgb(if login_on { theme().cursor } else { theme().find_field_border }))
            .p(px(2.))
            .flex()
            .when(login_on, |d| d.justify_end())
            .cursor(gpui::CursorStyle::PointingHand)
            .child(div().size(px(cs * 1.1)).rounded_full().bg(rgb(theme().bg)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let want = !login_item::enabled();
                    this.notice = Some(match login_item::set(want) {
                        Ok(()) if want => Notice::remark("GravityNote will open at login."),
                        Ok(()) => Notice::remark("GravityNote will no longer open at login."),
                        Err(err) => {
                            Notice::alert(format!("Could not change open at login: {err}"))
                        }
                    });
                    cx.set_menus(menus());
                    cx.notify();
                }),
            )
            .into_any_element();

        // The scrim: clicking outside the card closes the panel.
        div()
            .id("settings-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_settings(cx)),
            )
            .child(
                div()
                    .id("settings-card")
                    // Clicks inside the card must not fall through to the scrim.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(400.))
                    .max_w(relative(0.92))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_5()
                    .bg(rgb(theme().bg))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(theme().rule))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(cs * 1.2))
                                    .text_color(rgb(theme().fg))
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .id("settings-close")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(cs * 1.6))
                                    .rounded_full()
                                    .text_color(rgb(theme().fg_dim))
                                    .hover(|s| s.bg(rgb(theme().find_field_bg)).text_color(rgb(theme().fg)))
                                    .cursor(gpui::CursorStyle::PointingHand)
                                    .child("✕")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| this.close_settings(cx)),
                                    ),
                            ),
                    )
                    .child(div().h(px(1.)).w_full().bg(rgb(theme().rule)))
                    .child(row("Text size", text_size_controls))
                    .child(row("Appearance", appearance_control))
                    .child(row("Glass", glass_control))
                    .child(row("Show / Hide shortcut", shortcut_controls))
                    .child(row("Open at login", login_toggle))
                    .child(
                        div()
                            .text_size(px(cs * 0.85))
                            .text_color(rgb(theme().placeholder))
                            .child(
                                "The Show / Hide shortcut works anywhere on the Mac. \
                                 Record any chord with a modifier, or turn it off.",
                            ),
                    )
                    .child(div().h(px(1.)).w_full().bg(rgb(theme().rule)))
                    .child(
                        div().flex().flex_row().justify_end().child(
                            chip("Restore Defaults".into(), false).id("settings-defaults").on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.restore_settings_defaults(window, cx)
                                }),
                            ),
                        ),
                    ),
            )
    }

    /// The right-click menu: Cut / Copy / Paste / Look Up, anchored at the click.
    ///
    /// GPUI 0.2 ships no `ContextMenu`, so this is a small element built from
    /// `anchored` (so it never spills off-screen) inside `deferred` (so it paints
    /// above the note). A full-window backdrop behind it closes the menu on any
    /// click elsewhere. Cut and Copy grey out with no selection, the way macOS
    /// shows them; the caret was already placed at the click by
    /// [`NoteApp::on_right_mouse_down`].
    fn render_context_menu(&self, at: Point<Pixels>, cx: &mut Context<Self>) -> impl IntoElement {
        let size = chrome_text(self.settings.text_size);
        // Paste is greyed when there is nothing to paste, the way macOS greys it.
        let can_paste = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|t| !t.is_empty());

        // The styled label of one row; the click handler is attached per item so
        // each can call its own command. Disabled rows are greyed and inert.
        let row = move |label: &'static str, enabled: bool| {
            let mut d = div()
                .id(label)
                .px(px(size * 0.9))
                .py(px(size * 0.28))
                .text_color(rgb(if enabled { theme().fg } else { theme().placeholder }))
                .child(label);
            if enabled {
                d = d.cursor_pointer().hover(|s| s.bg(rgb(theme().find_field_bg)));
            }
            d
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // A click anywhere off the menu dismisses it and is swallowed, so it
            // does not also move the caret or reopen the menu behind it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    this.context_menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    this.context_menu = None;
                    cx.notify();
                }),
            )
            // A scroll anywhere dismisses the menu, the way a macOS context menu
            // closes the moment the view moves under it.
            .on_scroll_wheel(cx.listener(|this, _e: &ScrollWheelEvent, _w, cx| {
                cx.stop_propagation();
                this.context_menu = None;
                cx.notify();
            }))
            .child(
                deferred(
                    anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                        div()
                            // The panel swallows its own clicks so they do not
                            // reach the dismiss backdrop underneath.
                            .occlude()
                            .min_w(px(size * 10.))
                            .py(px(4.))
                            .bg(rgb(theme().bg))
                            .border_1()
                            .border_color(rgb(theme().rule))
                            .rounded(px(6.))
                            .text_size(px(size))
                            .font_family("Lilex")
                            // Cut/Copy are always enabled: with no selection they
                            // act on the caret's whole line, exactly as ⌘X/⌘C do,
                            // so the menu and the keyboard no longer disagree.
                            .child(row("Cut", true).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.cut(&Cut, window, cx);
                                    this.context_menu = None;
                                    cx.notify();
                                }),
                            ))
                            .child(row("Copy", true).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.copy(&Copy, window, cx);
                                    this.context_menu = None;
                                    cx.notify();
                                }),
                            ))
                            .child(row("Paste", can_paste).when(can_paste, |d| {
                                d.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _e: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.paste(&Paste, window, cx);
                                        this.context_menu = None;
                                        cx.notify();
                                    }),
                                )
                            }))
                            .child(row("Select All", true).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.select_all(&SelectAll, window, cx);
                                    this.context_menu = None;
                                    cx.notify();
                                }),
                            ))
                            // A hairline between editing and the lookup.
                            .child(
                                div()
                                    .my(px(4.))
                                    .mx(px(size * 0.5))
                                    .h(px(1.))
                                    .bg(rgb(theme().rule)),
                            )
                            .child(row("Look Up", true).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.look_up(cx);
                                }),
                            )),
                    ),
                ),
            )
    }

    /// Hit-test against the visible rows only — the off-screen ones have no
    /// layout, which is exactly what virtualization buys us.
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        self.note
            .clamp_offset(rows::offset_at_point(self.note.len(), &self.line_layouts, position))
    }

    /// The URL under byte `offset`, if it lands on a markdown link or a bare
    /// autolink. Links inside a fenced code block are literal text, not links —
    /// mirroring what the highlighter paints — so a fence line never resolves one.
    fn link_at_offset(&self, offset: usize) -> Option<String> {
        let line = self.index.line_at(offset);
        if self.fences.is_open(line) {
            return None;
        }
        let (start, end) = self.index.line_range(line)?;
        let raw = &self.note.text()[start..end];
        markdown::link_at(raw, offset - start)
    }

    /// Hand a link to the system browser. A URL with no scheme — a bare domain in
    /// a `[text](url)` — is given `https://`, or `open` would read it as a path.
    fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        // A note is a document, and a document from anywhere — synced, pasted,
        // imported — can carry a link that is not a web page. `open` will
        // happily launch an application for `file://`, or for whatever scheme
        // something on this Mac has registered. So the scheme is judged on what
        // the note actually says, *before* a bare domain is given `https://`:
        // deciding afterwards let `javascript:…` through as `https://javascript:…`,
        // which is not what the link said and not what was checked.
        //
        // Schemes are case-insensitive; `HTTP://EXAMPLE.COM` is what a lot of
        // pasted text looks like.
        let scheme = url.split_once(':').map(|(s, _)| s).unwrap_or("");
        let has_scheme = !scheme.is_empty() && !scheme.contains(['/', ' ', '.', '?']);
        if has_scheme
            && !["http", "https", "mailto"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        {
            self.notice = Some(Notice::alert(format!(
                "Not opening {url} — only web and mail links open from a note."
            )));
            cx.notify();
            return;
        }
        let target = if has_scheme {
            url.to_string()
        } else {
            format!("https://{url}")
        };
        open_with_finder(&target);
    }

    /// The side of the wrap a click at `position` landed on.
    fn affinity_for_position(&self, offset: usize, position: Point<Pixels>) -> rows::Affinity {
        let line = self.index.line_at(offset);
        match self.line_layouts.get(&line) {
            Some(row) => {
                rows::affinity_at(self.note.text(), &self.index, row, offset, position.y)
            }
            None => rows::Affinity::RowEnd,
        }
    }

    /// The find bar: a question about the note, asked above it.
    ///
    /// It stands in for the top inset while it is open, so opening it moves the
    /// text as little as possible. The left padding clears the traffic lights,
    /// which float over this strip whether or not they are showing.
    fn render_find_bar(&self, find: &Find, width: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let mut caret: Hsla = rgb(theme().cursor).into();
        caret.a = self.caret_alpha;
        let text_size = chrome_text(self.settings.text_size);
        let has_query = !find.query().is_empty();
        // A search that finds nothing tints the field and its count, so "no
        // matches" is felt at a glance rather than read off a small grey label.
        let broken = find.pattern_is_broken();
        let no_matches = has_query && (find.matches().is_empty() || broken);
        let pill_h = px(text_size * 2.1);
        let icon = px(text_size * 0.95);
        // What the bar can afford at this window width. Everything except the
        // query and the close ✕ is `flex_none`, so in a narrow window the one
        // thing that could shrink was the one thing that must not: the query
        // text was squeezed to nothing, the match count was drawn over the clear
        // chip, and a chevron was cut off by the window edge. The app shrinks to
        // 180 points wide on purpose, so the bar has to have an answer for it.
        //
        // Nothing dropped here is unreachable: ⌥⌘F toggles replace, ⏎ and ⇧⏎
        // step through matches, and the count is a convenience.
        let full = f32::from(width) - f32::from(side_inset(self.settings.text_size)) * 2.;
        // Where the bar's own column starts, and therefore whether it runs into
        // the window buttons' corner.
        let column = f32::from(width).min(f32::from(reading_width(self.settings.text_size)));
        let column_left = (f32::from(width) - column) / 2. + f32::from(side_inset(self.settings.text_size));
        let gutter_left = (TRAFFIC_LIGHT_GUTTER - column_left).max(0.);
        // In a window this narrow the gutter would take a third of the bar, so
        // the bar goes *under* the strip the lights sit on instead of beside
        // them. Same idea, one row down.
        let stack_below = gutter_left > 1. && full - gutter_left < text_size * 14.;
        let reserve_gutter = gutter_left > 1. && !stack_below;
        // What is left for the bar itself once the lights have their corner.
        // Measuring the thresholds against the *window* instead meant widening
        // it by two points could take the gutter and shrink the query.
        let room = if reserve_gutter { full - gutter_left } else { full };
        // Room for the buttons *and* for the query to still show something: the
        // pill's own furniture — magnifier, toggles, count, clear chip — is all
        // `flex_none`, so it is counted here rather than discovered at layout
        // time by the query text collapsing to nothing.
        let show_toggles = room > text_size * 40.;
        let show_count = room > text_size * 27.;
        let show_chevrons = room > text_size * 21.;
        // Last to go: the query itself is what the bar is for, and ⌘F with the
        // text offered replaces it in one keystroke anyway.
        let show_clear = room > text_size * 17.;

        // One plain span of field text.
        let span = |s: &str| {
            div()
                .flex_none()
                .text_color(rgb(theme().fg))
                .child(SharedString::from(s.to_string()))
        };
        let caret_block = move || div().flex_none().w(CARET_WIDTH).h(px(text_size * 1.3)).bg(caret);

        // One editable field drawn around its caret. `show_caret` is on for the
        // field that currently holds the keyboard; the other draws as plain text.
        // With a selection the caret gives way to the wash. An empty field shows
        // its semi-transparent placeholder, with the caret ahead of it.
        let field_view = |text: &str,
                          caret_at: usize,
                          sel: Range<usize>,
                          has_sel: bool,
                          show_caret: bool,
                          placeholder: &'static str| {
            let base = div()
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w_0()
                .overflow_hidden();
            let mut ph: Hsla = rgb(theme().placeholder).into();
            ph.a = 0.55;
            if text.is_empty() {
                let base = if show_caret { base.child(caret_block()) } else { base };
                base.child(div().flex_none().text_color(ph).child(placeholder))
            } else if show_caret && has_sel {
                base.child(span(&text[..sel.start]))
                    .child(
                        div()
                            .flex_none()
                            .rounded_sm()
                            .bg(rgb(theme().selection))
                            .text_color(rgb(theme().fg))
                            .child(SharedString::from(text[sel.clone()].to_string())),
                    )
                    .child(span(&text[sel.end..]))
            } else if show_caret {
                base.child(span(&text[..caret_at]))
                    .child(caret_block())
                    .child(span(&text[caret_at..]))
            } else {
                base.child(span(text))
            }
        };

        // Which field the caret belongs to. The bar can be blurred (a body click
        // handed the keyboard back to the note) — then neither field shows one.
        let focused = self.find_focused;
        let on_replace = find.active_is_replacement();
        let query_caret = focused && !on_replace;
        let replace_caret = focused && on_replace;

        // The Match Case / Whole Word / Pattern / This Note toggles. `which`
        // says which one, so the four share one chip.
        let toggle = |id: &'static str, label: &'static str, active: bool, which: Toggle| {
            let tip: SharedString = match which {
                Toggle::MatchCase => "Match Case",
                Toggle::WholeWord => "Whole Word",
                Toggle::Regex => "Regular Expression",
                Toggle::NoteOnly => "Search This Note Only",
            }
            .into();
            div()
                .id(id)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(text_size * 1.5))
                .rounded_md()
                .text_size(px(text_size * 0.82))
                .cursor(gpui::CursorStyle::PointingHand)
                .tooltip(move |_window, cx| cx.new(|_| TextTooltip(tip.clone())).into())
                .child(label)
                .when(active, |b| b.bg(rgb(theme().cursor)).text_color(rgb(theme().find_field_bg)))
                .when(!active, |b| {
                    b.text_color(rgb(theme().fg_dim)).hover(|s| s.bg(rgb(theme().btn_bg_hover)))
                })
                .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| match which {
                    Toggle::MatchCase => this.toggle_match_case(cx),
                    Toggle::WholeWord => this.toggle_whole_word(cx),
                    Toggle::Regex => this.toggle_regex(cx),
                    Toggle::NoteOnly => this.toggle_note_scope(cx),
                }))
        };

        // The disclosure that shows / hides the replace row — a ⇄ that lights
        // when replace mode is on.
        let replacing = find.replacing();
        let replace_toggle = div()
            .id("find-replace-toggle")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(pill_h)
            .rounded_full()
            .cursor(gpui::CursorStyle::PointingHand)
            .tooltip(|_window, cx| cx.new(|_| TextTooltip("Find & Replace".into())).into())
            .text_size(px(text_size * 1.05))
            .when(replacing, |b| b.text_color(rgb(theme().cursor)))
            .when(!replacing, |b| b.text_color(rgb(theme().btn_fg)).hover(|s| s.bg(rgb(theme().btn_bg_hover))))
            .child("⇄")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.toggle_replace(&ToggleReplace, window, cx)),
            );

        // The query pill: magnifier, the query (with its caret), the case/word
        // toggles, the running count, and the clear chip.
        let query_field = div()
            .flex_1()
            .min_w(px(text_size * 6.))
            .overflow_hidden()
            .h(pill_h)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px(px(text_size * 0.7))
            .rounded_full()
            .bg(rgb(theme().find_field_bg))
            .border_1()
            .border_color(rgb(if no_matches { theme().danger } else { theme().find_field_border }))
            .child(
                svg()
                    .path("icons/search.svg")
                    .flex_none()
                    .size(icon)
                    .text_color(rgb(theme().fg_dim)),
            )
            .child(field_view(find.query(), find.caret(), find.selection(), find.has_selection(), query_caret, "Search"))
            .when(show_toggles, |row| {
                row.child(toggle(
                    "find-match-case",
                    "Aa",
                    find.match_case(),
                    Toggle::MatchCase,
                ))
                .child(toggle(
                    "find-whole-word",
                    "W",
                    find.whole_word(),
                    Toggle::WholeWord,
                ))
                .child(toggle("find-regex", ".*", find.is_regex(), Toggle::Regex))
                .child(toggle("find-note-only", "¶", self.find_note_only, Toggle::NoteOnly))
            })
            .when(has_query && show_count, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(text_size * 0.85))
                        .text_color(rgb(if no_matches { theme().danger } else { theme().fg_dim }))
                        .child(SharedString::from(find.status())),
                )
            })
            .when(has_query && show_clear, |row| {
                row.child(
                    div()
                        .id("find-clear")
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(text_size * 1.15))
                        .rounded_full()
                        .bg(rgb(theme().find_clear_bg))
                        .text_size(px(text_size * 0.7))
                        .text_color(rgb(theme().find_field_bg))
                        .hover(|s| s.bg(rgb(theme().find_clear_bg_hover)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.clear_query(cx)),
                        )
                        .child("✕"),
                )
            });

        // A round chevron button for stepping through matches.
        let chevron = |id: &'static str, path: &'static str, forward: bool| {
            div()
                .id(id)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(pill_h)
                .rounded_full()
                .hover(|s| s.bg(rgb(theme().btn_bg_hover)))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(svg().path(path).size(px(text_size * 1.05)).text_color(rgb(theme().btn_fg)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.step_find(forward, cx)),
                )
        };

        // The close ✕ — the one affordance that dismisses the bar.
        let close_button = div()
            .id("find-close")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(pill_h)
            .rounded_full()
            .hover(|s| s.bg(rgb(theme().btn_bg_hover)))
            .cursor(gpui::CursorStyle::PointingHand)
            .tooltip(|_window, cx| cx.new(|_| TextTooltip("Close Find".into())).into())
            .text_size(px(text_size * 1.05))
            .text_color(rgb(theme().btn_fg))
            .child("✕")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.close_find_bar(cx);
                    cx.stop_propagation();
                }),
            );

        // A pill-shaped action button for the replace row.
        let has_current = find.current().is_some();
        let action = |id: &'static str, label: &'static str, enabled: bool, replace_all: bool| {
            let mut b = div()
                .id(id)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .h(pill_h)
                .px(px(text_size * 0.7))
                .rounded_full()
                .text_size(px(text_size * 0.85))
                .child(label);
            if enabled {
                b = b
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(rgb(theme().find_field_bg))
                    .border_1()
                    .border_color(rgb(theme().find_field_border))
                    .text_color(rgb(theme().fg))
                    .hover(|s| s.bg(rgb(theme().btn_bg_hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if replace_all {
                                this.replace_all(&ReplaceAll, window, cx);
                            } else {
                                this.replace_and_find(&ReplaceAndFind, window, cx);
                            }
                        }),
                    );
            } else {
                b = b.text_color(rgb(theme().fg_dim));
            }
            b
        };

        // The replacement row, shown only in replace mode.
        let replace_row = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(pill_h)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px(px(text_size * 0.7))
                    .rounded_full()
                    .bg(rgb(theme().find_field_bg))
                    .border_1()
                    .border_color(rgb(theme().find_field_border))
                    // A small spacer where the magnifier sits on the query row, so
                    // the two fields' text lines up.
                    .child(div().flex_none().size(icon))
                    .child(field_view(find.replacement(), find.caret(), find.selection(), find.has_selection(), replace_caret, "Replace with…")),
            )
            .child(action("find-replace-one", "Replace", has_current, false))
            .child(action("find-replace-all", "All", has_query && !no_matches, true));

        // The bar: the query row, and — in replace mode — the replacement row
        // beneath it. A left disclosure toggles the second row.
        div()
            .id("find-bar")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.focus_find(cx);
                    cx.stop_propagation();
                }),
            )
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .justify_center()
            .py(px(text_size * 0.45))
            .when(stack_below, |bar| bar.mt(px(TITLEBAR_H)))
            .min_h(px(text_size * 3.0).max(px(44.)))
            .text_size(px(text_size))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(reading_width(self.settings.text_size))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px(side_inset(self.settings.text_size))
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    // Room for the traffic lights to appear in.
                                    // The bar stands where the title bar would
                                    // be, so without this it covers them and a
                                    // search leaves no way to close the window
                                    // by mouse. Only when the bar actually
                                    // reaches that far left: a wide window
                                    // centres it well clear of them.
                                    .when(reserve_gutter, |row| {
                                        row.child(div().flex_none().w(px(gutter_left)))
                                    })
                                    .when(show_toggles, |row| row.child(replace_toggle))
                                    .child(query_field)
                                    .when(show_chevrons, |row| {
                                        row.child(chevron(
                                            "find-prev",
                                            "icons/chevron-up.svg",
                                            false,
                                        ))
                                        .child(chevron(
                                            "find-next",
                                            "icons/chevron-down.svg",
                                            true,
                                        ))
                                    })
                                    .child(close_button),
                            )
                            .when(replacing, |col| col.child(replace_row)),
                    ),
            )
    }

    // ── Images ──────────────────────────────────────────────────────────────

    /// The widest an image may be drawn: the reading column, less what the row
    /// bleeds either side. Falls back to the measure before the column has been
    /// laid out once.
    fn image_measure(&self) -> f32 {
        let base = self.settings.text_size;
        // `viewport` is the canvas's bounds, and the canvas fills the column's
        // *padding* box — so the row's content width is that less the column
        // inset as well as the row's own bleed. Counting only the bleed made
        // every full-width picture overhang the text it sits among, and in a
        // narrow window pushed it under the scroll rail.
        let column = self
            .viewport
            .map(|b| f32::from(b.size.width))
            .unwrap_or_else(|| f32::from(reading_width(base)));
        let insets = (f32::from(column_inset(base)) + f32::from(row_bleed(base))) * 2.;
        // Only a floor against a degenerate box. It used to be four rows, which
        // is wider than the column itself once the window is small and the text
        // large — the picture then reached out under the scroll rail, which is
        // the very thing the insets above are subtracted to prevent.
        (column - insets).max(1.)
    }

    /// The box an image line is actually drawn at, in points.
    ///
    /// The note records what the reader asked for; this is what fits. Two
    /// clamps, both load-bearing:
    ///
    /// * **Width** to the reading measure, so an image is never wider than the
    ///   text it sits among and never overflows a narrowed window.
    /// * **Height** to the viewport, because GPUI's list never clamps a scroll
    ///   offset to a row that has since shrunk. A row taller than the window
    ///   can be scrolled into and then, when the caret arrives and it collapses
    ///   to one line of markdown, the page lurches by whatever the difference
    ///   was — and `scroll_to_reveal_item` works in whole items, so such a row
    ///   could never be brought fully into view anyway.
    fn image_box(&self, asked: (f32, f32)) -> (f32, f32) {
        let (w, h) = asked;
        let max_w = self.image_measure();
        let max_h = self
            .viewport
            .map(|b| f32::from(b.size.height) * 0.9)
            .unwrap_or(f32::INFINITY);
        let scale = 1.0_f32.min(max_w / w).min(max_h / h).max(0.01);
        ((w * scale).max(1.), (h * scale).max(1.))
    }

    /// What the note says this image's box is, before clamping: the recorded
    /// `|WxH`, the size a live drag has reached, or — for a reference that
    /// never carried one — the image's own size once it is known.
    fn asked_box(&self, line: usize, reference: &images::Reference, intrinsic: Option<(f32, f32)>) -> (f32, f32) {
        if let Some(drag) = self.image_drag.as_ref().filter(|d| d.line == line) {
            return (drag.width, drag.width / drag.aspect);
        }
        if let Some((w, h)) = reference.box_ {
            return (w as f32, h as f32);
        }
        // A hand-typed reference with no size. Fit the measure until the image
        // arrives, then take its own proportions — one reflow, once.
        match intrinsic {
            Some((w, h)) => {
                let width = w.min(self.image_measure());
                (width, width * h / w)
            }
            None => {
                let width = self.image_measure();
                (width, width * 0.6)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_image_row(
        &mut self,
        line: usize,
        reference: &images::Reference,
        reveal: bool,
        selected: bool,
        byte_start: usize,
        byte_end: usize,
        row: gpui::Stateful<Div>,
        text: StyledText,
        overlay: RowOverlay,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let base = self.settings.text_size;
        let path = images::path(&self.images_dir, &reference.name);
        let resolved = self.bank.get(&path, window, cx);

        let intrinsic = match &resolved {
            image_bank::Resolved::Ready(data) => {
                let size = data.size(0);
                Some(self.intrinsic_points(
                    size.width.0.max(0) as u32,
                    size.height.0.max(0) as u32,
                    window.scale_factor(),
                ))
            }
            _ => None,
        };
        let (w, h) = self.image_box(self.asked_box(line, reference, intrinsic));
        let radius = px(base * 0.3);

        // Both dimensions are definite, and the element may not shrink. `img`
        // sets an aspect ratio on itself the moment the image lands
        // (`img.rs:336`), so a flex child that was free to shrink would have
        // its height recomputed then and the row would jump under the reader.
        let sized = |el: Div| el.w(px(w)).h(px(h)).flex_shrink_0().rounded(radius);

        let picture = match resolved {
            image_bank::Resolved::Ready(data) => sized(div())
                .child(img(ImageSource::Render(data)).w(px(w)).h(px(h)).rounded(radius))
                .into_any_element(),
            // Decoding. A quiet plate the same size as the picture, so nothing
            // moves when it arrives.
            image_bank::Resolved::Loading => {
                sized(div()).bg(rgb(theme().codeblock_bg)).into_any_element()
            }
            // The reference points at nothing. Say so inside the plate: the
            // name is longer than most plates are wide, and unconstrained it
            // spilled out both sides and was clipped by the window — reading as
            // "g: deadbeefdeadbeef.png" beside a grey rectangle. A plate too
            // small for any of it gets a mark instead.
            image_bank::Resolved::Missing => {
                const PREFIX: usize = "missing: ".len();
                let room = (w / (base * 0.55)) as usize;
                let name = reference.name.chars().count();
                let label = if room < PREFIX + 4 {
                    // No room for "missing: " and enough of a name to recognise.
                    "?".to_string()
                } else if room >= name + PREFIX {
                    format!("missing: {}", reference.name)
                } else {
                    format!("missing: {}", ellipsise(&reference.name, room - PREFIX))
                };
                sized(div())
                    .bg(rgb(theme().codeblock_bg))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .px(px(base * 0.3))
                    .text_size(px(base * 0.85))
                    .text_color(rgb(theme().placeholder))
                    .child(SharedString::from(label))
                    .into_any_element()
            }
        };

        // Hovering anywhere on the picture brings its grip up, so the handle is
        // findable without being permanent furniture on a page of images.
        let group = SharedString::from(format!("image-group-{line}"));
        let dragging = self.image_drag.as_ref().is_some_and(|d| d.line == line);
        let framed = selected || dragging || dev::show_handles();
        // Big enough to hit without a steady hand, small enough not to be
        // furniture; tied to the text size so it keeps its proportions when the
        // note is scaled.
        let knob = (base * 0.8).max(9.);

        let block = div()
            .id(ElementId::Name(format!("image-{line}").into()))
            .group(group.clone())
            .relative()
            .w(px(w))
            .h(px(h))
            .child(picture)
            // A selected picture is framed, not washed: a tint over a
            // photograph reads as a fault in the photograph.
            //
            // Two rules, because one is not enough. A blue frame on a blue
            // image is invisible — verified, it was — so the accent sits just
            // *outside* the picture and a white hairline runs along the
            // picture's own edge to separate them. Between them they read on a
            // photograph of anything: dark, light, or the accent's own colour.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(w))
                    .h(px(h))
                    // Up while the picture is selected, and while the pointer is
                    // anywhere over it — a handle you can only find once you
                    // have found it is no handle at all.
                    .opacity(if framed { 1. } else { 0. })
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(
                        div()
                            .absolute()
                            .top(px(-2.))
                            .left(px(-2.))
                            .w(px(w + 4.))
                            .h(px(h + 4.))
                            .rounded(radius + px(2.))
                            .border_2()
                            .border_color(rgb(theme().cursor)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w(px(w))
                            .h(px(h))
                            .rounded(radius)
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 1., 0.85)),
                    )
                    // The grip, centred on the corner: half on the picture, half
                    // on the paper, so it reads as a handle on the boundary
                    // rather than a button placed over the image.
                    .child(
                        div()
                            .absolute()
                            .left(px(w - knob / 2.))
                            .top(px(h - knob / 2.))
                            .size(px(knob))
                            .rounded(px(1.5))
                            .bg(rgb(theme().bg))
                            .border_2()
                            .border_color(rgb(theme().cursor)),
                    ),
            )
            // Clicking a picture selects it — it does not put a caret inside its
            // markdown. Selecting the whole reference is what makes ⌘C, ⌘X, ⌫
            // and typing-to-replace work on it, since every one of those already
            // acts on a selection.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _e: &MouseDownEvent, _w, cx| {
                    // `set_selection` resets row motion for us. Breaking the
                    // undo group is what every other caret-moving click does,
                    // so typing after clicking a picture does not coalesce with
                    // typing from before it.
                    this.history.break_group();
                    this.set_selection(byte_start, byte_end);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(self.render_image_grip(line, w, h, base, cx));

        // The source line sits above the picture while the caret is on it, in
        // flow so it takes real space. The rest of the time the same shaped text
        // is pinned, invisible, to the row's top-left: it costs no height, and
        // it is what registers the row in `line_layouts` — without which the
        // caret cannot be drawn here, ↑/↓ lose their column, and the IME has
        // nowhere to put a candidate window.
        let text_el = div().flex_1().min_w_0().child(text).child(overlay);
        let column = row.relative().flex_col().items_start();
        let column = if reveal {
            column.child(div().flex().flex_row().w_full().child(text_el))
        } else {
            // Taffy resolves absolute insets against the padding box, so
            // `left_0` would put the invisible reference a bleed to the left of
            // where every other row's text starts — and ↑/↓ onto this line
            // would then land a character or two off the column they were
            // aiming for.
            column.child(
                div()
                    .absolute()
                    .top_0()
                    .left(row_bleed(base))
                    .child(text_el),
            )
        };
        column.child(block).into_any_element()
    }

    /// The target that resizes an image: the bottom-right corner itself.
    ///
    /// It draws nothing. The heavy crop mark already sitting there is the thing
    /// you see, and this is the square of pointer around it that can be taken
    /// hold of — comfortably bigger than the mark, because a handle sized to
    /// its own artwork is a handle you miss.
    #[allow(clippy::too_many_arguments)]
    fn render_image_grip(
        &self,
        line: usize,
        w: f32,
        h: f32,
        base: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let reach = (base * 2.0).max(22.);
        let aspect = w / h.max(1.);
        div()
            .id(ElementId::Name(format!("image-grip-{line}").into()))
            .absolute()
            .left(px(w - reach / 2.))
            .top(px(h - reach / 2.))
            .size(px(reach))
            .cursor(gpui::CursorStyle::ResizeUpLeftDownRight)
            .tooltip(|_window, cx| {
                cx.new(|_| TextTooltip("Drag to resize · double-click to reset".into()))
                    .into()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    if e.click_count >= 2 {
                        this.reset_image_size(line, cx);
                        return;
                    }
                    let Some(name) = this
                        .index
                        .line_range(line)
                        .and_then(|(a, b)| images::parse_reference(&this.note.text()[a..b]))
                        .map(|r| r.name)
                    else {
                        return;
                    };
                    this.image_drag = Some(ImageDrag {
                        line,
                        name,
                        start_w: w,
                        start_x: e.position.x,
                        aspect,
                        width: w,
                    });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// Track a resize. Only the view changes; the note is rewritten on release.
    fn drag_image(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let max = self.image_measure();
        let Some(drag) = self.image_drag.as_mut() else {
            return;
        };
        let delta = f32::from(position.x - drag.start_x);
        let min = self.settings.text_size * 4.;
        // Capped at the reading measure: past it the grip would sit still under
        // a picture that had stopped growing, while the number being committed
        // kept climbing.
        drag.width = (drag.start_w + delta).clamp(min, max);
        cx.notify();
    }

    /// Commit a resize: one edit, one undo entry, and the caret left exactly
    /// where it was.
    ///
    /// The caret matters. `replace_text_in_range` would leave it at the end of
    /// what it inserted — on the image's own line — and a caret on an image
    /// line is the cue to show markdown instead of the picture. Every resize
    /// would end by hiding the thing that had just been resized.
    fn end_image_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.image_drag.take() else {
            return;
        };
        // A press and release with no travel is a click, not a resize. The grip
        // is handed the *clamped* box, so committing it would quietly replace a
        // recorded size with whatever happened to fit the window — and widening
        // the window later would not bring it back.
        if (drag.width - drag.start_w).abs() < 1.0 {
            return;
        }
        // The line may not be the same picture any more; see `ImageDrag::name`.
        let still_ours = self
            .index
            .line_range(drag.line)
            .and_then(|(a, b)| images::parse_reference(&self.note.text()[a..b]))
            .is_some_and(|r| r.name == drag.name);
        if !still_ours {
            return;
        }
        let width = drag.width.round().max(1.);
        let height = (width / drag.aspect).round().max(1.);
        self.rewrite_image_line(drag.line, Some((width as u32, height as u32)), cx);
    }

    /// Drop the recorded size, so the image goes back to its own proportions.
    fn reset_image_size(&mut self, line: usize, cx: &mut Context<Self>) {
        self.image_drag = None;
        self.rewrite_image_line(line, None, cx);
    }

    /// Replace an image line's reference with the same name at a new box.
    fn rewrite_image_line(&mut self, line: usize, box_: Option<(u32, u32)>, cx: &mut Context<Self>) {
        // The same two refusals `replace_text_in_range` makes. The grip is
        // reachable by mouse while either is true.
        if self.modal_open() || self.find_focused {
            return;
        }
        let Some((start, end)) = self.index.line_range(line) else {
            return;
        };
        let Some(reference) = images::parse_reference(&self.note.text()[start..end]) else {
            return;
        };
        if reference.box_ == box_ {
            return;
        }
        // The alt text is the only thing on that line the reader wrote, so a
        // resize puts back what was there. Only the `|WxH` half is ours to
        // replace: it is the last `|`-separated field by construction.
        let alt = reference
            .alt
            .rsplit_once('|')
            .map(|(head, _)| head)
            .unwrap_or(&reference.alt)
            .to_string();
        let replacement = match box_ {
            Some((w, h)) => format!("![{alt}|{w}x{h}]({}{})", images::URL_PREFIX, reference.name),
            None => format!("![{alt}]({}{})", images::URL_PREFIX, reference.name),
        };
        let removed = self.note.text()[start..end].to_string();
        if removed == replacement {
            return;
        }
        // The caret is restored rather than followed, and the surrounding
        // groups are broken so a resize is never coalesced into the typing
        // before or after it.
        let before = (self.selected_range.start, self.selected_range.end);
        let shift = replacement.len() as isize - removed.len() as isize;
        let restore = |offset: usize| -> usize {
            if offset > end {
                (offset as isize + shift).max(0) as usize
            } else if offset == end {
                // The end of the reference stays the end of the reference. The
                // usual gesture is *click the picture, then drag its grip*, and
                // clicking selects the whole reference — collapsing that to a
                // caret would put the caret on the image's line, which is the
                // cue to reveal its markdown. Every resize would have ended by
                // hiding the thing it had just resized.
                start + replacement.len()
            } else if offset > start {
                // Anywhere else inside it: there is nowhere meaningful to be
                // in the middle of a reference, so sit at its start.
                start
            } else {
                offset
            }
        };
        let after = (restore(before.0), restore(before.1));

        self.history.break_group();
        self.note.replace_range(start, end, &replacement);
        self.selected_range = after.0..after.1;
        // The caret tracks `cursor_offset`, not the selection's start — that is
        // what `reset_row_motion` is for, and hand-rolling it put the caret on
        // the wrong end of a forward selection.
        self.reset_row_motion();
        // A composition in flight names bytes that this edit just moved. Every
        // other path that rewrites the buffer out from under the IME clears it
        // for the same reason.
        self.marked_range = None;
        self.history.record(
            Edit {
                start,
                removed: removed.clone(),
                inserted: replacement.clone(),
                before,
                after,
            },
            Instant::now(),
        );
        self.history.break_group();
        self.after_edit(start, &removed, &replacement);
        cx.notify();
    }

    /// Copy images into the store and write references to them at the caret.
    ///
    /// Hashing and writing twenty megabytes is not something a frame waits for,
    /// so the work runs on the background executor and the note is edited when
    /// it lands. The reference is inserted through the ordinary edit path, so a
    /// pasted image is undoable, autosaved and backed up like any other typing.
    fn import_images(
        &mut self,
        source: ImportSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find_focused {
            self.blur_find();
        }
        let dir = self.images_dir.clone();
        cx.spawn_in(window, async move |this, cx| {
            let imported = cx
                .background_executor()
                .spawn(async move {
                    match source {
                        ImportSource::Bytes(bytes) => {
                            vec![images::import_bytes(&dir, &bytes)]
                        }
                        ImportSource::Files(paths) => paths
                            .iter()
                            .map(|path| images::import_file(&dir, path))
                            .collect(),
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.insert_imported(imported, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Write the references for a finished import, and report anything that
    /// could not be read rather than dropping it in silence.
    fn insert_imported(
        &mut self,
        imported: Vec<std::io::Result<images::Import>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scale = window.scale_factor();
        let mut insert = String::new();
        let mut failed = 0;
        for result in imported {
            match result {
                Ok(import) => {
                    let (w, h) = self.default_image_box(&import, scale);
                    insert.push_str(&images::format_reference(&import.name, w, h));
                    insert.push('\n');
                }
                Err(_) => failed += 1,
            }
        }
        if failed > 0 {
            self.notice = Some(Notice::alert(format!(
                "{failed} file{} could not be read as an image",
                if failed == 1 { "" } else { "s" }
            )));
        }
        if insert.is_empty() {
            cx.notify();
            return;
        }
        // The import ran on the background executor, so the keyboard may have
        // moved somewhere modal since. `replace_text_in_range` refuses outright
        // while Settings is open, and routes into the query while Find has the
        // keyboard — where a string containing a newline is discarded. Either
        // way the file would be in the store and the reference nowhere.
        self.settings_open = false;
        self.backups_open = false;
        if self.find_focused {
            self.blur_find();
        }
        // A reference has to have its line to itself, or it is prose that
        // happens to mention an image and it will not draw. The test is against
        // where the insertion will *land* — the start of the range being
        // replaced — not against the caret, which for a forward selection is at
        // the other end and would say the line was already clear when it is not.
        let at = self
            .marked_range
            .clone()
            .unwrap_or_else(|| self.selected_range.clone())
            .start
            .min(self.note.text().len());
        if at > 0 && !self.note.text()[..at].ends_with('\n') {
            insert.insert(0, '\n');
        }
        self.history.break_group();
        self.replace_text_in_range(None, &insert, window, cx);
        self.history.break_group();
    }

    /// The box a freshly imported image is written at.
    ///
    /// Its own size in points — device pixels are twice points on every display
    /// this app runs on — clamped to the reading measure, so a screenshot lands
    /// at the width of the text it sits among instead of overflowing it.
    fn default_image_box(&self, import: &images::Import, scale: f32) -> (u32, u32) {
        let (w, h) = self.intrinsic_points(import.w, import.h, scale);
        let width = w.min(self.image_measure());
        (
            (width.round()).max(1.) as u32,
            (width * h / w).round().max(1.) as u32,
        )
    }

    /// An image's own size in points. Pixels divided by the display's scale
    /// factor — not by a hard 2, which is right on every Retina display and
    /// wrong by half on an external 1x monitor.
    fn intrinsic_points(&self, w: u32, h: u32, scale: f32) -> (f32, f32) {
        let scale = if scale > 0. { scale } else { 2. };
        (w.max(1) as f32 / scale, h.max(1) as f32 / scale)
    }

    // ── Row rendering (the list asks for one visible line at a time) ─────────

    /// Whether `line` sits in a four-space indented code block.
    ///
    /// A run of indented lines is one block, and only its first line can be
    /// judged on its own — so this walks back to the top of the run and asks
    /// what preceded it. Asking only the line directly above made the *third*
    /// line of a two-space outline code while the second was not.
    ///
    /// Bounded rather than unbounded: a run longer than this is a code block by
    /// any reading, and the frame path may not walk the document.
    fn starts_indented_code(&self, line: usize) -> bool {
        const MAX_RUN: usize = 200;
        let raw = |l: usize| {
            self.index
                .line_range(l)
                .map(|(a, b)| &self.note.text()[a..b])
        };
        if !raw(line).is_some_and(|l| markdown::is_indented_code(l, true)) {
            return false;
        }
        let mut above = line;
        for _ in 0..MAX_RUN {
            let Some(previous) = above.checked_sub(1) else {
                // The run reaches the top of the document, which is as good as
                // a blank line before it.
                return true;
            };
            match raw(previous) {
                // A blank line opens the block.
                Some(l) if l.trim().is_empty() => return true,
                // Still inside the run.
                Some(l) if markdown::is_indented_code(l, true) => above = previous,
                // Prose (or a list item) directly above: this is a continuation
                // of that, not a code block.
                _ => return false,
            }
        }
        true
    }

    fn render_row(&mut self, line: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (byte_start, byte_end) = self
            .index
            .line_range(line)
            .unwrap_or((self.index.len(), self.index.len()));
        let raw = &self.note.text()[byte_start..byte_end];

        let in_fence = self.fences.is_open(line);
        // Four-space (or tab) indented code, which is code in every markdown
        // parser and was plain prose here. Decided from the line above: a run of
        // indented lines is one block, and only its first line can be judged on
        // its own.
        let indented_code = !in_fence && self.starts_indented_code(line);
        // A note boundary is an explicit `---`, and only outside a fenced block
        // — inside one it is code. Answered from this line and the fence state,
        // never a document scan.
        let is_separator = note::is_boundary_line(self.note.text(), &self.index, line, self.fences.in_closed_fence(line));
        // Band the whole row for fenced code, fence lines included, so the block
        // reads as one surface. An indented block gets the same surface.
        let in_code_block = in_fence || markdown::is_fence(raw) || indented_code;
        let placeholder = self.note.is_empty() && line == 0;

        // An image line draws as a picture — unless the caret is sitting on it,
        // when it shows its own markdown so it can be read and edited. That is
        // Obsidian's live preview, and it is also what keeps every caret
        // question simple: the moment the caret is here, this is an ordinary
        // text row and none of the caret machinery has ever met an image.
        //
        // A selection deliberately does *not* reveal. It would mean a drag
        // across an image shrank the row by hundreds of pixels mid-drag, moving
        // the content under the pointer, which moves the drag's endpoint. The
        // selection tints the picture instead.
        //
        // Parsing is a couple of `find`s on one line, and it is measured beside
        // `is_separator` in `highlighting_a_viewport_is_fast`.
        // A line that is nothing but a reference into the store draws as a
        // picture. Parsing is a couple of `find`s on one line, and it is
        // measured beside `is_separator` in `highlighting_a_viewport_is_fast`.
        // `in_code_block` already covers `in_fence`.
        let image = (!in_code_block)
            .then(|| images::parse_reference(raw))
            .flatten();
        // The source line appears when the caret is on it — *above* the picture,
        // which stays. This is the part of Obsidian's live preview that matters:
        // you can see what you are editing and what it produces at the same
        // time. Hiding the picture to show its markdown means every edit to a
        // size is made blind.
        //
        // `self.caret` is not "the caret when nothing is selected" — every
        // selection change resets it to the moving endpoint of a drag — so this
        // asks for an empty selection and reads the offset the drawn caret
        // reads. A selection therefore never reveals, which is also what stops a
        // drag across an image resizing the row under the pointer.
        let reveal = image.is_some()
            && self.selected_range.is_empty()
            && self.cursor_offset() >= byte_start
            && self.cursor_offset() <= byte_end
            && !self.image_drag.as_ref().is_some_and(|d| d.line == line);

        // The caret's fade repaints the window about twenty-five times a second,
        // and every one of those frames used to copy each visible line into a
        // fresh string and highlight it again. Same revision and same byte range
        // means the same text, which is the whole of the invalidation.
        let cached = self
            .row_cache
            .get(&line)
            .filter(|c| {
                c.revision == self.revision
                    && c.range == (byte_start, byte_end)
                    && c.in_fence == (in_fence || indented_code)
            })
            .cloned();
        let RowText { spans, display, .. } = match cached {
            Some(hit) => {
                // The cache's whole claim is "same revision and same range means
                // same text". An incremental cache whose failure mode is showing
                // the wrong line deserves to be checked rather than trusted, and
                // in a debug build it is: this is one comparison per visible row.
                debug_assert!(
                    placeholder || raw.is_empty() || hit.display.as_ref() == raw,
                    "row cache returned {:?} for a line that now reads {raw:?}",
                    hit.display
                );
                hit
            }
            None => {
                let fresh = RowText {
                    revision: self.revision,
                    range: (byte_start, byte_end),
                    in_fence: in_fence || indented_code,
                    spans: if placeholder {
                        std::rc::Rc::from([] as [markdown::Span; 0])
                    } else {
                        markdown::highlight_line(raw, in_fence || indented_code).into()
                    },
                    display: if placeholder {
                        "Start typing…".into()
                    } else if raw.is_empty() {
                        // An empty row still needs something to shape, or it
                        // collapses.
                        " ".into()
                    } else {
                        raw.to_string().into()
                    },
                };
                // Only visible rows are ever asked for, so this holds a
                // screenful — until a long scroll, which is what the cap is for.
                if self.row_cache.len() > ROW_CACHE_CAP {
                    self.row_cache.clear();
                }
                self.row_cache.insert(line, fresh.clone());
                fresh
            }
        };

        let selection = self.selected_range.clone();
        let selection_local = {
            let lo = selection.start.clamp(byte_start, byte_end) - byte_start;
            let mut hi = selection.end.clamp(byte_start, byte_end) - byte_start;
            // A selection that runs past this line covers its newline; show that
            // as a sliver so selecting blank lines is visible.
            if selection.end > byte_end && selection.start <= byte_end && !selection.is_empty() {
                hi = display.len();
            }
            lo.min(display.len())..hi.min(display.len())
        };
        // While the find bar holds the keyboard the caret belongs to the query
        // field, not here. A blurred bar leaves the note's caret on screen.
        let caret_local = (self.caret_alpha > 0.
            && !self.find_focused
            && selection.is_empty()
            && self.cursor_offset() >= byte_start
            && self.cursor_offset() <= byte_end)
            .then(|| (self.cursor_offset() - byte_start).min(display.len()));
        // The grapheme after the caret, which is how a wrap reveals itself once
        // the row is measured.
        let caret_next_local = caret_local
            .map(|local| {
                index::next_grapheme(self.note.text(), &self.index, byte_start + local) - byte_start
            })
            .unwrap_or_default()
            .min(display.len());

        // Headings render larger, which variable-height rows now allow. The
        // level is found anywhere in the line, not just at the first span: a
        // quoted heading ("> # Title") leads with the QuoteMarker, so reading
        // only `spans.first()` left it at body size. A HeadingMarker can only
        // appear at the head of a line or just past a quote sigil, so scanning
        // for it never mistakes ordinary text for a heading.
        let heading = spans
            .iter()
            .find_map(|span| match span.style {
                MdStyle::Heading(level) | MdStyle::HeadingMarker(level) => Some(level),
                _ => None,
            })
            // ...or a run of `=` on the line below, which is the other way
            // markdown writes a heading. It used to render as literal text with
            // the title above it at body size, which is the one shape a
            // markdown file can have that this app did not draw at all.
            .or_else(|| {
                if in_code_block {
                    // A row of `=` under a line of code is a divider in the
                    // code, not a heading.
                    return None;
                }
                let below = self.index.line_range(line + 1)?;
                let below = &self.note.text()[below.0..below.1];
                (markdown::setext_level(below).is_some() && markdown::takes_setext_underline(raw))
                    .then_some(1)
            });
        // The underline itself is markup, not content: dim, like a `#` sigil.
        let is_underline = !in_code_block
            && markdown::setext_level(raw).is_some()
            && line
                .checked_sub(1)
                .and_then(|above| self.index.line_range(above))
                .is_some_and(|(a, b)| markdown::takes_setext_underline(&self.note.text()[a..b]));

        let hide_text = image.is_some() && !reveal;
        let default_color: Hsla = if is_underline {
            rgb(theme().marker).into()
        } else if placeholder {
            rgb(theme().placeholder).into()
        } else if is_separator || hide_text {
            // The rule *is* the separator. Its `---` text stays present and
            // editable but invisible, so the line reads as one unbroken stroke.
            //
            // An image line keeps its text for exactly the same reason, and it
            // is not decoration: the row has to appear in `line_layouts` or the
            // caret cannot be drawn on it, ↑/↓ lose the column they were aiming
            // for, ⌘←/⌘→ fall back to whole lines, the IME has nowhere to put a
            // candidate window, and the scroll thumb loses its sub-line
            // fraction. Shaping the reference invisibly costs one short line and
            // buys all of that back.
            gpui::transparent_black()
        } else {
            rgb(theme().fg).into()
        };
        // A rule and a hidden image reference are shaped invisibly, so they
        // carry no styling of their own.
        let spans: &[markdown::Span] = if is_separator || hide_text || is_underline {
            &[]
        } else {
            &spans
        };
        // Search hits wash the line under the selection, converted to
        // line-local offsets. Only the visible lines are asked for.
        let mut washes: Vec<(Range<usize>, Hsla)> = Vec::new();
        // A hidden reference has no glyphs to mark, but it still has a width,
        // so a search for "png" or a hash used to paint a yellow band on the
        // paper beside the picture. Same reason the selection wash is skipped.
        if let Some(find) = self.find.as_ref().filter(|_| !hide_text) {
            let current = find.current();
            for hit in find.overlapping(byte_start..byte_end) {
                let local = hit.start.clamp(byte_start, byte_end) - byte_start
                    ..hit.end.clamp(byte_start, byte_end) - byte_start;
                let colour = if current.as_ref() == Some(&hit) {
                    theme().find_current
                } else {
                    theme().find_match
                };
                washes.push((local, rgb(colour).into()));
            }
        }
        // While the find bar holds the keyboard the "selection" is find's own
        // marker on the current hit. Painting it here would cover that hit in
        // selection blue and leave it the least remarkable thing on screen, with
        // its neighbours glowing. A blurred bar hands the note its selection back.
        // A hidden reference has no glyphs to wash, but it still has a *width* —
        // so selecting an image painted a blue band across the paper beside it,
        // tracing text nobody can see. The frame around the picture is what says
        // the picture is selected.
        if !self.find_focused && !hide_text {
            // macOS drains the colour from a selection when its window is not the
            // active one; a saturated blue on a background window competes for
            // attention it should not have. Cheap: one comparison per row.
            let colour = if window.is_window_active() {
                theme().selection
            } else {
                theme().selection_inactive
            };
            washes.push((selection_local, rgb(colour).into()));
        }

        // Inline-code chips are painted behind the text, tight to the glyph
        // band, rather than as full-height run backgrounds (see CodeUnderlay).
        // Their line-local ranges come from the same spans the runs do.
        let glass = self.settings.glass != settings::GlassStyle::Identity;
        let code_chip_alpha = if theme::is_dark() { 0.62 } else { 0.68 };
        let code_block_alpha = if theme::is_dark() { 0.46 } else { 0.52 };
        let chips: Vec<(Range<usize>, Hsla)> = spans
            .iter()
            .filter(|s| s.end <= display.len())
            .filter_map(|s| match s.style {
                MdStyle::Code => Some((
                    s.start..s.end,
                    code_surface(theme().code_bg, glass, code_chip_alpha),
                )),
                MdStyle::Highlight => Some((s.start..s.end, rgb(theme().highlight_bg).into())),
                _ => None,
            })
            .collect();

        let runs = build_runs(
            &display,
            spans,
            &window.text_style().font(),
            default_color,
            &washes,
        );

        let text = StyledText::new(display).with_runs(runs);
        let overlay = RowOverlay {
            byte_start,
            byte_end,
            line,
            layout: text.layout().clone(),
            caret: caret_local.map(|local| (local, caret_next_local, self.caret.affinity)),
            caret_alpha: self.caret_alpha,
            app: cx.entity().clone(),
        };

        let base = self.settings.text_size;
        let code_underlay = CodeUnderlay {
            line,
            layout: text.layout().clone(),
            chips,
        };
        // Nesting that is only in the punctuation is not nesting anyone can
        // see: `> > inner` sat at the same margin as the quote around it. One
        // level of indent per sigil, past the first — which is where a quote
        // already sits.
        let quote_indent = if in_code_block {
            // `>>>` is a Python prompt, not three levels of quotation.
            0.
        } else {
            markdown::quote_depth(raw).saturating_sub(1) as f32 * base * 1.2
        };
        let row = div()
            .id(ElementId::Name(format!("row-{line}").into()))
            .flex()
            .flex_row()
            .items_start()
            .w_full()
            .px(row_bleed(base))
            .when(quote_indent > 0., |d| d.pl(px(quote_indent) + row_bleed(base)))
            .when(in_code_block, |d| {
                d.bg(code_surface(
                    theme().codeblock_bg,
                    glass,
                    code_block_alpha,
                ))
            })
            .when_some(heading, |d, level| {
                let (size, space_above) = heading_scale(level);
                // Only pad a heading that follows text. The first line needs no
                // gap, and neither does one you already put a blank line above —
                // otherwise the space depends on your whitespace habits, which
                // is exactly what this app promises it does not do.
                // A rule is not text to be spaced away from: it carries its own
                // air now, and adding the heading's on top double-counts it.
                let follows_text = line > 0
                    && self.index.line_range(line - 1).is_some_and(|(a, b)| {
                        let above = &self.note.text()[a..b];
                        !above.trim().is_empty() && !markdown::is_separator(above)
                    });
                d.text_size(px(base * size)).pt(px(if follows_text {
                    base * LINE_HEIGHT * space_above
                } else {
                    0.
                }))
            });

        // The space above the first line. It belongs to row 0 rather than to the
        // list, because the list's own top padding is a fixed band that never
        // scrolls (see the `list` call in `render`). Wrapped around the row, not
        // padded into it, so nothing the row paints — a code block's background,
        // a separator's rule — spills into the empty space above the note. While
        // find is open the gap under its hairline plays this part instead.
        let lead = (line == 0 && self.find.is_none())
            .then(|| top_pad(base, window.viewport_size().height));
        let with_lead = |row: AnyElement| match lead {
            // `w_full` is not decoration. The list measures each item as a
            // layout root at the list's width; when the item root was the row
            // itself, its own `w_full` resolved against that. Wrapping it in an
            // auto-width column made the wrapper the root, so `w_full` inside
            // resolved against an indefinite parent, and GPUI's text measure
            // returns the *unwrapped* width when it has no wrap width — line 0
            // stopped soft-wrapping and ran off the right edge.
            Some(pad) => div()
                .flex()
                .flex_col()
                .w_full()
                .child(div().h(pad))
                .child(row)
                .into_any_element(),
            None => row,
        };

        if let Some(reference) = image {
            // "The picture is selected" means the selection covers its whole
            // reference — which is exactly what clicking the picture does. That
            // one fact gives copy, cut, delete and replace for nothing: they are
            // the ordinary commands acting on an ordinary selection.
            let selected = !self.selected_range.is_empty()
                && self.selected_range.start <= byte_start
                && self.selected_range.end >= byte_end;
            return with_lead(self.render_image_row(
                line, &reference, reveal, selected, byte_start, byte_end, row, text, overlay,
                window, cx,
            ));
        }

        if is_separator {
            with_lead(
                row.relative()
                // The rule occupies exactly one ordinary row — no extra air. Typing
                // `---` to turn a line into a separator must not change its height
                // or nudge the rows around it; the transform is invisible in the
                // layout, only in the ink. The hairline is centred in that one row.
                // One unbroken hairline across the measure, vertically centred
                // behind everything else.
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        // Inset by the row's bleed, so the rule stays flush
                        // with the text rather than running past it.
                        .left(row_bleed(base))
                        // Leave a clean gap for the bare promote arrow. The
                        // button only gains a circular surface while hovered.
                        .right(px(base * 1.9))
                        .flex()
                        .items_center()
                        .child(div().w_full().h(px(1.)).bg(rgb(theme().rule))),
                )
                .items_center()
                .child(div().flex_1().min_w_0().child(text).child(overlay))
                .child(
                    div()
                        .id(ElementId::Name(format!("promote-{line}").into()))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(base * 1.5))
                        .rounded_full()
                        .text_size(px(base * 0.73))
                        .text_color(rgb(theme().btn_fg))
                        .cursor_pointer()
                        // Quiet until pointed at: a soft chip and the accent,
                        // so it is unmistakably a control without shouting on a
                        // page that is otherwise only text.
                        .hover(|s| s.bg(rgb(theme().btn_bg_hover)).text_color(rgb(theme().btn_fg_hover)))
                        // The caret glyph's ink sits high in its em box, so
                        // centring the box leaves it floating above the rule.
                        .child(div().mt(px(base * 0.2)).child("^"))
                        // A macOS control fires on mouse-*up*, and cancels if you
                        // press it then drag off — so this is `on_click`, not
                        // `on_mouse_down`. The bare mouse-down handler only stops
                        // the press reaching the note body, so pressing the chip
                        // never plants a caret in the rule behind it.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.promote_block_below_separator(line, cx);
                        })),
                )
                .into_any_element(),
            )
        } else {
            // `min_w_0` lets the text shrink below its intrinsic width, which is
            // what allows it to wrap instead of overflowing the window. The code
            // underlay is first, so its chips paint *behind* the glyphs; the
            // caret overlay is last, so it paints on top.
            with_lead(
                row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(code_underlay)
                        .child(text)
                        .child(overlay),
                )
                .into_any_element(),
            )
        }
    }
}

/// Hand something to `open(1)`, and wait for it somewhere else.
///
/// `open` exits as soon as it has told Launch Services what to do, and nothing
/// was waiting for it — so every ⌘-click on a link and every Look Up left a
/// defunct process behind for the rest of the session.
fn open_with_finder(target: &str) {
    let target = target.to_string();
    std::thread::spawn(move || {
        if let Ok(mut child) = std::process::Command::new("open").arg(target).spawn() {
            let _ = child.wait();
        }
    });
}

/// Whether a selection is something a URL can be wrapped around: one line of
/// ordinary words.
///
/// Not markup — a selection holding brackets or parentheses may already be a
/// link, and nesting one inside another produces something no parser reads —
/// and not a URL itself, where `[https://a](https://b)` is nobody's intent. The
/// selection goes in verbatim: trimming it would drop characters from inside the
/// range being replaced.
fn is_link_text(selected: &str) -> bool {
    !selected.is_empty()
        && !selected.contains('\n')
        && !selected.trim().is_empty()
        && !selected.contains(['[', ']', '(', ')'])
        && !is_bare_url(selected)
}

/// Whether `text` is one bare URL and nothing else — the shape a paste can turn
/// into a link without having to guess.
fn is_bare_url(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && !trimmed.contains(char::is_whitespace)
        && (trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || trimmed.starts_with("mailto:"))
}

/// `text` cut to `room` characters, with an ellipsis standing for the middle —
/// the end of a file name says more than the middle of it.
fn ellipsise(text: &str, room: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= room || room < 4 {
        return chars.into_iter().take(room).collect();
    }
    let tail = (room - 1) / 2;
    let head = room - 1 - tail;
    chars[..head]
        .iter()
        .chain(std::iter::once(&'…'))
        .chain(&chars[chars.len() - tail..])
        .collect()
}

/// A file size the way the Finder says it: never "0.0 MB" for a real file.
fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..=999 => format!("{bytes} bytes"),
        1_000..=999_999 => format!("{:.0} KB", bytes as f32 / 1_000.),
        _ => format!("{:.1} MB", bytes as f32 / 1_000_000.),
    }
}

/// Convert a UTF-16 unit offset into a UTF-8 byte offset **within** `s` only.
fn utf16_offset_in(s: &str, utf16_units: usize) -> usize {
    let mut u16c = 0usize;
    let mut byte = 0usize;
    for ch in s.chars() {
        if u16c >= utf16_units {
            break;
        }
        u16c += ch.len_utf16();
        byte += ch.len_utf8();
    }
    byte
}

/// Build shaping runs for one line from its markdown spans, then paint the
/// selection behind `selection` by splitting runs at its edges.
///
/// [`markdown::highlight_line`] guarantees the spans exactly tile the line, but
/// this validates anyway: the displayed string is sometimes a stand-in (a space
/// for an empty row, or placeholder copy), and a mismatch must degrade to plain
/// text rather than panic inside the text system.
/// The faces the text system reaches for when Lilex has no glyph.
///
/// Lilex is a mono face with no emoji and no CJK, so a note with either in it
/// was at the mercy of whatever the shaper picked per run — which is how one
/// emoji drew as a box and the one beside it drew as a picture. Naming the
/// fallbacks makes it the same face every time.
/// Built once and cloned — it is an `Arc` inside, and this is called for every
/// styled run of every visible row on every frame.
fn text_fallbacks() -> gpui::FontFallbacks {
    static FALLBACKS: std::sync::OnceLock<gpui::FontFallbacks> = std::sync::OnceLock::new();
    FALLBACKS
        .get_or_init(|| {
            gpui::FontFallbacks::from_fonts(vec![
                "Apple Color Emoji".into(),
                "PingFang SC".into(),
                "Hiragino Sans".into(),
                "Apple SD Gothic Neo".into(),
                ".AppleSystemUIFont".into(),
            ])
        })
        .clone()
}

fn build_runs(
    display: &str,
    spans: &[markdown::Span],
    base_font: &Font,
    default_color: Hsla,
    washes: &[(Range<usize>, Hsla)],
) -> Vec<TextRun> {
    let plain = || {
        let mut font = base_font.clone();
        font.fallbacks = Some(text_fallbacks());
        vec![TextRun {
            len: display.len(),
            font,
            color: default_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }]
    };

    let tiles = !spans.is_empty()
        && spans[0].start == 0
        && spans.last().map(|s| s.end) == Some(display.len())
        && spans.windows(2).all(|w| w[0].end == w[1].start)
        && spans.iter().all(|s| {
            s.start < s.end && display.is_char_boundary(s.start) && display.is_char_boundary(s.end)
        });

    let runs = if tiles {
        spans
            .iter()
            .map(|span| {
                let attrs = attrs_for(span.style);
                let mut font = base_font.clone();
                font.weight = attrs.weight;
                font.style = attrs.style;
                font.fallbacks = Some(text_fallbacks());
                TextRun {
                    len: span.end - span.start,
                    font,
                    color: attrs.color_hsla(),
                    // Inline code and fenced blocks have dedicated underlays.
                    // A `TextRun` background would add an opaque glyph-width
                    // band over those surfaces; selection/find washes remain
                    // run backgrounds so their ranges stay exact.
                    background_color: if matches!(
                        span.style,
                        MdStyle::Code | MdStyle::CodeBlock | MdStyle::Fence
                    ) {
                        None
                    } else {
                        attrs.background_hsla()
                    },
                    underline: attrs.underline.then(|| UnderlineStyle {
                        thickness: px(1.),
                        color: Some(attrs.color_hsla()),
                        wavy: false,
                    }),
                    strikethrough: attrs.strikethrough.then(|| StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(attrs.color_hsla()),
                    }),
                }
            })
            .collect()
    } else {
        plain()
    };

    washes.iter().fold(runs, |runs, (range, colour)| {
        apply_wash(runs, range.clone(), *colour)
    })
}

/// Split runs at `range`'s edges and wash the covered pieces.
///
/// Painting selection and search hits as run backgrounds rather than quads
/// means they follow soft-wrapped text across visual rows for free.
fn apply_wash(runs: Vec<TextRun>, range: Range<usize>, wash: Hsla) -> Vec<TextRun> {
    let selection = range;
    if selection.is_empty() {
        return runs;
    }
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len() + 2);
    let mut pos = 0usize;
    for run in runs {
        let end = pos + run.len;
        let mut cuts = vec![pos, end];
        for edge in [selection.start, selection.end] {
            if edge > pos && edge < end {
                cuts.push(edge);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let mut piece = run.clone();
            piece.len = to - from;
            if from >= selection.start && to <= selection.end {
                piece.background_color = Some(wash);
            }
            out.push(piece);
        }
        pos = end;
    }
    out
}

// ── EntityInputHandler (IME / system text input) ─────────────────────────────

impl EntityInputHandler for NoteApp {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        let (a, b) = self.note.clamp_range(range.start, range.end);
        actual_range.replace(self.range_to_utf16(&(a..b)));
        Some(self.note.text()[a..b].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        // While the find bar owns the keyboard the composition lives in the
        // query, so clear it there rather than on the note. A blurred bar's
        // composition belongs to the note.
        if self.find_focused {
            if let Some(find) = self.find.as_mut() {
                find.unmark();
            }
        } else {
            self.marked_range = None;
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The palette borrows the same single input path the find bar does, and
        // it has to be asked *before* the modal guard: the palette is itself
        // modal, so the guard below would swallow every keystroke meant for it.
        if let Some(palette) = self.palette.as_mut() {
            if !new_text.is_empty() && !new_text.contains('\n') {
                palette.query.push_str(new_text);
                palette.selected = 0;
                cx.notify();
            }
            return;
        }
        // The Settings panel is modal: no text reaches the note while it is open.
        if self.modal_open() {
            return;
        }
        // The find bar is the app's only other text field, and it borrows this
        // one input path rather than owning a second focus handle and IME — but
        // only while it holds the keyboard. A blurred bar routes typing to the note.
        if self.find_focused {
            if !new_text.is_empty() && !new_text.contains('\n') {
                // Typed text lands at the query caret, replacing the selection
                // (the offered query on the first keystroke). The full-document
                // scan is debounced; only the field redraws now.
                if let Some(find) = self.find.as_mut() {
                    find.insert(new_text);
                }
                self.schedule_query_search(cx);
            }
            return;
        }

        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let (start, end) = self.note.clamp_range(range.start, range.end);
        // Kept so the index, fence map and list heights can be patched instead
        // of rebuilt, and so undo has the bytes to put back. Usually a
        // character or two.
        let removed = self.note.text()[start..end].to_string();
        let before = (self.selected_range.start, self.selected_range.end);

        self.note.replace_range(start, end, new_text);
        let cursor = start + new_text.len();
        self.selected_range = cursor..cursor;
        // An edit is a caret move like any other: the column ↑/↓ were aiming at
        // is not the column you are in now. This is the path ordinary typing
        // takes, and the one the deletes that pass an explicit range take.
        self.reset_row_motion();
        self.marked_range = None;
        self.history.record(
            Edit {
                start,
                removed: removed.clone(),
                inserted: new_text.to_string(),
                before,
                after: (cursor, cursor),
            },
            Instant::now(),
        );
        self.after_edit(start, &removed, new_text);
        self.follow_caret();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A composition in progress — a dead key, an accent popup, any CJK
        // input method — belongs to the find query while the bar has the
        // keyboard, not the note behind it. Route the preedit into the query so
        // accented and CJK search terms can be typed; the commit arrives through
        // `replace_text_in_range`, which consumes the marked run. Only while the
        // bar holds the keyboard — a blurred bar composes into the note.
        if self.modal_open() {
            return;
        }
        if self.find_focused {
            if let Some(find) = self.find.as_mut() {
                if new_text.is_empty() {
                    find.unmark();
                } else {
                    find.compose(new_text);
                }
            }
            self.schedule_query_search(cx);
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let (start, end) = self.note.clamp_range(range.start, range.end);
        // Kept so the index, fence map and list heights can be patched instead
        // of rebuilt. Usually a character or two.
        let removed = self.note.text()[start..end].to_string();
        let before = (self.selected_range.start, self.selected_range.end);

        self.note.replace_range(start, end, new_text);
        if !new_text.is_empty() {
            self.marked_range = Some(start..start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // IME selection is relative to the replacement string, not the document.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| {
                let a = utf16_offset_in(new_text, r.start);
                let b = utf16_offset_in(new_text, r.end);
                start + a..start + b
            })
            .unwrap_or_else(|| start + new_text.len()..start + new_text.len());
        // Marked text must go into the history too. Without it an IME
        // composition (CJK, or ⌥e accents) is invisible to undo, and — worse —
        // a later undo computes its inverse against offsets that silently moved,
        // trips the desync guard, and clears the whole undo stack.
        //
        // Each composition step is its own group, so undoing a composed
        // character walks back through the composition rather than dropping it
        // in one go. That is more granular than AppKit, and deliberately so:
        // collapsing the steps would mean reconstructing the composition's
        // start state, and being correct here matters more than being terse.
        self.history.record(
            Edit {
                start,
                removed: removed.clone(),
                inserted: new_text.to_string(),
                before,
                after: (self.selected_range.start, self.selected_range.end),
            },
            Instant::now(),
        );
        self.after_edit(start, &removed, new_text);
        self.follow_caret();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let (start, end) = self.note.clamp_range(range.start, range.end);
        let row = self.line_layouts.get(&self.index.line_at(start))?;
        if start < row.byte_start || start > row.byte_end {
            return None;
        }
        let from = row.geometry.position_for_index(start - row.byte_start)?;
        let to = row
            .geometry
            .position_for_index(end.min(row.byte_end) - row.byte_start)
            .unwrap_or(from);
        let line_height = row.geometry.line_height();
        Some(Bounds::from_corners(
            from,
            point(to.x.max(from.x), to.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.index_for_mouse_position(point);
        Some(self.index.to_utf16(self.note.text(), offset))
    }
}

impl Focusable for NoteApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── Render ───────────────────────────────────────────────────────────────────

impl Render for NoteApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Open the frame for the image bank, which is also where it makes room
        // if the last frame left too much decoded. Here, at the top of `render`,
        // is *after* the previous frame was painted — and nothing the previous
        // frame drew is touched, because dropping an image hands its slot back
        // to the sprite atlas and the atlas recycles slots at once.
        self.bank.begin_frame(window);

        // How many rows were actually inside the viewport last frame. Read
        // before the layouts are dropped, and before the list replaces them.
        let visible = match self.viewport {
            Some(text) => self
                .line_layouts
                .values()
                .filter(|row| {
                    let row = row.geometry.bounds();
                    row.bottom() > text.top() && row.top() < text.bottom()
                })
                .count()
                .max(1),
            // First frame — and the frame right after a resize, before the
            // canvas re-reports its bounds — has no measured viewport. Estimate
            // the screenful from the window height and the row height so the
            // thumb opens at roughly the right size, instead of the 1/total
            // sliver a hard-coded 1 would draw until a second frame measured it.
            None => {
                let row_h = self.settings.text_size * LINE_HEIGHT;
                (f32::from(window.viewport_size().height) / row_h).floor().max(1.) as usize
            }
        };
        // Shared with the scrollbar drag/paging math, so the thumb it draws and
        // the thumb a drag moves are computed from the same screenful count.
        self.visible_lines = visible;

        // Position indicator: with 200,000 lines you otherwise have no idea
        // where you are. Sized and placed in lines, not pixels — the list only
        // measures the visible rows, so a true pixel extent is not on offer
        // without shaping the whole document. See [`Self::scroll_metrics`], which
        // this also feeds so a click or drag on the rail lands where the thumb
        // says. Computed before the layouts are dropped, so the sub-line offset
        // that smooths the thumb is still available.
        let scroll_thumb = self.scroll_metrics().map(|m| (m.top, m.height));

        // Only the visible rows will report bounds; stale entries would make
        // hit-testing point at lines that are no longer on screen.
        self.line_layouts.clear();
        // Where the matches are in the note as a whole. On a document this long
        // "412 of 900" says nothing about whether the hits are all in one week
        // or spread over ten years; a tick per match on the rail does.
        let match_ticks = self.match_rail.clone();

        // Light or dark, resolved before a single colour is read this frame:
        // the preference, or — on System — what the Mac is set to right now,
        // which GPUI reports per window and updates when it changes.
        theme::set_dark(match self.settings.appearance {
            settings::Appearance::Light => false,
            settings::Appearance::Dark => true,
            settings::Appearance::System => matches!(
                window.appearance(),
                gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
            ),
        });
        let focus = self.focus_handle.clone();
        // Whether the traffic lights get the whole width of the strip, or only
        // their own corner of it.
        let wide_lights = self.find.is_none()
            && lights_are_wide(self.settings.text_size, window.viewport_size().height);

        div()
            .id("note-root")
            // The find bar owns the keyboard while it is open: switching the
            // context means the note's own bindings stop dispatching by
            // construction, rather than by a guard in every command.
            // The Settings panel is modal: while it is open no note key binding
            // dispatches, so typing and commands cannot reach the note behind it.
            .key_context(if self.palette.is_some() {
                "Palette"
            } else if self.settings_open || self.backups_open {
                "Settings"
            } else if self.find_focused {
                "Find"
            } else {
                "Note"
            })
            .track_focus(&focus)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            // The reference material is the window background. Identity is
            // the sole no-effect mode, so it keeps the ordinary paper fill.
            .when(self.settings.glass == settings::GlassStyle::Identity, |d| {
                d.bg(rgb(theme().bg))
            })
            .text_color(rgb(theme().fg))
            .font_family("Lilex")
            .text_size(px(self.settings.text_size))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::new_note))
            .on_action(cx.listener(Self::bring_note_up))
            .on_action(cx.listener(Self::move_note_up))
            .on_action(cx.listener(Self::backup_now))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::toggle_task))
            .on_action(cx.listener(Self::toggle_done))
            .on_action(cx.listener(Self::toggle_open_at_login))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::toggle_window))
            .on_action(cx.listener(Self::minimize))
            .on_action(cx.listener(Self::zoom_window))
            .on_action(cx.listener(Self::toggle_full_screen))
            .on_action(cx.listener(Self::hide_app))
            .on_action(cx.listener(Self::hide_others))
            .on_action(cx.listener(Self::show_all_apps))
            .on_action(cx.listener(Self::about_app))
            .on_action(cx.listener(Self::reveal_in_finder))
            .on_action(cx.listener(Self::export_note))
            .on_action(cx.listener(Self::open_backups))
            .on_action(cx.listener(Self::reclaim_images))
            .on_action(cx.listener(Self::choose_note_folder))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::palette_step))
            .on_action(cx.listener(Self::palette_step_back))
            .on_action(cx.listener(Self::palette_run))
            .on_action(cx.listener(Self::show_help))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::paragraph_up))
            .on_action(cx.listener(Self::paragraph_down))
            .on_action(cx.listener(Self::select_paragraph_up))
            .on_action(cx.listener(Self::select_paragraph_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::document_start))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_to_document_start))
            .on_action(cx.listener(Self::select_to_document_end))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_back))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::kill_to_end))
            .on_action(cx.listener(Self::yank))
            .on_action(cx.listener(Self::open_line))
            .on_action(cx.listener(Self::center_caret))
            .on_action(cx.listener(Self::transpose))
            .on_action(cx.listener(Self::move_line_up))
            .on_action(cx.listener(Self::move_line_down))
            .on_action(cx.listener(Self::duplicate_lines))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::text_bigger))
            .on_action(cx.listener(Self::text_smaller))
            .on_action(cx.listener(Self::text_size_reset))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::find_char_left))
            .on_action(cx.listener(Self::find_char_right))
            .on_action(cx.listener(Self::find_word_left))
            .on_action(cx.listener(Self::find_word_right))
            .on_action(cx.listener(Self::find_line_start))
            .on_action(cx.listener(Self::find_line_end))
            .on_action(cx.listener(Self::find_select_char_left))
            .on_action(cx.listener(Self::find_select_char_right))
            .on_action(cx.listener(Self::find_select_word_left))
            .on_action(cx.listener(Self::find_select_word_right))
            .on_action(cx.listener(Self::find_select_line_start))
            .on_action(cx.listener(Self::find_select_line_end))
            .on_action(cx.listener(Self::find_delete_forward))
            .on_action(cx.listener(Self::find_delete_word_back))
            .on_action(cx.listener(Self::find_delete_word_forward))
            .on_action(cx.listener(Self::find_delete_to_start))
            .on_action(cx.listener(Self::find_focus_next_field))
            .on_action(cx.listener(Self::toggle_replace))
            .on_action(cx.listener(Self::replace_and_find))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::use_selection_for_find))
            .on_action(cx.listener(Self::jump_to_selection))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete_key))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            // Files dropped onto the window insert at the caret.
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_files(paths, window, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // The I-beam is set on the reading column below, not the whole window,
            // so the wide empty margins beside a centred measure show an arrow —
            // an I-beam there implied text that isn't there.
            // No header, no title bar: the window is a sheet of paper and the
            // note is the only thing on it. Everything that used to be chrome
            // now lives in the menu bar.
            //
            // Find is the exception, and it sits at the top: it is a question
            // about the note, so it belongs above the answer, and the eye is
            // already there when you press ⌘F.
            .when_some(self.find.as_ref(), |d, find| {
                d.child(self.render_find_bar(find, window.viewport_size().width, cx))
            })
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_row()
                            .justify_center()
                            .child(
                                div()
                                    .w_full()
                                    .max_w(reading_width(self.settings.text_size))
                                    .h_full()
                                    .cursor(gpui::CursorStyle::IBeam)
                                    .px(column_inset(self.settings.text_size))
                                    // The first and last lines' breathing room:
                                    // padding on the column that holds the list,
                                    // Relative, so headings keep their leading
                                    // in proportion as they scale up.
                                    .line_height(relative(LINE_HEIGHT))
                                    .relative()
                                    .child(
                                        list(
                                            self.list_state.clone(),
                                            cx.processor(|this, line: usize, window, cx| {
                                                this.render_row(line, window, cx)
                                            }),
                                        )
                                        .size_full()
                                        // The last line's breathing room is the
                                        // list's own bottom padding: GPUI adds it
                                        // to the scroll extent, so it is room you
                                        // scroll *into* and the text still reaches
                                        // the true window edge on the way.
                                        //
                                        // Top padding does not behave that way.
                                        // `prepaint_items` offsets the first
                                        // visible item by `padding.top` whatever
                                        // the scroll position, so it is a fixed
                                        // band across the top of the window — the
                                        // bar this used to draw. The first line's
                                        // breathing room rides on row 0 instead
                                        // (see `render_row`), which scrolls away
                                        // with it. What is left here is the gap
                                        // under the find bar's hairline, which
                                        // *should* stay put: the find bar is a
                                        // real element above the list, and the
                                        // note must not run into it.
                                        .pt(if self.find.is_some() {
                                            find_bar_gap(self.settings.text_size)
                                        } else {
                                            px(0.)
                                        })
                                        // The same air the first line gets, so
                                        // the page is not padded differently at
                                        // its two ends: scrolling from one to
                                        // the other should not show the margin
                                        // change size.
                                        .pb(top_pad(
                                            self.settings.text_size,
                                            window.viewport_size().height,
                                        )),
                                    )
                                    // Records where the text ends up on screen,
                                    // and listens for drags that leave it. An
                                    // element's own `on_mouse_move` only fires
                                    // while the pointer is inside it, which is
                                    // precisely not the case being handled.
                                    // Paints nothing.
                                    .child(
                                        canvas(
                                            cx.processor(|this, bounds, _window, _cx| {
                                                this.viewport = Some(bounds);
                                            }),
                                            {
                                                let app = cx.entity();
                                                move |_, _, window, cx| {
                                                    window.on_mouse_event({
                                                        let app = app.clone();
                                                        move |event: &MouseMoveEvent,
                                                              phase,
                                                              _window,
                                                              cx| {
                                                            if phase == DispatchPhase::Bubble {
                                                                app.update(cx, |this, cx| {
                                                                    this.on_drag_move(event, cx)
                                                                });
                                                            }
                                                        }
                                                    });
                                                    // Leaving through the top edge — the usual way
                                                    // out, towards the menu bar — sends no move
                                                    // event, so `on_hover` never reports the leave
                                                    // and the traffic lights would stay on screen.
                                                    window.on_mouse_event({
                                                        let app = app.clone();
                                                        move |_: &MouseExitEvent, phase, _window, cx| {
                                                            if phase == DispatchPhase::Bubble {
                                                                app.update(cx, |this, cx| {
                                                                    this.set_window_buttons_visible(
                                                                        false, cx,
                                                                    )
                                                                });
                                                            }
                                                        }
                                                    });
                                                    let _ = cx;
                                                }
                                            },
                                        )
                                        // Pinned to the container: an absolute
                                        // element with no inset keeps its
                                        // static position, which here is below
                                        // the list rather than over it.
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .size_full(),
                                    ),
                            ),
                    )
                    .when(scroll_thumb.is_some() || !match_ticks.is_empty(), |d| {
                        d.child(
                            div()
                                .id("scroll-rail")
                                .absolute()
                                // The rail starts where the text does, or the
                                // thumb is already part-way down a track whose
                                // top holds nothing.
                                .top(if self.find.is_some() {
                                    find_bar_gap(self.settings.text_size)
                                } else {
                                    top_pad(
                                        self.settings.text_size,
                                        window.viewport_size().height,
                                    )
                                })
                                .bottom(top_pad(
                                    self.settings.text_size,
                                    window.viewport_size().height,
                                ))
                                .right(px(6.))
                                .w(px(6.))
                                // The rail is a readout, not text: an arrow, not
                                // an I-beam, and a click on it must not fall
                                // through to place the caret in the text behind.
                                .cursor(gpui::CursorStyle::Arrow)
                                .occlude()
                                // A click on the track, off the thumb, pages the
                                // view toward it. The thumb and the ticks stop
                                // their own presses, so only bare-track clicks
                                // reach here.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(Self::on_scroll_rail_down),
                                )
                                // The rail's measured bounds, so a thumb drag and
                                // a track click can map a pointer onto the note.
                                // Paints nothing.
                                .child(
                                    canvas(
                                        cx.processor(|this, bounds, _w, _cx| {
                                            this.scroll_rail = Some(bounds);
                                        }),
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full(),
                                )
                                // Ticks on the left of the rail, thumb on the
                                // right. Sharing the width meant a match painted
                                // over the thumb — hiding where you are exactly
                                // when a search is what you are doing.
                                .when_some(scroll_thumb, |rail, (top, height)| {
                                    rail.child(
                                        div()
                                            .id("scroll-thumb")
                                            .absolute()
                                            .right_0()
                                            .w(px(3.))
                                            .top(relative(top))
                                            .h(relative(height))
                                            .rounded_full()
                                            .bg(rgb(theme().scroll_thumb))
                                            .cursor(gpui::CursorStyle::Arrow)
                                            // Widen and darken under the pointer,
                                            // the way a macOS overlay thumb thickens
                                            // to invite the drag.
                                            .hover(|s| s.w(px(6.)).bg(rgb(theme().find_clear_bg)))
                                            // Grab the thumb to drag it. The press
                                            // records where in the thumb it landed
                                            // so the thumb tracks the pointer
                                            // without jumping; the window-level
                                            // move listener does the scrolling.
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                                                    cx.stop_propagation();
                                                    if let Some(rail) = this.scroll_rail {
                                                        let thumb_top = rail.top()
                                                            + rail.size.height * top;
                                                        this.scroll_drag =
                                                            Some(e.position.y - thumb_top);
                                                        this.list_state.scrollbar_drag_started();
                                                    }
                                                }),
                                            ),
                                    )
                                })
                                .children(match_ticks.into_iter().map(|tick| {
                                    // The visible mark is a hair tall, so the
                                    // clickable target is the full rail width
                                    // and a little taller — clicking it jumps to
                                    // the nearest hit in that region. The current
                                    // match wears the same colour it does in the
                                    // body, so the eye finds it on the rail too.
                                    let colour = if tick.current { theme().find_current } else { theme().find_tick };
                                    let index = tick.match_index;
                                    div()
                                        .id(("match-tick", index))
                                        .absolute()
                                        .left_0()
                                        .w(px(6.))
                                        .top(relative(tick.at))
                                        .h(px(4.))
                                        .flex()
                                        .items_center()
                                        .cursor(gpui::CursorStyle::PointingHand)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.jump_to_match(index, cx);
                                            }),
                                        )
                                        .child(
                                            div()
                                                .w(px(3.))
                                                .h(px(2.))
                                                .bg(rgb(colour)),
                                        )
                                })),
                        )
                    }),
            )
            // The bar the traffic lights sit on, and above it the strip that
            // brings them back. Both come *after* the note, and the order of
            // these two among themselves is the whole trick.
            //
            // The bar covers the note: the first line starts inside the strip,
            // so a bar painted earlier would have the text drawn straight
            // across it. Covering is not enough, though — an element that only
            // hides a control leaves it live, and the row under the bar may
            // carry a separator's promote chip, an image and its resize grip.
            // Clicking white paper would have reordered notes. So the bar takes
            // the mouse as well as the pixels: it blocks the hitboxes below it,
            // which puts them out of reach — cursor shapes and tooltips
            // included — and means no handler needs to know the bar exists in
            // order to refuse it. Everything except the wheel: `block_mouse_
            // except_scroll` rather than `occlude`, or the note would stop
            // scrolling wherever the pointer happened to be resting.
            //
            // That is exactly why the strip is painted *last*. A blocking
            // hitbox ends the hover list, so a strip underneath the bar would
            // stop being hovered the moment the bar appeared, the lights would
            // hide, and they would flicker out just as you reached them. Above
            // it, the strip keeps the pointer and the bar stays up. It blocks
            // nothing itself: below the bar's 42 points it is the note's own
            // top line that answers the mouse, which is why this is two
            // elements and not one.
            .when(self.window_buttons_visible, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        // The bar is only as wide as the lights themselves when
                        // it would otherwise cover something: the find bar's
                        // query field, or — in a window too short to reserve a
                        // full 42 points — the note's first line.
                        .when(!wide_lights, |d| d.w(px(TRAFFIC_LIGHT_GUTTER)))
                        .when(wide_lights, |d| d.right_0())
                        .h(px(TITLEBAR_H))
                        .block_mouse_except_scroll()
                        // The lights float straight over the page, and the page
                        // may be a photograph — three coloured dots on whatever
                        // happens to be behind them. A capsule cut to the
                        // cluster had to be positioned against buttons the
                        // system places, and read as a stray pill dropped on
                        // the text. A full-width bar has no fit to get wrong:
                        // it is the title bar this window does not otherwise
                        // have, borrowed for as long as the pointer is up here.
                        //
                        // No rule along the bottom. A hairline the width of the
                        // window is a header, and this is not one — it is a
                        // patch of the page kept clear so three system dots
                        // have somewhere to land.
                        .bg(rgb(theme().bg)),
                )
            })
            // The strip that brings the traffic lights back. While find is open
            // it narrows to the gutter the bar keeps clear for them, so it
            // reveals the buttons without swallowing a click meant for the
            // query field — before, it was not mounted at all while searching,
            // and a search left no way to close the window by mouse.
            .child(
                div()
                    .id("titlebar-hover")
                    .absolute()
                    .top_0()
                    .left_0()
                    .when(self.find.is_some(), |d| d.w(px(TRAFFIC_LIGHT_GUTTER)))
                    .when(self.find.is_none(), |d| d.right_0())
                    .h(top_inset(self.settings.text_size))
                    .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                        this.set_window_buttons_visible(*hovered, cx);
                    })),
            )
            // The only chrome that ever appears down here: a failed save, which
            // silently losing someone's note rules out, or a word about
            // something they just asked for. Nothing shows when there is
            // nothing to say.
            .when_some(self.notice.as_ref(), |d, notice| {
                let colour = if notice.alert { theme().danger } else { theme().fg_dim };
                let dismissable = notice.alert && !notice.sticky;
                let size = self.settings.text_size;
                // The notice renders at `chrome_text`, not the note size, so its
                // padding and its two-row clamp must be measured against the same
                // size — otherwise at large text the box is sized for a 30pt row
                // while the words inside it are 20pt, and the "2 rows" clamp lets
                // the box grow half a note-row taller than two rows of what shows.
                let chrome = chrome_text(size);
                let pad = px(chrome * 0.4);
                d.child(
                    // Laid over the note, not inserted above it. As a child in
                    // the flow it pushed the whole document up when it appeared
                    // and dropped it back when it expired — the line you were
                    // reading moved twice to tell you a backup had been written.
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .bg(rgb(theme().bg))
                        .flex()
                        .flex_row()
                        .justify_center()
                        .text_size(px(chrome))
                        .text_color(rgb(colour))
                        .child(
                            // Aligned to the same column as the note and the
                            // find bar, with the same hairline at the same
                            // extent.
                            div()
                                .w_full()
                                .max_w(reading_width(size))
                                .px(side_inset(size))
                                .child(
                                    div()
                                        .w_full()
                                        .py(pad)
                                        // Two rows of the chrome-sized text at
                                        // most: a long alert in a narrow window
                                        // would otherwise wrap over the note it
                                        // is laid on. The cap holds the two rows
                                        // plus the padding they sit in, with a
                                        // hair over so `overflow_hidden` does not
                                        // shave the second line's descenders.
                                        .max_h(
                                            px(chrome * LINE_HEIGHT * 2.)
                                                + pad * 2.
                                                + px(2.),
                                        )
                                        .overflow_hidden()
                                        .border_t_1()
                                        .border_color(rgb(theme().rule))
                                        .flex()
                                        .flex_row()
                                        .items_start()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(SharedString::from(notice.text.clone())),
                                        )
                                        // A remark leaves on its own; an alert
                                        // stays until it is answered, which for
                                        // a condition that does not clear meant
                                        // permanent furniture over the last line
                                        // with no way to put it away.
                                        .when(dismissable, |row| {
                                            row.child(
                                                div()
                                                    .id("notice-close")
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .size(px(chrome * 1.4))
                                                    .rounded_full()
                                                    .text_color(rgb(theme().fg_dim))
                                                    .hover(|s| {
                                                        s.bg(rgb(theme().find_field_bg))
                                                            .text_color(rgb(theme().fg))
                                                    })
                                                    .cursor(gpui::CursorStyle::PointingHand)
                                                    .child("✕")
                                                    .on_click(cx.listener(
                                                        |this, _e: &ClickEvent, _w, cx| {
                                                            this.notice = None;
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        }),
                                ),
                        ),
                )
            })
            // While recording a chord, the next key press is the shortcut, not a
            // command: capture it before any binding or the input bridge can act
            // on it. `record_shortcut` validates and either saves it or explains
            // why not.
            .when(self.recording_shortcut, |d| {
                d.capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _w, cx| {
                    this.record_shortcut(&e.keystroke, cx);
                    cx.stop_propagation();
                }))
            })
            // The Settings panel is laid over everything else when open.
            .when(self.settings_open, |d| {
                d.child(self.render_settings_panel(cx))
            })
            .when(self.backups_open, |d| {
                d.child(self.render_backups_panel(cx))
            })
            .when(self.palette.is_some(), |d| d.child(self.render_palette(cx)))
            // The right-click context menu, anchored at the click.
            .when_some(self.context_menu, |d, at| {
                d.child(self.render_context_menu(at, cx))
            })
            // Focus + input handler attachment via a child element that claims focus
            .child(InputBridge {
                app: cx.entity().clone(),
            })
    }
}

/// Invisible bridge so GPUI routes IME/text input to NoteApp.
struct InputBridge {
    app: Entity<NoteApp>,
}

impl IntoElement for InputBridge {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputBridge {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("input-bridge".into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = px(0.).into();
        style.size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let entity = self.app.clone();
        window.handle_input(
            &entity.read(cx).focus_handle,
            ElementInputHandler::new(bounds, entity),
            cx,
        );
    }
}

/// Zero-sized sibling that paints the inline-code chips *behind* a row's text.
///
/// A GPUI `TextRun` background always fills the full line leading, so a one-line
/// `code` run drawn that way is a tall bar with the leading filled — the defect
/// this replaces. Painting the chip as its own quad, inset to roughly the em
/// box and drawn before the text (so it sits under the glyphs), keeps it tight
/// to the code. Selection and find washes stay full-height `TextRun`
/// backgrounds, which is what continuity across a wrap wants.
///
/// Offsets in `code_spans` are line-local, the same space
/// [`TextLayout::position_for_index`] reads.
struct CodeUnderlay {
    line: usize,
    layout: TextLayout,
    /// Line-local ranges and the colour each is painted in: an inline-code chip,
    /// or a `==highlight==`. Both want the same tight band rather than a
    /// full-leading `TextRun` background.
    chips: Vec<(Range<usize>, Hsla)>,
}

impl IntoElement for CodeUnderlay {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeUnderlay {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(format!("code-underlay-{}", self.line).into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = px(0.).into();
        style.size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        if self.chips.is_empty() {
            return;
        }
        let line_height = self.layout.line_height();
        // ~1.3em centred in the 1.6em line box, so the chip hugs the glyphs
        // rather than filling the whole row's leading. Derived from the row's own
        // (already-scaled) line height, so inline code inside an enlarged heading
        // gets a proportionally taller chip instead of a body-sized sliver.
        let chip_h = line_height * (1.3 / LINE_HEIGHT);
        let pad = (line_height - chip_h) / 2.;
        let bounds = self.layout.bounds();
        let (left, right) = (bounds.left(), bounds.right());
        let mut chip = |x0: Pixels, x1: Pixels, y: Pixels, colour: Hsla| {
            if x1 > x0 {
                window.paint_quad(fill(
                    Bounds::from_corners(point(x0, y + pad), point(x1, y + pad + chip_h)),
                    colour,
                ));
            }
        };
        for (span, colour) in &self.chips {
            let (Some(start), Some(end)) = (
                self.layout.position_for_index(span.start),
                self.layout.position_for_index(span.end),
            ) else {
                continue;
            };
            if start.y == end.y {
                chip(start.x, end.x, start.y, *colour);
            } else {
                // The span soft-wrapped: a chip per visual row it crosses — to
                // the row's right edge, whole middle rows, then to the end.
                chip(start.x, right, start.y, *colour);
                let mut y = start.y + line_height;
                while y < end.y {
                    chip(left, right, y, *colour);
                    y += line_height;
                }
                chip(left, end.x, end.y, *colour);
            }
        }
    }
}

/// Zero-sized sibling of a row's text that does two jobs the text element
/// cannot: it registers the measured [`TextLayout`] with the app (for
/// hit-testing and IME geometry), and it paints the caret.
///
/// It runs *after* the text in child order, so by its prepaint the layout has
/// been measured and positioned, and its paint lands on top of the glyphs.
struct RowOverlay {
    line: usize,
    byte_start: usize,
    byte_end: usize,
    layout: TextLayout,
    /// The caret, when it is on this row: its byte offset within the line, the
    /// offset of the grapheme after it, and which side of a wrap it is on.
    /// Whether a wrap actually falls there is only known once the row has been
    /// measured, which is why the painter is given the ingredients rather than
    /// the answer. See [`rows::Affinity`].
    caret: Option<(usize, usize, rows::Affinity)>,
    /// The caret's opacity this frame. See [`caret_alpha`].
    caret_alpha: f32,
    app: Entity<NoteApp>,
}

impl IntoElement for RowOverlay {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RowOverlay {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(format!("overlay-{}", self.line).into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = px(0.).into();
        style.size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (line, byte_start, byte_end) = (self.line, self.byte_start, self.byte_end);
        let layout = self.layout.clone();
        self.app.update(cx, |app, _| {
            app.line_layouts.insert(
                line,
                rows::Row {
                    byte_start,
                    byte_end,
                    geometry: layout,
                },
            );
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let Some((local, next_local, affinity)) = self.caret else {
            return;
        };
        let row = rows::Row {
            byte_start: self.byte_start,
            byte_end: self.byte_end,
            geometry: self.layout.clone(),
        };
        // The same function the motion arithmetic uses. Drawing the caret and
        // deciding where it moves to are the same question about a wrap, and
        // answering it twice is how they drift apart.
        let Some((origin, line_height)) = rows::caret_origin(
            &row,
            self.byte_start + local,
            self.byte_start + next_local,
            affinity,
        ) else {
            return;
        };
        let mut colour: Hsla = rgb(theme().cursor).into();
        colour.a = self.caret_alpha;
        window.paint_quad(fill(
            Bounds::from_corners(
                origin,
                point(origin.x + CARET_WIDTH, origin.y + line_height),
            ),
            colour,
        ));
    }
}

// ── Menus ────────────────────────────────────────────────────────────────────

/// Everything that used to sit in a header lives here instead, where macOS
/// already puts an app's commands — and where they are discoverable with their
/// shortcuts spelled out.
///
/// Rebuilt whenever a menu item's label depends on state that has changed.
/// Turn a saved window frame into on-screen bounds, or `None` when it would
/// open off every display — a monitor was unplugged, or the resolution changed
/// since it was saved — so the caller centres instead. A window whose frame
/// overlaps no display is unreachable, which is worse than forgetting where it
/// was; requiring an overlap keeps at least part of the title bar grabbable.
fn restore_bounds(frame: settings::WindowFrame, cx: &App) -> Option<Bounds<Pixels>> {
    let bounds = Bounds {
        origin: point(px(frame.x), px(frame.y)),
        size: size(px(frame.w), px(frame.h)),
    };
    // Require the *title-bar strip* — the grabbable top of the window — to
    // overlap a display, not merely the window as a whole. A frame saved mostly
    // above the menu bar could "intersect" a screen by a one-pixel bottom sliver
    // yet restore with its title bar off the top of every display, leaving the
    // window impossible to move. If the strip is off-screen, fall back to
    // centering (the caller's default).
    let titlebar = Bounds {
        origin: bounds.origin,
        size: size(bounds.size.width, px(28.).min(bounds.size.height)),
    };
    cx.displays()
        .iter()
        .any(|display| display.bounds().intersects(&titlebar))
        .then_some(bounds)
}

/// The menu bar.
///
/// GPUI 0.2.2's `MenuItem::Action` carries no enabled/disabled flag — there is
/// no per-item greying to drive from state, and building a whole custom menu bar
/// to add one would be far out of proportion to the payoff. So the contract is
/// the other direction: every action here must be a safe no-op when it does not
/// apply, since the item is always clickable. That already holds — `undo`/`redo`
/// return early when the history stack is empty (`History::can_undo`/`can_redo`);
/// `cut`/`copy` with no selection act on the caret line the way every Mac editor
/// does; the note-moving and formatting commands fall through when there is
/// nothing to move or format. Adding a menu item means keeping that property.
fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "GravityNote".into(),
            items: vec![
                MenuItem::action("About GravityNote", AboutApp),
                MenuItem::separator(),
                // The show/hide chord, text size, and open-at-login all live in a
                // real Settings panel now (⌘,) rather than a submenu of fake
                // ticked labels — GPUI menu items carry no checkmark column.
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::submenu(Menu {
                    name: "Text Size".into(),
                    items: vec![
                        MenuItem::action("Bigger", TextBigger),
                        MenuItem::action("Smaller", TextSmaller),
                        MenuItem::action("Default", TextSizeReset),
                    ],
                }),
                MenuItem::action(open_at_login_label(), ToggleOpenAtLogin),
                MenuItem::separator(),
                MenuItem::action("Hide GravityNote", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAllApps),
                MenuItem::separator(),
                MenuItem::action("Quit GravityNote", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Export…", ExportNote),
                MenuItem::action("Reveal in Finder", RevealInFinder),
                MenuItem::separator(),
                MenuItem::action("Back Up Now", BackupNow),
                MenuItem::action("Restore from Backup…", OpenBackups),
                MenuItem::separator(),
                MenuItem::action("Reclaim Unused Images…", ReclaimImages),
                MenuItem::action("Change Note Folder…", ChooseNoteFolder),
            ],
        },
        Menu {
            name: "Note".into(),
            items: vec![
                MenuItem::action("New Note", NewNote),
                MenuItem::separator(),
                MenuItem::action("Bring Note to Top", BringNoteUp),
                MenuItem::action("Move Note Up", MoveNoteUp),
                MenuItem::separator(),
                MenuItem::action("Indent", Indent),
                MenuItem::action("Outdent", Outdent),
                MenuItem::action("Make a Task", ToggleTask),
                MenuItem::action("Complete", ToggleDone),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", Undo, OsAction::Undo),
                MenuItem::os_action("Redo", Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
                MenuItem::separator(),
                // Find belongs under Edit, where macOS puts it — and where its
                // ⌘F / ⌘G / ⌘⇧G equivalents become discoverable. It used to be a
                // lone "Find in Note" under Help.
                MenuItem::submenu(Menu {
                    name: "Find".into(),
                    items: vec![
                        MenuItem::action("Find…", OpenFind),
                        MenuItem::action("Find and Replace…", ToggleReplace),
                        MenuItem::action("Find Next", FindNext),
                        MenuItem::action("Find Previous", FindPrevious),
                        MenuItem::separator(),
                        MenuItem::action("Use Selection for Find", UseSelectionForFind),
                        MenuItem::action("Jump to Selection", JumpToSelection),
                    ],
                }),
            ],
        },
        // macOS auto-populates a menu named exactly "Window" with Minimize/Zoom
        // and the window list; these are the app's own additions to it.
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", ZoomWindow),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullScreen),
            ],
        },
        // A menu titled exactly "Help" is what macOS looks for to attach its own
        // Search field at the top — the one that finds and highlights any command
        // across every menu here. The system inserts it; the item below is the
        // menu's own content, so it is never empty before the search is used.
        Menu {
            name: "Help".into(),
            items: vec![MenuItem::action("GravityNote Help", ShowHelp)],
        },
    ]
}

/// GPUI's menu items carry no checked state, so the label carries it. A tick in
/// the name is what the system's own menus do when they cannot use a checkmark
/// column.
/// The well-known command a hotkey would shadow globally, if it is one of the
/// plain ⌘-key shortcuts every Mac app shares. `None` for anything else — an
/// ⌥/⌃ chord, or a key no app claims — which is safe to reserve without a word.
fn common_shortcut_conflict(h: &Hotkey) -> Option<&'static str> {
    // Only bare ⌘ (optionally with ⇧) collides with the universal shortcuts.
    if !h.cmd || h.ctrl || h.alt {
        return None;
    }
    match (h.shift, h.key.as_str()) {
        (false, "q") => Some("Quit"),
        (false, "w") => Some("Close Window"),
        (false, "n") => Some("New"),
        (false, "s") => Some("Save"),
        (false, "o") => Some("Open"),
        (false, "p") => Some("Print"),
        (false, "f") => Some("Find"),
        (false, "g") => Some("Find Next"),
        (false, "z") => Some("Undo"),
        (false, "x") => Some("Cut"),
        (false, "c") => Some("Copy"),
        (false, "v") => Some("Paste"),
        (false, "a") => Some("Select All"),
        (false, "t") => Some("New Tab"),
        (false, "h") => Some("Hide"),
        (false, "m") => Some("Minimize"),
        (false, "space") => Some("Spotlight"),
        (false, ",") => Some("Settings"),
        _ => None,
    }
}

fn open_at_login_label() -> SharedString {
    if login_item::enabled() {
        "✓ Open at Login".into()
    } else {
        "Open at Login".into()
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

mod onboarding;

fn main() {
    println!("gravitynote: starting (Rust + GPUI markdown note window)");

    Application::new().with_assets(Icons).run(|cx: &mut App| {
        // Register embedded Lilex — all faces, so bold/italic markdown resolves.
        if let Err(err) = cx.text_system().add_fonts(vec![
            Cow::Borrowed(LILEX_REGULAR),
            Cow::Borrowed(LILEX_SEMIBOLD),
            Cow::Borrowed(LILEX_BOLD),
            Cow::Borrowed(LILEX_ITALIC),
            Cow::Borrowed(LILEX_BOLD_ITALIC),
        ]) {
            eprintln!("gravitynote: failed to load Lilex fonts: {err}");
        }

        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, Some("Note")),
            // ⌘W hides the window the way the red button does — a keyboard user
            // otherwise had only the global chord to put GravityNote away.
            KeyBinding::new("cmd-w", ToggleWindow, Some("Note")),
            // The standard window/app commands. Bound in the Note context, but
            // reached as menu key equivalents from either context, so ⌘M / ⌘H
            // work even while the find bar has the keyboard.
            KeyBinding::new("cmd-m", Minimize, Some("Note")),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, Some("Note")),
            KeyBinding::new("cmd-h", HideApp, Some("Note")),
            KeyBinding::new("cmd-alt-h", HideOthers, Some("Note")),
            KeyBinding::new("cmd-n", NewNote, Some("Note")),
            // ⌘⇧↑ is the standard "select to document start" (mirror of ⌘⇧↓);
            // it must not be a structural note move. Bringing a note to the top
            // lives on ⌘⌃⇧↑ — the ⌘⌃↑ "move up one" chord plus Shift for "all
            // the way up", so the two note moves share a family.
            KeyBinding::new("cmd-shift-up", SelectToDocumentStart, Some("Note")),
            KeyBinding::new("cmd-ctrl-up", MoveNoteUp, Some("Note")),
            KeyBinding::new("cmd-ctrl-shift-up", BringNoteUp, Some("Note")),
            KeyBinding::new("cmd-s", BackupNow, Some("Note")),
            KeyBinding::new("backspace", Backspace, Some("Note")),
            KeyBinding::new("delete", Delete, Some("Note")),
            KeyBinding::new("left", Left, Some("Note")),
            KeyBinding::new("right", Right, Some("Note")),
            KeyBinding::new("up", Up, Some("Note")),
            KeyBinding::new("down", Down, Some("Note")),
            KeyBinding::new("pageup", PageUp, Some("Note")),
            KeyBinding::new("pagedown", PageDown, Some("Note")),
            // ⌥↑/↓ jump by prose paragraph, the standard macOS caret binding.
            KeyBinding::new("alt-up", ParagraphUp, Some("Note")),
            KeyBinding::new("alt-down", ParagraphDown, Some("Note")),
            KeyBinding::new("alt-shift-up", SelectParagraphUp, Some("Note")),
            KeyBinding::new("alt-shift-down", SelectParagraphDown, Some("Note")),
            KeyBinding::new("alt-left", WordLeft, Some("Note")),
            KeyBinding::new("alt-right", WordRight, Some("Note")),
            KeyBinding::new("alt-shift-left", SelectWordLeft, Some("Note")),
            KeyBinding::new("alt-shift-right", SelectWordRight, Some("Note")),
            KeyBinding::new("alt-backspace", DeleteWordBack, Some("Note")),
            KeyBinding::new("alt-delete", DeleteWordForward, Some("Note")),
            KeyBinding::new("cmd-backspace", DeleteToLineStart, Some("Note")),
            KeyBinding::new("cmd-z", Undo, Some("Note")),
            KeyBinding::new("cmd-shift-z", Redo, Some("Note")),
            // ⌘Y is the other redo chord a lot of muscle memory reaches for.
            KeyBinding::new("cmd-y", Redo, Some("Note")),
            KeyBinding::new("cmd-=", TextBigger, Some("Note")),
            KeyBinding::new("cmd-+", TextBigger, Some("Note")),
            // On a US layout the "⌘+" people press is physically ⌘⇧= — without
            // this the conventional zoom-in chord did nothing.
            KeyBinding::new("cmd-shift-=", TextBigger, Some("Note")),
            KeyBinding::new("cmd--", TextSmaller, Some("Note")),
            KeyBinding::new("cmd-0", TextSizeReset, Some("Note")),
            KeyBinding::new("cmd-f", OpenFind, Some("Note")),
            KeyBinding::new("cmd-,", OpenSettings, Some("Note")),
            KeyBinding::new("escape", CloseFind, Some("Note")),
            // ⌘. is macOS's other "cancel": it dismisses find / the settings
            // panel the same way Escape does.
            KeyBinding::new("cmd-.", CloseFind, Some("Note")),
            KeyBinding::new("cmd-g", FindNext, Some("Note")),
            KeyBinding::new("cmd-shift-g", FindPrevious, Some("Note")),
            KeyBinding::new("cmd-e", UseSelectionForFind, Some("Note")),
            KeyBinding::new("cmd-j", JumpToSelection, Some("Note")),
            KeyBinding::new("alt-cmd-f", ToggleReplace, Some("Note")),
            // ⌘⇧V is Paste and Match Style; in a plain-text note it is just Paste.
            KeyBinding::new("cmd-shift-v", Paste, Some("Note")),
            KeyBinding::new("shift-enter", FindPrevious, Some("Note")),
            KeyBinding::new("shift-left", SelectLeft, Some("Note")),
            KeyBinding::new("shift-right", SelectRight, Some("Note")),
            KeyBinding::new("shift-up", SelectUp, Some("Note")),
            KeyBinding::new("shift-down", SelectDown, Some("Note")),
            KeyBinding::new("cmd-shift-left", SelectHome, Some("Note")),
            KeyBinding::new("cmd-shift-right", SelectEnd, Some("Note")),
            // ⇧Home / ⇧End extend to the row edge, matching ⇧⌘←/→ on the keys a
            // full keyboard actually has.
            KeyBinding::new("shift-home", SelectHome, Some("Note")),
            KeyBinding::new("shift-end", SelectEnd, Some("Note")),
            KeyBinding::new("cmd-a", SelectAll, Some("Note")),
            KeyBinding::new("home", Home, Some("Note")),
            KeyBinding::new("cmd-left", Home, Some("Note")),
            KeyBinding::new("end", End, Some("Note")),
            KeyBinding::new("cmd-right", End, Some("Note")),
            KeyBinding::new("cmd-up", DocumentStart, Some("Note")),
            KeyBinding::new("cmd-down", DocumentEnd, Some("Note")),
            // ⌘⇧↑ is this app's "bring the note to the top", so selection to
            // the document ends lives on the ⌘⇧ home/end pair instead.
            KeyBinding::new("cmd-shift-home", SelectToDocumentStart, Some("Note")),
            KeyBinding::new("cmd-shift-end", SelectToDocumentEnd, Some("Note")),
            KeyBinding::new("cmd-shift-down", SelectToDocumentEnd, Some("Note")),
            KeyBinding::new("enter", Enter, Some("Note")),
            KeyBinding::new("cmd-v", Paste, Some("Note")),
            KeyBinding::new("cmd-c", Copy, Some("Note")),
            KeyBinding::new("cmd-x", Cut, Some("Note")),
            KeyBinding::new("tab", Indent, Some("Note")),
            KeyBinding::new("shift-tab", Outdent, Some("Note")),
            KeyBinding::new("cmd-shift-t", ToggleTask, Some("Note")),
            KeyBinding::new("cmd-enter", ToggleDone, Some("Note")),
            // The emacs-style control keys every Cocoa text field carries. Most
            // reuse the motion/delete actions already defined; ⌃K and ⌃T are
            // their own. ⌃A is deliberately absent — it is the global show/hide
            // hotkey's default, not line-start.
            KeyBinding::new("ctrl-d", Delete, Some("Note")),
            KeyBinding::new("ctrl-h", Backspace, Some("Note")),
            KeyBinding::new("ctrl-e", End, Some("Note")),
            KeyBinding::new("ctrl-f", Right, Some("Note")),
            KeyBinding::new("ctrl-b", Left, Some("Note")),
            KeyBinding::new("ctrl-p", Up, Some("Note")),
            KeyBinding::new("ctrl-n", Down, Some("Note")),
            KeyBinding::new("ctrl-k", KillToEnd, Some("Note")),
            // ⌃W deletes the word behind the caret, ⌃U back to the line start —
            // the terminal/emacs deletions that work in every macOS text field.
            KeyBinding::new("ctrl-w", DeleteWordBack, Some("Note")),
            KeyBinding::new("ctrl-u", DeleteToLineStart, Some("Note")),
            KeyBinding::new("ctrl-t", Transpose, Some("Note")),
            // Move the line (or the selected lines) up and down, and duplicate
            // them: the three commands every editor has and this one did not.
            KeyBinding::new("alt-cmd-up", MoveLineUp, Some("Note")),
            KeyBinding::new("alt-cmd-down", MoveLineDown, Some("Note")),
            KeyBinding::new("cmd-d", DuplicateLines, Some("Note")),
            // Everything the menus hold, searchable. ⌘⇧P is where every editor
            // put it, and this app has sixty commands behind three menus.
            KeyBinding::new("cmd-shift-p", OpenPalette, Some("Note")),
            KeyBinding::new("cmd-shift-p", OpenPalette, Some("Palette")),
            KeyBinding::new("escape", CloseFind, Some("Palette")),
            KeyBinding::new("cmd-.", CloseFind, Some("Palette")),
            KeyBinding::new("down", PaletteStep, Some("Palette")),
            KeyBinding::new("up", PaletteStepBack, Some("Palette")),
            KeyBinding::new("ctrl-n", PaletteStep, Some("Palette")),
            KeyBinding::new("ctrl-p", PaletteStepBack, Some("Palette")),
            KeyBinding::new("enter", PaletteRun, Some("Palette")),
            KeyBinding::new("backspace", Backspace, Some("Palette")),
            // The rest of the standard Cocoa control set: ⌃Y yanks the last ⌃K
            // kill, ⌃O opens a line, ⌃V pages down, ⌃L recentres the caret.
            KeyBinding::new("ctrl-y", Yank, Some("Note")),
            KeyBinding::new("ctrl-o", OpenLine, Some("Note")),
            KeyBinding::new("ctrl-v", PageDown, Some("Note")),
            KeyBinding::new("ctrl-l", CenterCaret, Some("Note")),
            // The find bar's keyboard. Typing itself arrives through the input
            // handler, not a binding, so only the commands are listed here.
            // The modal Settings context: only cancel is bound, so nothing else
            // reaches the note. Recording a chord captures keys ahead of this.
            KeyBinding::new("escape", CloseFind, Some("Settings")),
            KeyBinding::new("cmd-.", CloseFind, Some("Settings")),
            KeyBinding::new("cmd-,", OpenSettings, Some("Settings")),
            KeyBinding::new("escape", CloseFind, Some("Find")),
            KeyBinding::new("cmd-.", CloseFind, Some("Find")),
            KeyBinding::new("enter", FindNext, Some("Find")),
            KeyBinding::new("shift-enter", FindPrevious, Some("Find")),
            KeyBinding::new("down", FindNext, Some("Find")),
            KeyBinding::new("up", FindPrevious, Some("Find")),
            KeyBinding::new("cmd-g", FindNext, Some("Find")),
            KeyBinding::new("cmd-shift-g", FindPrevious, Some("Find")),
            KeyBinding::new("backspace", Backspace, Some("Find")),
            // The query field's other deletions and its own undo/redo — the same
            // set the note has, kept inside the field while it owns the keyboard.
            KeyBinding::new("delete", FindDeleteForward, Some("Find")),
            KeyBinding::new("alt-backspace", FindDeleteWordBack, Some("Find")),
            KeyBinding::new("alt-delete", FindDeleteWordForward, Some("Find")),
            KeyBinding::new("cmd-backspace", FindDeleteToStart, Some("Find")),
            KeyBinding::new("cmd-z", Undo, Some("Find")),
            KeyBinding::new("cmd-shift-z", Redo, Some("Find")),
            KeyBinding::new("cmd-y", Redo, Some("Find")),
            // Tab moves between the query and replacement fields; ⌥⌘F toggles the
            // replace row; ⌘⌥⏎ replaces the current match and steps on.
            KeyBinding::new("tab", FindFocusNextField, Some("Find")),
            KeyBinding::new("alt-cmd-f", ToggleReplace, Some("Find")),
            KeyBinding::new("cmd-alt-enter", ReplaceAndFind, Some("Find")),
            KeyBinding::new("cmd-v", Paste, Some("Find")),
            KeyBinding::new("cmd-f", OpenFind, Some("Find")),
            KeyBinding::new("cmd-q", Quit, Some("Find")),
            // The query field is a real text field: the standard motion keys
            // move its caret and select within it, never the note. ⌘A / ⌘C
            // reach the query through the SelectAll / Copy handlers, which branch
            // while find is open, so they need no binding of their own here.
            KeyBinding::new("left", FindCharLeft, Some("Find")),
            KeyBinding::new("right", FindCharRight, Some("Find")),
            KeyBinding::new("alt-left", FindWordLeft, Some("Find")),
            KeyBinding::new("alt-right", FindWordRight, Some("Find")),
            KeyBinding::new("home", FindLineStart, Some("Find")),
            KeyBinding::new("end", FindLineEnd, Some("Find")),
            KeyBinding::new("cmd-left", FindLineStart, Some("Find")),
            KeyBinding::new("cmd-right", FindLineEnd, Some("Find")),
            KeyBinding::new("shift-left", FindSelectCharLeft, Some("Find")),
            KeyBinding::new("shift-right", FindSelectCharRight, Some("Find")),
            KeyBinding::new("alt-shift-left", FindSelectWordLeft, Some("Find")),
            KeyBinding::new("alt-shift-right", FindSelectWordRight, Some("Find")),
            KeyBinding::new("shift-home", FindSelectLineStart, Some("Find")),
            KeyBinding::new("shift-end", FindSelectLineEnd, Some("Find")),
            KeyBinding::new("cmd-shift-left", FindSelectLineStart, Some("Find")),
            KeyBinding::new("cmd-shift-right", FindSelectLineEnd, Some("Find")),
        ]);

        // Read the persisted preferences once here. The view reads the same
        // file, so both agree. The saved show/hide chord (or `None` when the
        // user turned it off) is what the global hotkey registers; the saved
        // frame is where the window reopens.
        let startup = Settings::load(&settings::settings_path());
        let window_background = window_background_for_glass(startup.glass);
        // The development build never claims the global chord. Only one process
        // can hold it, so whichever launched last would silently take ⌃A away
        // from the copy holding the real notes — and give it to the copy being
        // restarted every few minutes. The menu-bar item is still installed:
        // it is worth being able to test.
        let startup_shortcut = if dev::is_dev() {
            None
        } else {
            startup.toggle_shortcut.clone()
        };

        // Everything that used to sit in a header lives here instead, where
        // macOS already puts an app's commands — and where they are
        // discoverable with their shortcuts spelled out.
        cx.set_menus(menus());

        // Reopen where the window was last left, if that frame still lands on a
        // display. Otherwise a first-run default: 30% narrower than the old
        // 820pt, a slimmer page that leans on the reading measure to centre the
        // column rather than filling a wide window with it.
        let bounds = startup
            .window
            .and_then(|frame| restore_bounds(frame, cx))
            .unwrap_or_else(|| Bounds::centered(None, size(px(574.), px(760.)), cx));
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("GravityNote".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.), px(14.))),
                    }),
                    // Small enough to park in a corner of the screen as a
                    // scratch pad — a few words per line is a legitimate way to
                    // use this, and the app being a sheet of paper with no
                    // chrome is what makes it one. The floor is the window
                    // buttons and one row of text: below that there is nothing
                    // left to shrink.
                    window_min_size: Some(size(px(180.), px(120.))),
                    // Native compositor material from the reference. The root
                    // stays transparent except for Identity's solid paper fill.
                    window_background,
                    focus: true,
                    show: true,
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(NoteApp::new);
                    window.focus(&view.read(cx).focus_handle);

                    // The red button hides rather than closes. GPUI would
                    // otherwise destroy the only window and leave the process
                    // running with nothing to show and no way back: the tick
                    // loop would stop, taking the menu-bar item and the global
                    // ⌃A hotkey with it. Hiding matches how ⌃A already behaves.
                    let closing = view.clone();
                    window.on_window_should_close(cx, move |window, cx| {
                        closing.update(cx, |app, _| {
                            app.flush();
                            app.save_window_frame(window);
                        });
                        cx.hide();
                        false
                    });

                    view
                },
            )
            .expect("open GravityNote window");

        // Menu-bar item + global show/hide hotkey, registered to the saved chord.
        // Degrades to an in-app-only shortcut when the platform layer cannot
        // install (e.g. the chord is already taken). The handle goes to the view,
        // which keeps it alive and re-registers the chord when Settings change;
        // the tick loop only needs the receiver.
        let rx = match platform::install(startup_shortcut) {
            Ok((handle, rx)) => {
                if let Some(warning) = handle.warning() {
                    eprintln!("gravitynote: {warning}");
                }
                let handle = Rc::new(RefCell::new(handle));
                let _ = window.update(cx, |view, _, cx| view.set_platform(handle, cx));
                Some(rx)
            }
            Err(err) => {
                eprintln!("gravitynote: menu bar unavailable: {err}");
                None
            }
        };

        // Repaint the caret the instant focus changes, rather than waiting up to
        // one idle poll (CARET_IDLE_POLL) to notice. `advance_caret` already does
        // the right thing on the transition — solid on return, gone on blur — so
        // this only has to run it and notify synchronously when the window's
        // active state actually flips.
        let _ = window.update(cx, |_view, window, cx| {
            cx.observe_window_activation(window, |view, window, cx| {
                let (repaint, _) = view.advance_caret(window.is_window_active());
                if repaint {
                    cx.notify();
                }
            })
            .detach();
        });

        // One timer drives autosave settling, hourly backups, and menu events.
        cx.spawn(async move |cx: &mut AsyncApp| {
            let rx = rx;
            loop {
                cx.background_executor().timer(TICK).await;

                let events: Vec<PlatformEvent> = rx
                    .as_ref()
                    .map(|rx| rx.try_iter().collect())
                    .unwrap_or_default();

                let alive = window
                    .update(cx, |view, window, cx| {
                        for event in events {
                            view.on_platform_event(event, window, cx);
                        }
                        view.tick(window, cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        // The caret fades, so it needs a finer clock than the housekeeping one —
        // but only while it is actually fading. `caret::phase` says how long its
        // answer holds, so the still phases are slept through rather than
        // polled, and an unfocused window is not woken at all.
        cx.spawn(async move |cx: &mut AsyncApp| {
            let mut delay = caret::cycle();
            loop {
                cx.background_executor().timer(delay).await;
                let Ok(next) = window.update(cx, |view, window, cx| {
                    let (repaint, next) = view.advance_caret(window.is_window_active());
                    if repaint {
                        cx.notify();
                    }
                    next
                }) else {
                    break;
                };
                delay = next;
            }
        })
        .detach();

        // Flush the buffer and remember the window frame on any quit path that
        // skips the action handler.
        cx.on_app_quit(move |cx| {
            let _ = window.update(cx, |view, window, cx| {
                // Quit is the one path with no next tick, so reconcile here
                // before the final flush: if `note.md` changed underneath us,
                // this backs the external copy up (or, failing that, blocks the
                // flush) instead of silently overwriting it on the way out.
                // Quitting is the last chance to notice, so it always looks.
                if !view.reconcile_disk(true, cx) {
                    view.flush();
                }
                view.save_window_frame(window);
            });
            async {}
        })
        .detach();

        // The window opens as a bare sheet of paper: the traffic lights come
        // back when the pointer goes looking for them in the top strip.
        platform::set_window_buttons_visible(dev::show_handles());

        cx.activate(true);
        let _ = window.update(cx, |_, window, cx| onboarding::show_once(window, cx));
        println!("gravitynote: window open");
    });
}
