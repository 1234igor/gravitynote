//! GravityNote — one markdown file, many notes, separated by `---`.
//!
//! The crate is split so the document layer can be tested and benchmarked
//! without a window: the document modules stay pure while [`theme`] and
//! [`platform`] own rendering and macOS integration.
//!
//! | Module | Role |
//! |--------|------|
//! | [`note`] | The document: text buffer, lines, and `---`-separated blocks |
//! | [`dev`] | Whether this is the development copy, which keeps its own data |
//! | [`caret`] | The caret's fade rhythm, and when it next needs drawing |
//! | [`rows`] | Visual-row caret motion, wrap affinity, and hit-testing |
//! | [`lists`] | Indenting a line, and turning it into a task |
//! | [`index`] | O(log n) line / UTF-16 index so edits stay cheap on huge notes |
//! | [`markdown`] | Per-line syntax highlighting into contiguous styled spans |
//! | [`fences`] | Which lines sit inside a ``` block, patched incrementally |
//! | [`history`] | Undo/redo over spans, with typing coalesced into groups |
//! | [`find`] | Case-insensitive search over the whole note |
//! | [`selection`] | Word / paragraph ranges for multi-click and word motion |
//! | [`images`] | The content-addressed image store, and how a note refers to it |
//! | [`image_bank`] | Decoded images, downscaled and bounded |
//! | [`persist`] | Atomic autosave and hourly rolling backups |
//! | [`settings`] | Persisted text, appearance, glass, shortcut, and window preferences |
//! | [`corpus`] | Deterministic note generator used by the performance tests |
//! | [`theme`] | Palette and the markdown-style → text-attribute mapping |
//! | [`platform`] | macOS menu-bar item and the global show/hide hotkey |
//! | [`login_item`] | Whether the app opens at login |

pub mod caret;
pub mod corpus;
pub mod dev;
pub mod image_bank;
pub mod images;
pub mod fences;
pub mod find;
pub mod history;
pub mod index;
pub mod lists;
pub mod login_item;
pub mod markdown;
pub mod note;
pub mod persist;
pub mod rows;
pub mod platform;
pub mod selection;
pub mod settings;
pub mod theme;

pub mod sandbox;
