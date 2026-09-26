# Roadmap

What GravityNote does, what it will do, and what it deliberately will not.
"Considered — not doing" entries keep their reason, because the reason is the
useful part.

## The note model

| Feature | Status | Notes |
|---|---|---|
| One markdown file holds every note | Done | `~/Library/Application Support/gravitynote-gpui/note.md` |
| Notes separated by a `---` thematic break | Done | `note::is_separator_line` |
| Notes separated by a run of blank lines | Considered — not doing | Removed. It made the app guess where a note ended from how the text was spaced; `---` is the only boundary, and it is one you can see and delete |
| A note is many lines, not one line | Done | `note::Block` |
| New note at the top (⌘N) | Done | |
| Bring the current note to the top (⌘⇧↑) | Done | |
| Move the current note up one place (⌘⌃↑) | Done | |
| `^` on a rule promotes the note below it | Done | Quiet on the rule; soft chip and accent on hover |
| Reorder notes by dragging | Planned | Needs a drag affordance that does not compete with text selection |
| Fold a long note to its first line | Planned | |

## Editing

| Feature | Status | Notes |
|---|---|---|
| Full text editing with IME support | Done | |
| Soft wrap | Done | Variable-height rows via `ListState` |
| Undo / redo, typing coalesced (⌘Z / ⌘⇧Z) | Done | `history`, spans not snapshots. Note moves record the run that changed (`Edit::between`), so undo crosses them instead of corrupting the buffer |
| Double-click word, triple-click paragraph | Done | `selection`, UAX #29 |
| Word motion and word delete (⌥←→, ⌥⌫) | Done | |
| Delete to line start (⌘⌫) | Done | |
| Document start / end (⌘↑ / ⌘↓) | Done | |
| Find in note (⌘F) | Done | At the top of the window, with a close ✕ and the caret in the query. Seeded from the selection. ⏎ / ⇧⏎, ↓ / ↑ or ⌘G / ⌘⇧G to cycle. While it is open it owns the keyboard, so no note command edits behind it |
| Move the caret by visual row, not logical line | Done | ↑/↓ step one visual row and keep a goal column, so walking past a short row and back returns to the column you left |
| Line start / end by visual row (⌘← / ⌘→) | Done | Same reason: on a wrapped line the logical ends are off-screen |
| Caret affinity at a soft wrap | Done | A wrap offset is both the end of one row and the start of the next; the caret tracks which, so it draws at the head of a wrapped row instead of the tail of the row above |
| Indent / outdent a line or a selection (⇥ / ⇧⇧⇥) | Done | Two spaces a level, tabs read as one level. One edit for the whole selection, so one ⌘Z takes it back. `lists` |
| Make a line a task, and complete it (⌘⇧T / ⌘⏎) | Done | The box goes on and off; the bullet stays, including an ordered item's number, which could not be recovered. Completing leaves non-tasks alone |
| Enter keeps the indentation you are working at | Done | For plain indented lines as well as list items, so a hand-made outline does not jump back to the margin |
| Auto-continue lists on Enter | Done | Bullets, ordered lists (counting up), tasks (carried over unticked) and quotes; a second Enter on an empty item leaves the list. Never inside a fenced code block. `markdown::continuation` |
| Drag to select across a scroll boundary | Done | Past the edge the text scrolls to the pointer, faster the further out it goes, and keeps going while the pointer is held still |

## Appearance

| Feature | Status | Notes |
|---|---|---|
| Light, paper-white theme | Done | |
| Markdown syntax colouring | Done | Headings, emphasis, code, links, lists, tasks, quotes, fences |
| Real bold / italic faces | Done | Lilex Regular, SemiBold, Bold, Italic, BoldItalic embedded |
| Headings at their own size | Done | Scale relative to body size, so it moves with the text-size setting |
| Caret with a rhythm rather than a blink | Done | Solid while you work, then a slow fade out and back. The old version toggled on every 200 ms tick, which read as a flicker |
| Scroll position indicator | Done | |
| Reading measure, centred | Done | |
| No in-window chrome | Done | Commands live in the menu bar. The traffic lights are hidden until the pointer enters the top strip, and arrive on a plain white bar across the window — no rule under it, and the first line clears it so nothing is sliced |
| Shrinks to a scratch pad | Done | The window goes down to 180×120 |
| One continuous rule between notes | Done | The `---` text stays present and editable but invisible |
| Dark theme | Done | System / Light / Dark, following the Mac by default |
| Live preview (hide markup off the caret line) | Considered — not doing | Fights direct editing of a plain file; the whole point is that the file is the truth |

## Chrome and system integration

| Feature | Status | Notes |
|---|---|---|
| Menu-bar status item | Done | SF Symbol `note.text` |
| Global ⌃A show / hide | Done | Carbon hotkey, no Accessibility permission needed |
| macOS menu bar (App / Note / Edit) | Done | |
| Settings: text size (⌘+ / ⌘− / ⌘0) | Done | Persisted to `settings.txt` |
| Closing the window hides rather than quits | Done | |
| A `.app` to drag into /Applications | Done | `./package.sh` builds `dist/GravityNote.app`; `--install` copies it to /Applications, `--zip` makes an archive. Ad-hoc signed, fonts are in the binary, no dylibs outside the OS — verified by relaunching a copy from another directory |
| Open at login | Done | `SMAppService` against the app's own bundle — no helper app, no launch agent. The system owns the setting, so the menu reads it back rather than remembering it. `login_item` |
| One line of feedback at the foot of the window | Done | Alerts (a failed save) stay until resolved; remarks (⌘S naming the backup it wrote) fade after 4 s. Until now ⌘S said nothing at all |
| Dock icon badge | Considered — not doing | Nothing to count |

## Storage

| Feature | Status | Notes |
|---|---|---|
| Debounced autosave | Done | 400 ms after the last keystroke |
| Atomic writes | Done | Temp file in the same directory, fsync, rename |
| Refuse to save after a failed load | Done | A read error must never overwrite the file — including a lossy UTF-8 decode, which is a read error and used to be written back over the original |
| Hourly rolling backups, 48 kept | Done | |
| Back up now (⌘S) | Done | |
| Reload when the file changes on disk | Done | And the folder is configurable, which is the whole of sync |
| iCloud / Dropbox sync | Considered — not doing | The file is plain markdown in a normal directory; sync is the user's own choice of tool |

## Performance

| Feature | Status | Notes |
|---|---|---|
| Virtualized rendering | Done | Only visible rows are shaped and highlighted |
| O(log n) line and UTF-16 index | Done | `index::LineIndex` |
| Incremental index splice | Done | 62 µs vs a 5.9 ms rebuild |
| Incremental fence map | Done | 24 ns per keystroke |
| 20-year corpus benchmark | Done | `tests/huge_note.rs`, `corpus::twenty_years()` |
| The frame scales with the text size | Done | Margins, the top inset and the reading measure are multiples of the text size, so ⌘+ zooms one page instead of resizing text inside fixed chrome. The measure holds 74 characters at every size; fixed at 720px it gave 100 at the smallest and 37 at the largest |
| Images | Done | Paste or drop one and it lands in a content-addressed store beside the note, referenced as `![\|WxH](images/<hash>.png)`. Renders inline, resizes by a corner grip, and shows its markdown when the caret lands on the line. Resizing is a text edit, so undo, autosave and backups need no new code |
| A dev copy that cannot touch the real note | Done | `./dev.sh` builds `GravityNoteDev.app`: same binary, DEV ribbon on the icon, own data directory, no global chord, never installed. `src/dev.rs` reads its own executable name, so the answer survives `open`, the Finder and Spotlight alike |
| The first line's breathing room scrolls away | Done | GPUI's `list` offsets the first visible item by `padding.top` at every scroll position, so the top inset was a fixed band across the window — text vanished into it mid-scroll while the bottom edge ran clean. The space now rides on row 0, and is half what it was |
| Promoting a note records the document above it into undo | Done | A move is a cut and a paste: 104 bytes of history rather than eighteen megabytes. `Edit::between` shares no prefix when byte 0 changes, so ⌘⇧↑ on an old note would otherwise record everything above it — twice, on a twenty-year note |
| Deleting at a note's edge takes the whole rule | Done | One ⌫ merges the notes instead of revealing `---` a character at a time, and ⌥⌫ can no longer reach back through a separator into the note above |
| ⌘⌫ deletes to the start of the visual row | Done | Matches ⌘←, which has always moved there; a wrapped paragraph no longer loses 400 characters to one keystroke |
| Word motion stops at emoji and symbols | Done | A double-click already selected one on its own; `selection` now agrees with itself, with whitespace as its own kind so it stays crossable |
| Pasted line endings are normalised | Done | A lone `\r` is not a line break to the note model, so a document from an older source arrived as one unsplittable line |
| Starting a note is a small edit | Done | ⌘N prepends the rule instead of taking the document apart and putting it back, which rewrote every `***` into `---` and landed in undo as one enormous step |
| `first_unclosed` recomputed per splice | Planned | While a fence is open, every keystroke walks back over the trailing run: 60 µs on a 214,000-line note, against 36 ns when the fence is closed. Not a freeze, and the incremental version has a silent-corruption failure mode, so it wants the same differential test the other two caches have before it is written |
| Rope buffer | Considered — not doing | The memmove is not the bottleneck at 9 MB |

## Quality

| Feature | Status | Notes |
|---|---|---|
| Differential tests for both incremental caches | Done | Thousands of random edits vs a rebuild |
| Property tests for the highlighter's span tiling | Done | 123 adversarial cases, every prefix and suffix |
| Undo round-trip property test | Done | 5,000 random edits |
| Tests for the row arithmetic | Done | `rows`: visual-row motion, wrap affinity and hit-testing, against a monospace fake with known wraps. This is where every caret bug has lived |
| Tests for the rest of the view layer | Considered — not doing | What is left in `main.rs` is delegation to tested modules, or GPUI and AppKit behaviour that only running the app verifies — menu key equivalents bypassing the key context, real text shaping, `ListState`. A seam for those would be indirection for its own sake |
| Reload when the file changes on disk | Planned | Matters as soon as the file is synced between machines. A note deleted underneath the app is already rewritten on the next save |
