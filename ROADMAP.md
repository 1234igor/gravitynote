# Roadmap

These are open ideas, not scheduled releases. See the [README](README.md) for
current features and usage.

## Planned

- **Drag to reorder notes.** The drag handle must not interfere with text
  selection.
- **Fold long notes.** Show the first line while an entry is collapsed.
- **Reduce fence-map work after edits.** An unclosed code fence can require a
  scan through trailing lines. Any incremental replacement needs differential
  tests against a full rebuild.

## Design decisions

- **One Markdown file.** Explicit thematic breaks separate entries; blank lines
  remain part of an entry.
- **Direct text editing.** Keep Markdown editable rather than hiding formatting
  behind a live preview. Images render inline and expose their Markdown on the
  caret line.
- **Sync through a chosen folder.** Use a filesystem sync provider rather than
  an app account or a built-in sync service. External file changes are already
  reloaded; conflicting versions are preserved in backups.
- **Optimize measured bottlenecks.** Rendering uses visible rows, and edits patch
  the line index and fence map. A rope buffer remains deferred until measurements
  justify replacing the current text buffer.

## Verification

The test suite covers document operations, undo history, search, persistence,
text layout calculations, and incremental caches. The large-note benchmark is
in `tests/huge_note.rs`.

Window behavior, text shaping, menus, and interactions also need checks in the
running app. See [CONTRIBUTING.md](CONTRIBUTING.md).
