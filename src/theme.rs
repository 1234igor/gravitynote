//! The palette, in two of them, and the markdown style → text attribute map.
//!
//! Paper and ink, light or dark. Markdown accents stay low-saturation either way
//! so a page of prose still reads as prose; only structural markup (headings,
//! list bullets, links, code) earns colour.
//!
//! Which palette is in force is process-wide rather than threaded through every
//! call: a colour is asked for from inside deeply nested element builders dozens
//! of times per row, and the answer is the same for all of them. It is written
//! once per frame, from the window's appearance, before anything reads it.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{rgb, FontStyle, FontWeight, Hsla, Rgba};

use crate::markdown::MdStyle;

/// Every colour the app draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub fg: u32,
    pub fg_dim: u32,
    pub rule: u32,
    pub cursor: u32,
    pub selection: u32,
    pub selection_inactive: u32,
    pub placeholder: u32,
    pub danger: u32,
    pub find_match: u32,
    pub find_current: u32,
    pub scroll_thumb: u32,
    pub find_tick: u32,
    pub btn_fg: u32,
    pub btn_fg_hover: u32,
    pub btn_bg_hover: u32,
    pub find_field_bg: u32,
    pub find_field_border: u32,
    pub find_clear_bg: u32,
    pub find_clear_bg_hover: u32,
    pub marker: u32,
    pub bullet: u32,
    pub code_bg: u32,
    pub codeblock_fg: u32,
    pub codeblock_bg: u32,
    pub link: u32,
    pub highlight_bg: u32,
}

/// Paper white, near-black ink.
pub const LIGHT: Palette = Palette {
    bg: 0xfffffe,
    fg: 0x1c1f24,
    fg_dim: 0x6f767f,
    rule: 0xdcdfe5,
    cursor: 0x2f6df6,
    selection: 0xd8e5ff,
    selection_inactive: 0xe0e2e6,
    placeholder: 0x8b939d,
    danger: 0xc0392b,
    find_match: 0xfde7b4,
    find_current: 0xffd463,
    scroll_thumb: 0xcbd0d8,
    find_tick: 0xe2ae36,
    btn_fg: 0x8f97a1,
    btn_fg_hover: 0x2f6df6,
    btn_bg_hover: 0xd8e5ff,
    find_field_bg: 0xeef0f3,
    find_field_border: 0xd8dce2,
    find_clear_bg: 0xb7bec8,
    find_clear_bg_hover: 0x8f97a1,
    marker: 0xaeb5be,
    bullet: 0x8f97a1,
    code_bg: 0xe9edf3,
    codeblock_fg: 0x33404d,
    codeblock_bg: 0xeef1f5,
    link: 0x2f6df6,
    highlight_bg: 0xd6f0d2,
};

/// The same page after dark: a near-black surface rather than pure black, which
/// on an OLED display makes every edge a hard line, and inks lifted off it far
/// enough to read without glowing.
pub const DARK: Palette = Palette {
    bg: 0x16181c,
    fg: 0xe6e8ec,
    fg_dim: 0x9aa1ab,
    rule: 0x2c3037,
    cursor: 0x6ea2ff,
    selection: 0x243a5e,
    selection_inactive: 0x2a2e35,
    placeholder: 0x6d757f,
    danger: 0xf07a6d,
    find_match: 0x5a4a1e,
    find_current: 0x8a6d19,
    scroll_thumb: 0x3a3f47,
    find_tick: 0xc39a30,
    btn_fg: 0x767d87,
    btn_fg_hover: 0x6ea2ff,
    btn_bg_hover: 0x243a5e,
    find_field_bg: 0x212429,
    find_field_border: 0x343941,
    find_clear_bg: 0x4a505a,
    find_clear_bg_hover: 0x6d757f,
    marker: 0x5f666f,
    bullet: 0x8b939d,
    code_bg: 0x23272e,
    codeblock_fg: 0xc7cdd6,
    codeblock_bg: 0x1c1f25,
    link: 0x6ea2ff,
    highlight_bg: 0x2f4a2c,
};

/// Which palette the next colour lookup answers from. `0` light, `1` dark.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The palette in force.
pub fn theme() -> &'static Palette {
    if CURRENT.load(Ordering::Relaxed) == 0 {
        &LIGHT
    } else {
        &DARK
    }
}

/// Switch palettes. Called from the render path with the window's appearance
/// and the user's preference already resolved; a no-op when nothing changed.
pub fn set_dark(dark: bool) {
    CURRENT.store(u8::from(dark), Ordering::Relaxed);
}

/// Whether the dark palette is in force.
pub fn is_dark() -> bool {
    CURRENT.load(Ordering::Relaxed) == 1
}

/// Resolved text attributes for one markdown span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attrs {
    pub color: Rgba,
    pub weight: FontWeight,
    pub style: FontStyle,
    pub background: Option<Rgba>,
    pub underline: bool,
    pub strikethrough: bool,
}

impl Attrs {
    fn plain(hex: u32) -> Self {
        Self {
            color: rgb(hex),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            background: None,
            underline: false,
            strikethrough: false,
        }
    }

    fn bold(mut self) -> Self {
        self.weight = FontWeight::BOLD;
        self
    }

    fn semibold(mut self) -> Self {
        self.weight = FontWeight::SEMIBOLD;
        self
    }

    fn italic(mut self) -> Self {
        self.style = FontStyle::Italic;
        self
    }

    fn bg(mut self, hex: u32) -> Self {
        self.background = Some(rgb(hex));
        self
    }

    fn underlined(mut self) -> Self {
        self.underline = true;
        self
    }

    fn struck(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    pub fn color_hsla(&self) -> Hsla {
        self.color.into()
    }

    pub fn background_hsla(&self) -> Option<Hsla> {
        self.background.map(Into::into)
    }
}

/// Text attributes for a markdown span style.
///
/// Exhaustive on [`MdStyle`] on purpose — adding a variant should fail the build
/// here rather than silently render as body text.
pub fn attrs_for(style: MdStyle) -> Attrs {
    match style {
        MdStyle::Text => Attrs::plain(theme().fg),
        MdStyle::Heading(1) => Attrs::plain(theme().fg).bold(),
        MdStyle::Heading(2) => Attrs::plain(theme().fg).bold(),
        MdStyle::Heading(_) => Attrs::plain(theme().fg).semibold(),
        MdStyle::HeadingMarker(_) => Attrs::plain(theme().marker),
        MdStyle::Bold => Attrs::plain(theme().fg).bold(),
        MdStyle::Italic => Attrs::plain(theme().fg).italic(),
        MdStyle::BoldItalic => Attrs::plain(theme().fg).bold().italic(),
        // The chip is painted by the renderer, not by the run's background.
        MdStyle::Highlight => Attrs::plain(theme().fg),
        MdStyle::Strikethrough => Attrs::plain(theme().fg_dim).struck(),
        MdStyle::Code => Attrs::plain(theme().codeblock_fg).bg(theme().code_bg),
        MdStyle::CodeBlock => Attrs::plain(theme().codeblock_fg).bg(theme().codeblock_bg),
        MdStyle::Fence => Attrs::plain(theme().marker).bg(theme().codeblock_bg),
        MdStyle::Marker => Attrs::plain(theme().marker),
        MdStyle::LinkText => Attrs::plain(theme().link),
        MdStyle::LinkUrl => Attrs::plain(theme().link).underlined(),
        MdStyle::ListMarker => Attrs::plain(theme().bullet),
        MdStyle::TaskOpen => Attrs::plain(theme().bullet),
        MdStyle::TaskDone => Attrs::plain(theme().fg_dim),
        MdStyle::QuoteMarker => Attrs::plain(theme().bullet),
        MdStyle::Quote => Attrs::plain(theme().fg_dim),
        MdStyle::Separator => Attrs::plain(theme().marker),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emphasis_maps_to_real_faces() {
        assert_eq!(attrs_for(MdStyle::Bold).weight, FontWeight::BOLD);
        assert_eq!(attrs_for(MdStyle::Italic).style, FontStyle::Italic);
        let bi = attrs_for(MdStyle::BoldItalic);
        assert_eq!(bi.weight, FontWeight::BOLD);
        assert_eq!(bi.style, FontStyle::Italic);
    }

    #[test]
    fn code_spans_carry_a_background() {
        assert!(attrs_for(MdStyle::Code).background.is_some());
        assert!(attrs_for(MdStyle::CodeBlock).background.is_some());
        assert!(attrs_for(MdStyle::Text).background.is_none());
    }

    #[test]
    fn all_heading_levels_resolve() {
        for level in 1..=6u8 {
            let a = attrs_for(MdStyle::Heading(level));
            assert!(a.weight >= FontWeight::SEMIBOLD, "level {level}");
            assert_eq!(attrs_for(MdStyle::HeadingMarker(level)).weight, FontWeight::NORMAL);
        }
    }
}
