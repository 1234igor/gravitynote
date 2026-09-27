# GravityNote

GravityNote is a macOS app that keeps your notes in one Markdown file. Add a note
at the top, search the whole document, and bring older notes back to the top when
you want to revisit them. It is built in Rust with [GPUI](https://gpui.rs).

![GravityNote editor](docs/screenshot.png)

## Features

- Markdown highlighting, soft wrapping, lists, and checkboxes.
- Inline images: paste or drop an image, then drag its corner to resize it.
- Find and replace, undo and redo, and a searchable command palette.
- A global show/hide shortcut and a menu-bar icon.
- Light, dark, and system appearance; adjustable text size and glass background.
- Autosave, automatic backups, and a backup browser for restoring earlier notes.
- A configurable note folder and reloading of changes made by another editor.

The workflow is inspired by Andrej Karpathy’s
[append-and-review note](https://karpathy.bearblog.dev/the-append-and-review-note/).

## Build and run

Use macOS with Rust stable and Xcode build tools installed. From the repository
root:

```sh
./run.sh
```

This builds and opens `dist/GravityNote.app`. To build without launching, run
`./package.sh`. Add `--install` to copy the app into `/Applications`, or `--zip`
to create an archive. These local builds use an ad-hoc signature.

For development, use `./dev.sh`. It opens a separate development app with its
own default data directory and no global hotkey. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Using your note

Separate entries with a Markdown thematic break such as `---`. Blank lines stay
within an entry. Click the `^` on a separator to bring the entry below it to the
top. The editor also recognizes `***` and `___` as separators outside fenced
code blocks.

| Default shortcut | Action |
|---|---|
| Control-A | Show or hide GravityNote from any app |
| Command-N | Add a note at the top |
| Command-Control-Shift-Up | Bring the current note to the top |
| Command-Control-Up | Move the current note up one place |
| Command-F | Find |
| Command-Option-F | Show find and replace |
| Command-Shift-P | Open the command palette |
| Command-comma | Open settings |
| Command-S | Back up now |
| Command-Shift-T | Add or remove a task checkbox |
| Command-Return | Toggle task completion |
| Tab / Shift-Tab | Indent / outdent |
| Command-plus / Command-minus / Command-0 | Increase / decrease / reset text size |
| Command-Z / Command-Shift-Z | Undo / redo |

Change or disable the global shortcut in Settings. If another app has already
registered it, use the menu-bar icon to open GravityNote.

## Files and backups

The default data directory is:

```text
~/Library/Application Support/gravitynote-gpui/
├── note.md
├── settings.txt
├── images/
└── backups/note-YYYY-MM-DD_HH-MM-SS.md
```

The app saves about 400 ms after you stop typing. It writes a temporary file and
replaces the previous file after the write completes.

Automatic backups run hourly when the note has changed. The retention policy
keeps all backups from the last two days, then one per day for a month, then one
per month for about two years, with a limit of 3,000 files. Use **Restore from
Backup** in the command palette to browse them.

**Change Note Folder** selects another directory, including one managed by a
sync service. If it contains `note.md`, the app opens that note; otherwise, it
copies the current text there. Images move to the selected folder. New backups
are written there, while earlier backups remain in the old folder. Settings
stay in Application Support. Keep the `images` directory with the note when
copying it yourself.

If the file changes while you have unsaved edits, GravityNote backs up the
external version before saving your edits. Autosave pauses if that backup fails.
This handles competing copies; it does not merge their text.

See the [privacy policy](PRIVACY.md) for data handling.

## Development

[CONTRIBUTING.md](CONTRIBUTING.md) covers development builds and checks.
[ROADMAP.md](ROADMAP.md) lists remaining work and design decisions.

The document code lives in `src/`. Rendering and window integration use the
vendored GPUI fork; its changes are listed in
[vendor/gpui/MODIFICATIONS.md](vendor/gpui/MODIFICATIONS.md).

To measure document operations on a generated large note:

```sh
cargo test --release --test huge_note -- --nocapture
```

These timings measure document operations, not end-to-end typing latency or
rendering. Results depend on the machine and build.

## License

The code is [MIT licensed](LICENSE). Dependencies and fonts retain their own
licenses; see [THIRD-PARTY.md](THIRD-PARTY.md).

The GravityNote name and app icon are excluded from the code license. Use your
own name and icon when distributing a fork.
