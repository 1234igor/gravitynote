//! Which copy of the app this is.
//!
//! There are two on this Mac: the one in `/Applications` holding years of real
//! notes, and the one being worked on. They must not share a note file, a
//! settings file, a backup directory, or the global hotkey — a development
//! build that autosaves over the real note has destroyed something no test
//! catches.
//!
//! The question is asked of the executable itself rather than passed in at
//! launch. `open` forwards arguments to a bundle but not environment, and the
//! app can also be started from the Finder, from Spotlight, or by the system
//! at login — a marker that only survives one of those launch paths is a marker
//! that will one day let a dev build write to the real note. The name of the
//! running executable survives all of them.
//!
//! `package.sh --dev` builds `dist/GravityNoteDev.app`, whose executable is
//! `gravitynote-gpui-dev`. `GRAVITYNOTE_DEV=1` says the same thing for
//! `cargo run` and for tests, which have no bundle at all.

/// True when this is the development build, which keeps its own data beside the
/// real note's and never claims the global hotkey.
pub fn is_dev() -> bool {
    if std::env::var("GRAVITYNOTE_DEV").is_ok_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    std::env::current_exe().is_ok_and(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with("-dev"))
    })
}

/// Force controls that are normally only visible on hover or selection to draw.
///
/// Set `GRAVITYNOTE_SHOW_HANDLES=1`. The point of it is that a handle nobody has
/// looked at is a handle nobody has designed: synthetic mouse input cannot reach
/// this app (no Accessibility permission), so without a way to pin a transient
/// control on screen it can only be reviewed as source code — which is how a
/// resize grip once shipped as a fat white square.
pub fn show_handles() -> bool {
    std::env::var("GRAVITYNOTE_SHOW_HANDLES").is_ok_and(|v| !v.is_empty() && v != "0")
}
