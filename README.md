# GravityNote

A macOS notes app built in Rust with [GPUI](https://gpui.rs). Keep your notes in
one Markdown file. Add new entries at the top and bring older entries back when
you need them.

<p>
  <a href="https://apps.apple.com/app/id6814392510">
    <img src="https://developer.apple.com/assets/elements/badges/download-on-the-app-store.svg" alt="Download GravityNote on the App Store" height="48">
  </a>
</p>

## Features

- Markdown highlighting, soft wrapping, lists, and checkboxes.
- Inline images: paste or drop an image, then drag its corner to resize it.
- Find and replace, undo and redo, and a searchable command palette.
- A global show/hide shortcut and a menu-bar icon.
- Light, dark, and system appearance; adjustable text size.
- Autosave, automatic backups, and a backup browser for restoring earlier notes.
- Choose where to store your notes, including a folder managed by a sync service.

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

Press Command-N to start a note. Entries are separated by `---`; blank lines
stay within an entry. Click the `^` on a separator to bring the entry below it
to the top.

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

On first launch, choose a folder for your notes. Your notes save automatically
as `note.md` in that folder, with `images/` and `backups/` alongside it.
Preferences and bookmarks remain in Application Support.

Older notes, images and backups in Application Support are copied into the
chosen folder. Originals are preserved, and different existing files are never
overwritten. Canceling the folder picker exits without changing your files.

Changed notes are backed up hourly. Older backups are kept as daily and monthly
copies for up to two years. Open **Restore from Backup** in the command palette
to browse them.

**Change Note Folder** selects another directory, including one managed by a
sync service. If it contains `note.md`, the app opens that note; otherwise, it
copies the current text there. Images move to the selected folder. New backups
are written there, while earlier backups remain in the old folder. Settings
stay in Application Support. Keep the `images` directory with the note when
copying it yourself.

Changes made in another editor reload automatically. If you also have unsaved
edits, the external version goes into a backup before your edits are saved.
Autosave pauses if that backup fails.

See the [privacy policy](PRIVACY.md) for data handling.

## Development

[Contributing](CONTRIBUTING.md) · [Roadmap](ROADMAP.md).

## License

The code is [MIT licensed](LICENSE). Dependencies and fonts retain their own
licenses; see [THIRD-PARTY.md](THIRD-PARTY.md).

The GravityNote name and app icon are excluded from the code license. Use your
own name and icon when distributing a fork.
