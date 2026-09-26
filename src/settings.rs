//! Persisted preferences.
//!
//! Text size, appearance, glass material, and the global chord that shows or
//! hides the window live in a one-line-per-key file next to the note.
//!
//! Reading never fails: a missing, unreadable, or malformed file yields the
//! defaults, because a broken preference must never stop the note from opening.

use std::io;
use std::path::{Path, PathBuf};

/// Smallest text size the menu will go to.
pub const MIN_TEXT_SIZE: f32 = 11.0;
/// Largest text size the menu will go to.
pub const MAX_TEXT_SIZE: f32 = 30.0;
/// Default text size, in points.
pub const DEFAULT_TEXT_SIZE: f32 = 13.0;
/// One notch of the Bigger / Smaller commands.
pub const TEXT_SIZE_STEP: f32 = 1.0;

/// `~/Library/Application Support/gravitynote-gpui/settings.txt`
pub fn settings_path() -> PathBuf {
    crate::note::default_note_path().with_file_name("settings.txt")
}

// Carbon modifier bits (Events.h).
const CARBON_CMD: u32 = 0x0100;
const CARBON_SHIFT: u32 = 0x0200;
const CARBON_OPTION: u32 = 0x0800;
const CARBON_CONTROL: u32 = 0x1000;

/// A global show/hide chord: one non-modifier key plus any set of modifiers.
///
/// Held as a layout-independent key *name* (the same spelling GPUI uses, e.g.
/// `"a"`, `"space"`, `` "`" ``) and four modifier flags. The Carbon virtual key
/// code the hotkey API wants is derived on demand from an ANSI table, and the
/// on-disk form is the same human-readable spelling — so the setting survives a
/// keycode table that only ever grows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    /// The GPUI/lowercase key name, e.g. `"a"`, `"space"`, `"f5"`.
    pub key: String,
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Hotkey {
    pub fn new(key: impl Into<String>, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> Self {
        Self {
            key: key.into(),
            cmd,
            ctrl,
            alt,
            shift,
        }
    }

    /// At least one modifier must be held. A bare key reserved system-wide would
    /// swallow that key everywhere on the Mac — never something to register.
    pub fn has_modifier(&self) -> bool {
        self.cmd || self.ctrl || self.alt || self.shift
    }

    /// The Carbon virtual key code and modifier mask for `RegisterEventHotKey`,
    /// or `None` when the key is not one this app knows how to register.
    pub fn carbon(&self) -> Option<(u32, u32)> {
        let code = key_to_vk(&self.key)?;
        let mut mods = 0;
        if self.cmd {
            mods |= CARBON_CMD;
        }
        if self.shift {
            mods |= CARBON_SHIFT;
        }
        if self.alt {
            mods |= CARBON_OPTION;
        }
        if self.ctrl {
            mods |= CARBON_CONTROL;
        }
        Some((code, mods))
    }

    /// The chord written the way macOS spells it — control, option, shift,
    /// command, then the key — for menu hints and the settings panel.
    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push('⌃');
        }
        if self.alt {
            out.push('⌥');
        }
        if self.shift {
            out.push('⇧');
        }
        if self.cmd {
            out.push('⌘');
        }
        out.push_str(&key_display(&self.key));
        out
    }

    /// The stable on-disk form: modifiers in a fixed order joined to the key by
    /// `+`, e.g. `ctrl+a`, `cmd+shift+space`. Never localise: it is the contract.
    pub fn id(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.cmd {
            parts.push("cmd");
        }
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        let mods = parts.join("+");
        if mods.is_empty() {
            self.key.clone()
        } else {
            format!("{mods}+{}", self.key)
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let mut key = None;
        let (mut cmd, mut ctrl, mut alt, mut shift) = (false, false, false, false);
        for part in id.split('+') {
            match part.trim() {
                "cmd" | "command" | "super" => cmd = true,
                "ctrl" | "control" => ctrl = true,
                "alt" | "opt" | "option" => alt = true,
                "shift" => shift = true,
                "" => {}
                other => key = Some(other.to_string()),
            }
        }
        let key = key?;
        // Only accept a key this build can actually register.
        key_to_vk(&key)?;
        Some(Hotkey::new(key, cmd, ctrl, alt, shift))
    }
}

/// The historical default, ⌃A — the chord the welcome note and every past build
/// taught. The user can change it, or turn the global hotkey off entirely, in
/// Settings.
pub fn default_toggle_shortcut() -> Option<Hotkey> {
    Some(Hotkey::new("a", false, true, false, false))
}

/// ANSI (US) virtual key codes. A chord is only registrable if its key is here;
/// the table covers the letters, digits, the main punctuation, space, the
/// arrows and the function keys — everything a sensible global chord uses.
pub fn key_to_vk(key: &str) -> Option<u32> {
    let code: u32 = match key {
        "a" => 0, "s" => 1, "d" => 2, "f" => 3, "h" => 4, "g" => 5, "z" => 6, "x" => 7,
        "c" => 8, "v" => 9, "b" => 11, "q" => 12, "w" => 13, "e" => 14, "r" => 15, "y" => 16,
        "t" => 17, "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22, "5" => 23, "=" => 24,
        "9" => 25, "7" => 26, "-" => 27, "8" => 28, "0" => 29, "]" => 30, "o" => 31, "u" => 32,
        "[" => 33, "i" => 34, "p" => 35, "l" => 37, "j" => 38, "'" => 39, "k" => 40, ";" => 41,
        "\\" => 42, "," => 43, "/" => 44, "n" => 45, "m" => 46, "." => 47, "`" => 50,
        "space" => 49, "tab" => 48, "return" | "enter" => 36,
        "left" => 123, "right" => 124, "down" => 125, "up" => 126,
        "home" => 115, "end" => 119, "pageup" => 116, "pagedown" => 121,
        "f1" => 122, "f2" => 120, "f3" => 99, "f4" => 118, "f5" => 96, "f6" => 97, "f7" => 98,
        "f8" => 100, "f9" => 101, "f10" => 109, "f11" => 103, "f12" => 111,
        _ => return None,
    };
    Some(code)
}

/// The key as it should read in a chord label: an uppercase letter, a named
/// glyph for the whitespace and arrows, otherwise the character itself.
fn key_display(key: &str) -> String {
    match key {
        "space" => "Space".into(),
        "tab" => "⇥".into(),
        "return" | "enter" => "⏎".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "⇞".into(),
        "pagedown" => "⇟".into(),
        _ if key.len() == 1 => key.to_uppercase(),
        _ => key.to_uppercase(),
    }
}

/// The window's saved frame — origin and size in the global display coordinate
/// space, in points. Persisted so the window reopens where it was left instead
/// of always centering. Held as plain numbers, not a GPUI type, because this
/// module stays free of `gpui` so it can be unit-tested on its own; `main`
/// converts to and from `Bounds` at the one seam that touches the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl WindowFrame {
    /// True when every field is finite and the size is positive. A frame that
    /// fails this is discarded on read rather than reopening the window at a
    /// zero or NaN size, which no display could show.
    fn is_sane(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.w > 0.
            && self.h > 0.
    }
}

/// Which palette the note is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    /// Follow the Mac. The default, because an app that opens white at midnight
    /// on a machine that has been dark since morning is a flashbang.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub fn id(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }

    pub fn from_id(id: &str) -> Self {
        match id.trim().to_ascii_lowercase().as_str() {
            "light" => Appearance::Light,
            "dark" => Appearance::Dark,
            _ => Appearance::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    /// The next one, for a control that cycles through the three.
    pub fn next(self) -> Self {
        match self {
            Appearance::System => Appearance::Light,
            Appearance::Light => Appearance::Dark,
            Appearance::Dark => Appearance::System,
        }
    }
}

/// The live window material.
///
/// These mirror the complete material matrix in the sibling
/// `gpui-liquid-glass` reference: Apple's two glass variants, each with an
/// optional tint, plus Identity as the no-effect sentinel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlassStyle {
    #[default]
    Regular,
    Clear,
    RegularTinted,
    ClearTinted,
    Identity,
}

impl GlassStyle {
    pub fn id(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Clear => "clear",
            Self::RegularTinted => "regular-tinted",
            Self::ClearTinted => "clear-tinted",
            Self::Identity => "identity",
        }
    }

    pub fn from_id(id: &str) -> Self {
        match id.trim().to_ascii_lowercase().as_str() {
            "clear" => Self::Clear,
            "regular-tinted" => Self::RegularTinted,
            "clear-tinted" => Self::ClearTinted,
            "identity" | "solid" | "off" => Self::Identity,
            _ => Self::Regular,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Regular => "Regular",
            Self::Clear => "Clear",
            Self::RegularTinted => "Regular Tinted",
            Self::ClearTinted => "Clear Tinted",
            Self::Identity => "Identity",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Regular => Self::Clear,
            Self::Clear => Self::RegularTinted,
            Self::RegularTinted => Self::ClearTinted,
            Self::ClearTinted => Self::Identity,
            Self::Identity => Self::Regular,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub text_size: f32,
    /// Light, dark, or whatever the Mac is set to.
    pub appearance: Appearance,
    /// Regular or Clear liquid glass, optionally tinted; Identity is no effect.
    pub glass: GlassStyle,
    /// Where `note.md` lives, when it is not in Application Support. The whole
    /// of "sync": the note is plain markdown in a normal directory, and putting
    /// that directory in iCloud Drive or Dropbox is the user's own choice of
    /// tool. `None` means the default location.
    pub note_folder: Option<std::path::PathBuf>,
    /// The global show/hide chord, or `None` when the user has turned it off.
    pub toggle_shortcut: Option<Hotkey>,
    /// The last window frame, or `None` until the window has been saved once.
    /// Restored on launch, clamped to a display that still exists.
    pub window: Option<WindowFrame>,
    /// The caret's byte offset when the window last closed, so a relaunch opens
    /// where you left off rather than at the top. Clamped to the note on restore.
    pub caret: usize,
    /// The top visible line when the window last closed, so the reading position
    /// is restored, not just the caret. Clamped to the line count on restore.
    pub scroll_top: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            text_size: DEFAULT_TEXT_SIZE,
            appearance: Appearance::System,
            glass: GlassStyle::Regular,
            note_folder: None,
            toggle_shortcut: default_toggle_shortcut(),
            window: None,
            caret: 0,
            scroll_top: 0,
        }
    }
}

impl Settings {
    /// Read from disk, falling back to defaults for anything missing or broken.
    pub fn load(path: &Path) -> Self {
        let mut settings = Self::default();
        let Ok(contents) = std::fs::read_to_string(path) else {
            return settings;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "note_folder" => {
                    // Kept whether or not it is there right now: an external
                    // drive that is not mounted yet, or an iCloud folder that
                    // has not materialised, is not a reason to forget where the
                    // note lives — and forgetting would send the app back to
                    // Application Support, quietly open the stale copy left
                    // there, and then erase the setting on the next save.
                    settings.note_folder =
                        (!value.is_empty()).then(|| std::path::PathBuf::from(value));
                }
                "appearance" => {
                    settings.appearance = Appearance::from_id(value);
                }
                "glass" => {
                    settings.glass = GlassStyle::from_id(value);
                }
                "text_size" => {
                    if let Ok(size) = value.parse::<f32>() {
                        settings.text_size = clamp_text_size(size);
                    }
                }
                "toggle_shortcut" => {
                    // `off` (or any word without a registrable key) turns the
                    // global chord off rather than poisoning the setting.
                    settings.toggle_shortcut = if value.eq_ignore_ascii_case("off") {
                        None
                    } else {
                        Hotkey::from_id(value).or(settings.toggle_shortcut)
                    };
                }
                "window" => {
                    // `x,y,w,h`. A malformed or insane frame is dropped, not
                    // poisoned: the launch code centres when it is `None`, so a
                    // broken line simply forgets the position rather than opening
                    // the window somewhere impossible.
                    settings.window = parse_window_frame(value);
                }
                "caret" => {
                    if let Ok(n) = value.parse::<usize>() {
                        settings.caret = n;
                    }
                }
                "scroll_top" => {
                    if let Ok(n) = value.parse::<usize>() {
                        settings.scroll_top = n;
                    }
                }
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let shortcut = match &self.toggle_shortcut {
            Some(hotkey) => hotkey.id(),
            None => "off".to_string(),
        };
        let mut out = format!(
            "text_size = {}\ntoggle_shortcut = {shortcut}\nappearance = {}\nglass = {}\n",
            self.text_size,
            self.appearance.id(),
            self.glass.id(),
        );
        if let Some(folder) = &self.note_folder {
            out.push_str(&format!("note_folder = {}\n", folder.display()));
        }
        if let Some(w) = self.window {
            // Round to whole points: the window only ever sits on point
            // boundaries, and a tidy line is easier to eyeball or hand-edit.
            out.push_str(&format!(
                "window = {},{},{},{}\n",
                w.x.round(),
                w.y.round(),
                w.w.round(),
                w.h.round()
            ));
        }
        // The reading position. Zero is the default, so an untouched note writes
        // nothing extra and reopens at the top.
        if self.caret != 0 || self.scroll_top != 0 {
            out.push_str(&format!(
                "caret = {}\nscroll_top = {}\n",
                self.caret, self.scroll_top
            ));
        }
        crate::persist::write_atomically_bytes(path, out.as_bytes())
    }

    /// One notch larger, saturating at [`MAX_TEXT_SIZE`].
    pub fn bigger(&mut self) {
        self.text_size = clamp_text_size(self.text_size + TEXT_SIZE_STEP);
    }

    /// One notch smaller, saturating at [`MIN_TEXT_SIZE`].
    pub fn smaller(&mut self) {
        self.text_size = clamp_text_size(self.text_size - TEXT_SIZE_STEP);
    }

    pub fn reset_text_size(&mut self) {
        self.text_size = DEFAULT_TEXT_SIZE;
    }
}

/// Parse a `x,y,w,h` window frame, or `None` when it is malformed or insane.
fn parse_window_frame(value: &str) -> Option<WindowFrame> {
    let mut nums = value.split(',').map(|p| p.trim().parse::<f32>());
    let x = nums.next()?.ok()?;
    let y = nums.next()?.ok()?;
    let w = nums.next()?.ok()?;
    let h = nums.next()?.ok()?;
    if nums.next().is_some() {
        return None; // too many fields — treat as garbage
    }
    let frame = WindowFrame { x, y, w, h };
    frame.is_sane().then_some(frame)
}

/// Clamp to the supported range, mapping NaN to the default rather than
/// poisoning every later layout calculation.
pub fn clamp_text_size(size: f32) -> f32 {
    if size.is_nan() {
        DEFAULT_TEXT_SIZE
    } else {
        size.clamp(MIN_TEXT_SIZE, MAX_TEXT_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gravitynote-settings-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("settings.txt")
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = scratch("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn round_trips() {
        let path = scratch("roundtrip");
        let mut settings = Settings::default();
        settings.bigger();
        settings.bigger();
        settings.glass = GlassStyle::ClearTinted;
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.text_size, settings.text_size);
        assert_eq!(loaded.glass, GlassStyle::ClearTinted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn glass_styles_have_stable_ids_and_cycle() {
        let styles = [
            GlassStyle::Regular,
            GlassStyle::Clear,
            GlassStyle::RegularTinted,
            GlassStyle::ClearTinted,
            GlassStyle::Identity,
        ];
        for style in styles {
            assert_eq!(GlassStyle::from_id(style.id()), style);
        }
        let mut style = GlassStyle::Regular;
        for expected in [
            GlassStyle::Clear,
            GlassStyle::RegularTinted,
            GlassStyle::ClearTinted,
            GlassStyle::Identity,
            GlassStyle::Regular,
        ] {
            style = style.next();
            assert_eq!(style, expected);
        }
    }

    #[test]
    fn reading_position_round_trips() {
        let path = scratch("position");
        let mut settings = Settings::default();
        settings.caret = 1234;
        settings.scroll_top = 56;
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.caret, 1234);
        assert_eq!(loaded.scroll_top, 56);
        // The default (0, 0) writes nothing and reads back as 0.
        Settings::default().save(&path).unwrap();
        let zero = Settings::load(&path);
        assert_eq!((zero.caret, zero.scroll_top), (0, 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_content_never_fails_to_open() {
        let path = scratch("malformed");
        for junk in [
            "",
            "garbage",
            "text_size =",
            "text_size = abc",
            "text_size = NaN",
            "\u{0}\u{1}\u{2}",
            "# comment only",
            "other_key = 4",
        ] {
            std::fs::write(&path, junk).unwrap();
            let loaded = Settings::load(&path);
            assert!(
                (MIN_TEXT_SIZE..=MAX_TEXT_SIZE).contains(&loaded.text_size),
                "junk {junk:?} produced {loaded:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn out_of_range_values_are_clamped_on_read_and_on_step() {
        let path = scratch("clamp");
        std::fs::write(&path, "text_size = 9999").unwrap();
        assert_eq!(Settings::load(&path).text_size, MAX_TEXT_SIZE);
        std::fs::write(&path, "text_size = -5").unwrap();
        assert_eq!(Settings::load(&path).text_size, MIN_TEXT_SIZE);
        let _ = std::fs::remove_file(&path);

        let mut settings = Settings::default();
        for _ in 0..200 {
            settings.bigger();
        }
        assert_eq!(settings.text_size, MAX_TEXT_SIZE);
        for _ in 0..400 {
            settings.smaller();
        }
        assert_eq!(settings.text_size, MIN_TEXT_SIZE);
        settings.reset_text_size();
        assert_eq!(settings.text_size, DEFAULT_TEXT_SIZE);
    }

    #[test]
    fn hotkey_round_trips_and_ignores_junk() {
        let path = scratch("shortcut");
        let mut settings = Settings::default();
        settings.toggle_shortcut = Some(Hotkey::new("space", true, false, false, true));
        settings.save(&path).unwrap();
        assert_eq!(
            Settings::load(&path).toggle_shortcut,
            Some(Hotkey::new("space", true, false, false, true))
        );

        // An unknown key keeps the previous value rather than poisoning it.
        std::fs::write(&path, "toggle_shortcut = ctrl+notakey").unwrap();
        assert_eq!(
            Settings::load(&path).toggle_shortcut,
            default_toggle_shortcut()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn off_disables_the_global_chord() {
        let path = scratch("off");
        std::fs::write(&path, "toggle_shortcut = off").unwrap();
        assert_eq!(Settings::load(&path).toggle_shortcut, None);
        let mut settings = Settings::default();
        settings.toggle_shortcut = None;
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).toggle_shortcut, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_id_round_trips_and_maps_to_a_keycode() {
        for hotkey in [
            Hotkey::new("a", false, true, false, false),
            Hotkey::new("space", false, false, true, false),
            Hotkey::new("space", false, true, false, false),
            Hotkey::new("space", true, false, false, true),
            Hotkey::new("`", false, true, false, false),
        ] {
            assert!(hotkey.carbon().is_some(), "{hotkey:?} has no keycode");
            assert_eq!(Hotkey::from_id(&hotkey.id()), Some(hotkey));
        }
    }

    #[test]
    fn labels_read_in_apple_order() {
        assert_eq!(Hotkey::new("a", false, true, false, false).label(), "⌃A");
        assert_eq!(
            Hotkey::new("space", true, false, false, true).label(),
            "⇧⌘Space"
        );
    }

    #[test]
    fn window_frame_round_trips() {
        let path = scratch("window-roundtrip");
        let mut settings = Settings::default();
        settings.window = Some(WindowFrame {
            x: 120.,
            y: 64.,
            w: 574.,
            h: 760.,
        });
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).window, settings.window);
        // The other settings still round-trip alongside it.
        assert_eq!(Settings::load(&path).text_size, settings.text_size);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn window_frame_absent_by_default_and_when_omitted() {
        let path = scratch("window-absent");
        // A file that never mentions a window leaves the frame `None`, so the
        // launch code centres.
        std::fs::write(&path, "text_size = 15\n").unwrap();
        assert_eq!(Settings::load(&path).window, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_window_frame_is_dropped_not_poisoned() {
        let path = scratch("window-junk");
        for junk in [
            "window =",
            "window = 1,2,3",           // too few
            "window = 1,2,3,4,5",       // too many
            "window = a,b,c,d",         // not numbers
            "window = 1,2,0,10",        // zero width
            "window = 1,2,10,-5",       // negative height
            "window = NaN,2,10,10",     // not finite
            "window = 1,2,10,10junk",   // trailing garbage on a field
        ] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(
                Settings::load(&path).window,
                None,
                "junk {junk:?} should drop the frame"
            );
            // And it must never stop the rest of the file loading.
            assert!(
                (MIN_TEXT_SIZE..=MAX_TEXT_SIZE).contains(&Settings::load(&path).text_size)
            );
        }
        // A good frame beside junk on other lines still loads.
        std::fs::write(&path, "garbage\nwindow = 10,20,300,400\n").unwrap();
        assert_eq!(
            Settings::load(&path).window,
            Some(WindowFrame {
                x: 10.,
                y: 20.,
                w: 300.,
                h: 400.
            })
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_sit_beside_the_note() {
        let path = settings_path();
        assert_eq!(path.file_name().unwrap(), "settings.txt");
        assert_eq!(
            path.parent(),
            crate::note::default_note_path().parent(),
            "settings must live in the same directory as the note"
        );
    }
}
