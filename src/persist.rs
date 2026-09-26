//! Durable persistence for the single note, plus rolling hourly backups.
//!
//! # Why this module exists
//!
//! The note is the whole app: a single file at
//! `~/Library/Application Support/gravitynote-gpui/note.md`. A naive
//! `fs::write` on every keystroke opens the file with `O_TRUNC` and then
//! streams the new bytes in — so a crash, a panic, or a power loss between
//! those two steps leaves the user with a truncated or empty note. There is no
//! second copy to fall back on.
//!
//! # Atomic write strategy
//!
//! [`Persist::save`] never writes into the live note file. It:
//!
//! 1. creates the parent directory if needed,
//! 2. writes the full text to a uniquely named temp file
//!    (`.note.md.tmp-<pid>-<counter>`) **in the same directory** as the target
//!    — same filesystem, which is what makes step 4 atomic,
//! 3. calls [`std::fs::File::sync_all`] so the bytes and the file metadata are
//!    on stable storage before anything else happens,
//! 4. closes the file and `rename(2)`s it over the target.
//!
//! `rename(2)` within a filesystem is atomic: any reader either sees the whole
//! old file or the whole new file, never a half-written one. If any step fails
//! the temp file is removed on a best-effort basis and the original error is
//! returned, leaving the previous note untouched.
//!
//! Saves are also deduplicated: [`Persist`] remembers the last text it wrote,
//! so per-keystroke autosave collapses to "write only when the buffer actually
//! changed" ([`Persist::is_dirty`]).
//!
//! # Backup policy
//!
//! Backups live in `<parent of note>/backups` and are named
//! `note-YYYY-MM-DD_HH-MM-SS.md` in **local** time — filename-safe on macOS and
//! lexicographically sortable, so the directory listing is already
//! chronological.
//!
//! [`Persist::maybe_backup`] writes one only when *both* hold:
//!
//! * at least [`BACKUP_INTERVAL_SECS`] have elapsed since the last backup (or
//!   there is no previous backup at all), and
//! * the text actually differs from the most recent backup's contents.
//!
//! That keeps the directory free of identical hourly copies of an untouched
//! note. After every successful backup the directory is thinned to the tiers
//! [`keep_backups`] describes: every backup for two days, then one a day for a
//! month, then one a month for two years.
//!
//! A backup never clobbers an existing file: the name is reserved with an
//! exclusive `create_new` open, and on collision the timestamp is bumped one
//! second at a time until a free name is found. [`Persist::new`] rescans the
//! backup directory so [`Persist::last_backup_time`] survives app restarts.
//!
//! This module is deliberately free of `gpui` (and of any global state) so it
//! can be unit-tested on its own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Local, NaiveDateTime, TimeZone};

/// How often a backup is taken.
pub const BACKUP_INTERVAL_SECS: i64 = 3600;

/// How long every backup is kept, whatever else the tiers say: two days of
/// hourly snapshots, for the mistake you notice this afternoon.
pub const KEEP_ALL_HOURS: i64 = 48;

/// Then one a day, for a month — the mistake you notice next week.
pub const KEEP_DAILY_DAYS: i64 = 30;

/// Then one a month, for this many days — about two years. Twenty years of
/// notes deserve a safety net deeper than two days; a monthly snapshot of a
/// nine-megabyte file is a rounding error against the note itself.
pub const KEEP_MONTHLY_DAYS: i64 = 730;

/// However many the tiers would keep, never more than this. The tiers thin out
/// with age, but the newest tier keeps *everything* — and a backup is written
/// whenever the note differs from the last one, which a sync service flipping a
/// file between two versions can drive as fast as the tick loop runs. The cap is
/// what stops that filling the disk; at one an hour it is four months of
/// headroom, so nothing an ordinary week produces ever reaches it.
pub const KEEP_MOST: usize = 3000;

/// Name of the backup subdirectory, relative to the note's parent directory.
const BACKUP_DIR_NAME: &str = "backups";

/// Prefix of a backup file name.
const BACKUP_PREFIX: &str = "note-";

/// Extension (including the dot) of a backup file name.
const BACKUP_SUFFIX: &str = ".md";

/// `chrono` format string for the timestamp embedded in a backup file name.
const BACKUP_STAMP_FMT: &str = "%Y-%m-%d_%H-%M-%S";

/// Upper bound on how far a backup timestamp may be bumped to dodge an
/// existing file, so a pathological directory can never spin forever.
const MAX_NAME_PROBES: u32 = 4096;

/// A cheap fingerprint of the note file on disk: its byte length and its
/// modification time. Comparing two of these tells whether the file changed
/// underneath us without reading its whole content — the check runs on the tick
/// loop, and re-reading nine megabytes to diff it every time would be the
/// opposite of cheap. Size catches every length-changing edit outright; mtime
/// catches an in-place edit that happens to keep the length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: SystemTime,
}

/// What the note said, small enough to keep.
///
/// The length and a 64-bit hash of the text, which is what "have I already
/// written this?" is asked against. Holding the text itself would double the
/// app's memory for the life of the process — on a twenty-year note, nine
/// megabytes of it — and the question does not need the bytes, only their
/// identity.
///
/// Two different notes that share a length *and* a SipHash are what would fool
/// it, at odds around one in eighteen quintillion per comparison; the outcome
/// there is one skipped save of content that is about to be saved again by the
/// next keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    len: usize,
    hash: u64,
}

impl Fingerprint {
    fn of(text: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        Fingerprint {
            len: text.len(),
            hash: hasher.finish(),
        }
    }
}

/// Fingerprint the file at `path`, or `None` when it cannot be stat'd (missing,
/// or a filesystem that reports no modification time).
pub fn stamp_of(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

/// Who last published a version of the note, and what the file looked like when
/// they did.
///
/// Writes come from two places — a background [`SaveJob`] and the blocking save
/// on the quit path — and the note file is the one thing they must not disagree
/// about. Every rename happens with this locked, so the ordering is decided
/// rather than raced: a job that finds a newer version already published has
/// been overtaken while it was writing, and drops its temp file instead of
/// putting stale text back.
///
/// The stamp is taken under the same lock, immediately after the rename, so
/// "the file as we last wrote it" is never a stat from some arbitrary moment
/// later — which is how a sync service's write could be mistaken for our own.
#[derive(Debug, Default)]
struct WriteGate {
    published: u64,
    stamp: Option<FileStamp>,
}

/// One pending write of the note, with everything it needs to run away from the
/// main thread. Made by [`Persist::begin_save`] and reported back through
/// [`Persist::finish_save`].
pub struct SaveJob {
    dir: PathBuf,
    tmp: PathBuf,
    target: PathBuf,
    text: String,
    fingerprint: Fingerprint,
    seq: u64,
    gate: Arc<Mutex<WriteGate>>,
    /// Write even over a file that changed underneath us. Only
    /// [`Persist::save_force`] asks for that.
    force: bool,
}

/// What a [`SaveJob`] did, for [`Persist::finish_save`].
pub struct SaveOutcome {
    pub result: io::Result<()>,
    /// False when a newer write landed first, or the file changed underneath
    /// us, and this one stood down.
    published: bool,
    fingerprint: Fingerprint,
    stamp: Option<FileStamp>,
}

impl SaveOutcome {
    /// Whether these bytes are what the file now holds.
    pub fn published(&self) -> bool {
        self.published
    }
}

impl SaveJob {
    /// Do the write. Blocks — call it off the main thread.
    pub fn run(self) -> SaveOutcome {
        let stale = || SaveOutcome {
            result: Ok(()),
            published: false,
            fingerprint: self.fingerprint,
            stamp: None,
        };
        if let Err(err) = fs::create_dir_all(&self.dir) {
            return SaveOutcome {
                result: Err(err),
                published: false,
                fingerprint: self.fingerprint,
                stamp: None,
            };
        }
        // The slow part — writing and fsyncing megabytes — happens outside the
        // lock; only the rename is serialised.
        if let Err(err) = write_to_temp(&self.tmp, &self.text) {
            return SaveOutcome {
                result: Err(err),
                published: false,
                fingerprint: self.fingerprint,
                stamp: None,
            };
        }
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.published > self.seq {
            // Someone published newer text while this was being written. Putting
            // ours over the top would silently undo theirs.
            let _ = fs::remove_file(&self.tmp);
            return stale();
        }
        // ...and the same for a writer that is not us. Nine megabytes take long
        // enough to write that a sync service can land a version in the middle
        // of it; renaming over that would destroy it *and* record our own stamp,
        // so nothing would ever notice it had been there. A file that is simply
        // *gone* is not that case — it is one to write again — and `save_force`
        // is the deliberate "write anyway".
        let changed_underneath = gate.stamp.is_some()
            && stamp_of(&self.target).is_some_and(|now| Some(now) != gate.stamp);
        if changed_underneath && !self.force {
            let _ = fs::remove_file(&self.tmp);
            return stale();
        }
        match fs::rename(&self.tmp, &self.target) {
            Ok(()) => {
                gate.published = self.seq;
                gate.stamp = stamp_of(&self.target);
                SaveOutcome {
                    result: Ok(()),
                    published: true,
                    fingerprint: self.fingerprint,
                    stamp: gate.stamp,
                }
            }
            Err(err) => {
                let _ = fs::remove_file(&self.tmp);
                SaveOutcome {
                    result: Err(err),
                    published: false,
                    fingerprint: self.fingerprint,
                    stamp: None,
                }
            }
        }
    }
}

/// Durable, deduplicating writer for the note file and its rolling backups.
///
/// See the [module docs](self) for the atomic-write strategy and the backup
/// policy.
pub struct Persist {
    /// The live note file.
    path: PathBuf,
    /// Fingerprint of the last text successfully written by this instance, for
    /// dirty tracking. A fingerprint rather than the text: keeping the string
    /// meant a second copy of the whole note resident for as long as the app
    /// ran — nine megabytes to answer a yes/no question.
    last_saved: Option<Fingerprint>,
    /// Who last published the note, and what the file looked like then. Shared
    /// with any [`SaveJob`] in flight, which is what makes the two writers agree
    /// on an order. A stat that no longer matches [`WriteGate::stamp`] means
    /// someone else rewrote the file — a sync service, another editor — which
    /// [`Self::disk_changed`] reports so the app can reload rather than silently
    /// clobber it.
    gate: Arc<Mutex<WriteGate>>,
    /// Sequence handed to the next write. Monotonic, so "newer" is decidable.
    next_seq: u64,
    /// Timestamp encoded in the newest backup file we know about.
    last_backup: Option<DateTime<Local>>,
    /// When the backup question was last asked and answered "unchanged".
    ///
    /// Without it an idle app re-reads the newest backup on every tick once the
    /// interval has elapsed, because nothing advances `last_backup`. Cleared by
    /// any write, so an edit reopens the question immediately rather than
    /// waiting out another interval.
    last_checked: Option<DateTime<Local>>,
    /// Bumped per temp file so concurrent saves cannot pick the same name.
    tmp_counter: u64,
}

impl Persist {
    /// Creates a persister for the note at `path`.
    ///
    /// Scans the backup directory once so [`Self::last_backup_time`] survives
    /// app restarts. This never fails: an unreadable or missing backup
    /// directory simply means "no previous backup".
    pub fn new(path: PathBuf) -> Self {
        let mut this = Self {
            path,
            last_saved: None,
            gate: Arc::new(Mutex::new(WriteGate::default())),
            next_seq: 1,
            last_backup: None,
            last_checked: None,
            tmp_counter: 0,
        };
        this.last_backup = this
            .backups()
            .first()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .and_then(parse_backup_file_name);
        this
    }

    /// The live note file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `<parent of note>/backups`.
    pub fn backup_dir(&self) -> PathBuf {
        parent_dir(&self.path).join(BACKUP_DIR_NAME)
    }

    /// True when `text` differs from the last content this instance
    /// successfully wrote. Always true before the first successful save.
    pub fn is_dirty(&self, text: &str) -> bool {
        self.last_saved != Some(Fingerprint::of(text))
    }

    /// Atomically writes `text` to the note path.
    ///
    /// A no-op returning `Ok(())` when `!self.is_dirty(text)`, which is what
    /// makes per-keystroke autosave cheap. This is the blocking version, for the
    /// paths with no next frame to finish on — quitting, hiding. Everything else
    /// goes through [`Self::begin_save`].
    pub fn save(&mut self, text: &str) -> io::Result<()> {
        self.save_reporting(text).result
    }

    /// The same write, with what it did — for the caller that has to tell a
    /// write from a stand-down.
    pub fn save_reporting(&mut self, text: &str) -> SaveOutcome {
        let Some(job) = self.begin_save(text) else {
            return SaveOutcome {
                result: Ok(()),
                published: true,
                fingerprint: Fingerprint::of(text),
                stamp: self.known_stamp(),
            };
        };
        let outcome = job.run();
        self.finish_save(&outcome);
        outcome
    }

    /// Like [`Self::save`], but writes even when the text is unchanged.
    pub fn save_force(&mut self, text: &str) -> io::Result<()> {
        let job = self.job_with(text, Fingerprint::of(text), true);
        let outcome = job.run();
        self.finish_save(&outcome);
        outcome.result
    }

    /// The work of saving `text`, ready to be handed to another thread, or
    /// `None` when the note on disk already holds it.
    ///
    /// Writing nine megabytes takes about ten milliseconds, and doing it on the
    /// main thread stalls a frame every time typing settles — worse on a network
    /// volume, where it is unbounded. The bytes are copied here (the buffer goes
    /// on being edited while the write is in flight), the write happens
    /// elsewhere, and [`Self::finish_save`] records the outcome when it lands.
    pub fn begin_save(&mut self, text: &str) -> Option<SaveJob> {
        // A note that has vanished from disk is not "already saved". Deleted by
        // hand, or its folder moved into a sync service: without this check the
        // app goes on showing a note that exists nowhere else, and quitting
        // writes nothing because the buffer matches what was last written.
        let fingerprint = Fingerprint::of(text);
        if self.last_saved == Some(fingerprint) && self.path.exists() {
            return None;
        }
        Some(self.job_for(text, fingerprint))
    }

    /// What the note file looked like when we last wrote or adopted it.
    fn known_stamp(&self) -> Option<FileStamp> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).stamp
    }

    /// Record the file's current state as ours, at a given sequence.
    fn publish(&mut self, stamp: Option<FileStamp>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.published = seq;
        gate.stamp = stamp;
    }

    fn job_for(&mut self, text: &str, fingerprint: Fingerprint) -> SaveJob {
        self.job_with(text, fingerprint, false)
    }

    fn job_with(&mut self, text: &str, fingerprint: Fingerprint, force: bool) -> SaveJob {
        let dir = parent_dir(&self.path);
        let base = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("note.md");
        self.tmp_counter = self.tmp_counter.wrapping_add(1);
        let tmp = dir.join(format!(
            ".{}.tmp-{}-{}",
            base,
            std::process::id(),
            self.tmp_counter
        ));
        let seq = self.next_seq;
        self.next_seq += 1;
        SaveJob {
            dir,
            tmp,
            target: self.path.clone(),
            fingerprint,
            text: text.to_string(),
            seq,
            gate: Arc::clone(&self.gate),
            force,
        }
    }

    /// Record what a finished [`SaveJob`] did. Must be called on the same side
    /// as the rest of `Persist`, with the outcome the job returned.
    pub fn finish_save(&mut self, outcome: &SaveOutcome) {
        // A failed write leaves the baseline alone, and so does one that stood
        // down for newer text: in both cases the file does not hold ours.
        if outcome.result.is_err() || !outcome.published {
            return;
        }
        self.last_saved = Some(outcome.fingerprint);
        // The stamp was taken under the gate, immediately after the rename —
        // not re-stat'd here, where a sync service's write in the meantime
        // would be recorded as ours and then never noticed.
        let _ = outcome.stamp;
        // The note has moved on, so whatever the last backup comparison
        // concluded no longer holds: ask again on the next tick.
        self.last_checked = None;
    }

    /// Forget that anything has been written, so the next save writes whatever
    /// the buffer holds. For text that came from somewhere other than the note
    /// file — a restored backup — where the app's copy is deliberately *not*
    /// what is on disk.
    pub fn mark_dirty(&mut self) {
        self.last_saved = None;
    }

    /// The note file's fingerprint if it differs from the one recorded at the
    /// last write or adoption — i.e. someone changed the file underneath us.
    ///
    /// `None` when the file matches, is missing, or has never been
    /// fingerprinted. A missing file is deliberately *not* reported as a change:
    /// a note deleted from disk is already rewritten by the next [`Self::save`],
    /// and treating it as an external edit would trigger a reload of nothing.
    pub fn disk_changed(&self) -> Option<FileStamp> {
        let last = self.known_stamp()?;
        let current = stamp_of(&self.path)?;
        (current != last).then_some(current)
    }

    /// Adopt the file's current fingerprint without touching its bytes. Called
    /// after the app has read the disk copy into its buffer, so a later stat
    /// compares against what was actually loaded.
    pub fn sync_stamp(&mut self) {
        let stamp = stamp_of(&self.path);
        self.publish(stamp);
    }

    /// Record `text` as the on-disk baseline: the app now holds exactly what the
    /// file contains (a fresh load, or a reload after an external change). Marks
    /// the buffer clean and re-fingerprints the file so neither a redundant save
    /// nor a false change is triggered next tick.
    pub fn adopt_disk(&mut self, text: &str) {
        self.last_saved = Some(Fingerprint::of(text));
        let stamp = stamp_of(&self.path);
        self.publish(stamp);
        self.last_checked = None;
    }

    /// Writes a timestamped backup when the interval has elapsed *and* the
    /// text differs from the most recent backup's contents.
    ///
    /// Returns `Ok(Some(path))` when a backup was written, `Ok(None)`
    /// otherwise. Prunes to the retention tiers after a successful write.
    pub fn maybe_backup(
        &mut self,
        text: &str,
        now: DateTime<Local>,
    ) -> io::Result<Option<PathBuf>> {
        if !self.interval_elapsed(now) {
            return Ok(None);
        }
        if !self.differs_from_newest_backup(text) {
            // Nothing to back up, and nothing has been written since, so the
            // answer holds: record it. Otherwise every tick from here on
            // re-reads the newest backup in full — on an idle app with an
            // eight-megabyte note, forty megabytes a second of pointless
            // reading, forever. A write clears this.
            self.last_checked = Some(now);
            return Ok(None);
        }
        self.backup_now(text, now)
    }

    /// Writes a timestamped backup for the menu's "Back Up Now", *unless* the
    /// newest existing backup already holds this exact text.
    ///
    /// Returns `Ok(Some(path))` when a backup was written and `Ok(None)` when it
    /// was skipped as a duplicate. Mashing ⌘S must not fill the 48-slot ring
    /// with near-identical same-minute snapshots and evict real hourly history:
    /// a press that changes nothing is a no-op, so the caller can say so rather
    /// than quietly writing a copy the user already has. Prunes to
    /// the tiers after a write.
    pub fn backup_now(&mut self, text: &str, now: DateTime<Local>) -> io::Result<Option<PathBuf>> {
        if !self.differs_from_newest_backup(text) {
            return Ok(None);
        }

        let dir = self.backup_dir();
        fs::create_dir_all(&dir)?;

        // Reserve a free name with an exclusive create, so an existing backup
        // can never be clobbered.
        let (target, stamp) = reserve_backup_name(&dir, now)?;

        self.tmp_counter = self.tmp_counter.wrapping_add(1);
        let tmp = dir.join(format!(
            ".backup.tmp-{}-{}",
            std::process::id(),
            self.tmp_counter
        ));
        if let Err(err) = write_atomically(&tmp, &target, text) {
            // Do not leave the reserved (empty) placeholder behind.
            let _ = fs::remove_file(&target);
            return Err(err);
        }

        self.last_backup = Some(stamp);
        self.prune_backups(now)?;
        Ok(Some(target))
    }

    /// Existing backup files, newest first.
    ///
    /// Ignores subdirectories and any file whose name is not a backup name.
    pub fn backups(&self) -> Vec<PathBuf> {
        let dir = self.backup_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        let mut found: Vec<(DateTime<Local>, String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stamp) = parse_backup_file_name(name) else {
                continue;
            };
            found.push((stamp, name.to_string(), entry.path()));
        }

        // Newest first, tie-broken by name so the order is fully deterministic.
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        found.into_iter().map(|(_, _, path)| path).collect()
    }

    /// Timestamp of the newest backup this instance knows about.
    pub fn last_backup_time(&self) -> Option<DateTime<Local>> {
        self.last_backup
    }

    /// The backups, newest first, with enough about each one to choose between
    /// them: when it was taken, how big it is, and its opening line.
    ///
    /// Only the head of each file is read — a backup is a whole note, and
    /// listing forty of them must not mean reading four hundred megabytes.
    pub fn backup_list(&self) -> Vec<BackupInfo> {
        self.backups()
            .into_iter()
            .filter_map(|path| {
                let at = path.file_name()?.to_str().and_then(parse_backup_file_name)?;
                let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                Some(BackupInfo {
                    at,
                    bytes,
                    preview: preview_of(&path),
                    path,
                })
            })
            .collect()
    }

    /// Thin the backup directory down to the tiers described by
    /// [`keep_backups`], returning how many files were deleted.
    ///
    /// Only ever touches files inside [`Self::backup_dir`] whose names parse as
    /// backup names. A missing directory is not an error (returns `Ok(0)`).
    pub fn prune_backups(&self, now: DateTime<Local>) -> io::Result<usize> {
        let dir = self.backup_dir();
        if !dir.is_dir() {
            return Ok(0);
        }
        // One listing, used for both the decision and the deletion. Reading the
        // directory twice and zipping the results means a backup written by
        // another copy of the app in between shifts every decision onto the
        // wrong file.
        let listed = self.backups();
        let stamps: Vec<DateTime<Local>> = listed
            .iter()
            .filter_map(|p| p.file_name()?.to_str().and_then(parse_backup_file_name))
            .collect();
        let doomed: Vec<PathBuf> = listed
            .into_iter()
            .zip(keep_backups(&stamps, now))
            .filter_map(|(path, keep)| (!keep).then_some(path))
            .collect();

        let mut deleted = 0;
        for path in doomed {
            // Belt and braces: `backups()` only ever yields parsed names inside
            // `backup_dir()`, but re-check before unlinking anything.
            if path.parent() != Some(dir.as_path()) {
                continue;
            }
            let is_backup = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_backup_file_name)
                .is_some();
            if !is_backup {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => deleted += 1,
                // Someone else already removed it; that is the outcome we want.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(deleted)
    }

    /// Whether it is worth asking again. Answered from the last backup *or* the
    /// last time the question was asked and the answer was "no change" — both
    /// mean the note has been accounted for up to that moment.
    fn interval_elapsed(&self, now: DateTime<Local>) -> bool {
        let since = self.last_backup.max(self.last_checked);
        match since {
            None => true,
            Some(last) => now.signed_duration_since(last).num_seconds() >= BACKUP_INTERVAL_SECS,
        }
    }

    /// True when `text` differs from the newest backup on disk. An absent or
    /// unreadable backup counts as "differs", so we err towards keeping data.
    fn differs_from_newest_backup(&self, text: &str) -> bool {
        match self.backups().first() {
            None => true,
            Some(newest) => match fs::read_to_string(newest) {
                Ok(previous) => previous != text,
                Err(_) => true,
            },
        }
    }
}

/// One backup, as the "Restore from Backup" list shows it.
#[derive(Clone, Debug)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub at: DateTime<Local>,
    pub bytes: u64,
    /// The first line with anything on it, trimmed — enough to recognise which
    /// afternoon this was.
    pub preview: String,
}

/// How much of a backup is read to show what is in it.
const PREVIEW_BYTES: u64 = 4096;

/// The first non-empty line of `path`, read from its head only.
fn preview_of(path: &Path) -> String {
    use std::io::Read;
    // Bytes, then lossy: reading a fixed number of bytes into a `String` fails
    // outright when the cut lands inside a character, and a note with an emoji
    // near the cut would have shown as empty.
    let mut head = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(PREVIEW_BYTES).read_to_end(&mut head);
    }
    let head = String::from_utf8_lossy(&head);
    head.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(empty)")
        .chars()
        .take(80)
        .collect()
}

/// Which of `stamps` (newest first) the backup directory keeps, as of `now`.
///
/// Three tiers, coarser as they go back: every backup from the last
/// [`KEEP_ALL_HOURS`] hours, then the newest of each calendar day for
/// [`KEEP_DAILY_DAYS`] days, then the newest of each calendar month for
/// [`KEEP_MONTHLY_MONTHS`] months. Older than that is dropped.
///
/// A flat ring of 48 hourly files — which is what this replaces — is two days
/// of history for a file that is supposed to hold twenty years. Losing a note
/// and noticing on Friday should not be unrecoverable, and the tiers cost a few
/// dozen files rather than a few thousand.
fn keep_backups(stamps: &[DateTime<Local>], now: DateTime<Local>) -> Vec<bool> {
    let hours = |from: DateTime<Local>| now.signed_duration_since(from).num_hours();
    let days = |from: DateTime<Local>| now.signed_duration_since(from).num_days();
    let mut kept_day: Option<(i32, u32)> = None;
    let mut kept_month: Option<(i32, u32)> = None;
    let mut kept = 0usize;
    stamps
        .iter()
        .map(|&stamp| {
            if kept >= KEEP_MOST {
                return false;
            }
            use chrono::Datelike;
            let day = (stamp.year(), stamp.ordinal());
            let month = (stamp.year(), stamp.month());
            if hours(stamp) < KEEP_ALL_HOURS {
                // Inside the hourly window every backup stays, but it still
                // claims its day and month so the tiers below do not keep a
                // second copy of the same day.
                kept_day = Some(day);
                kept_month = Some(month);
                kept += 1;
                return true;
            }
            if days(stamp) < KEEP_DAILY_DAYS {
                if kept_day == Some(day) {
                    return false;
                }
                kept_day = Some(day);
                kept_month = Some(month);
                kept += 1;
                return true;
            }
            if days(stamp) < KEEP_MONTHLY_DAYS {
                if kept_month == Some(month) {
                    return false;
                }
                kept_month = Some(month);
                kept += 1;
                return true;
            }
            false
        })
        .collect()
}

/// `"note-2026-08-05_14-30-05.md"` — local time, lexicographically sortable,
/// filename-safe on macOS.
pub fn backup_file_name(now: DateTime<Local>) -> String {
    format!(
        "{}{}{}",
        BACKUP_PREFIX,
        now.format(BACKUP_STAMP_FMT),
        BACKUP_SUFFIX
    )
}

/// Inverse of [`backup_file_name`]; `None` when the name does not match.
pub fn parse_backup_file_name(name: &str) -> Option<DateTime<Local>> {
    let stamp = name
        .strip_prefix(BACKUP_PREFIX)?
        .strip_suffix(BACKUP_SUFFIX)?;
    let naive = NaiveDateTime::parse_from_str(stamp, BACKUP_STAMP_FMT).ok()?;
    // A local time can be ambiguous across a DST fall-back; take the earlier of
    // the two candidates so parsing stays deterministic.
    Local.from_local_datetime(&naive).earliest()
}

/// The directory containing `path`, falling back to the current directory for
/// bare file names.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Writes `text` to `tmp`, fsyncs it, then renames it over `target`.
///
/// `tmp` and `target` must live in the same directory (hence the same
/// filesystem) for the rename to be atomic. On any failure the temp file is
/// removed on a best-effort basis and the original error is returned.
fn write_atomically(tmp: &Path, target: &Path, text: &str) -> io::Result<()> {
    let result = write_to_temp(tmp, text).and_then(|()| fs::rename(tmp, target));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// The first half of an atomic write: the bytes, on disk, in a file nobody is
/// reading yet. Split out because a [`SaveJob`] does this part off the main
/// thread and unlocked, and only takes the write gate for the rename.
fn write_to_temp(tmp: &Path, text: &str) -> io::Result<()> {
    let mut file = File::create(tmp)?;
    file.write_all(text.as_bytes())?;
    // Durability before visibility: the bytes must be on disk before the
    // rename publishes them.
    file.sync_all()
}

/// Atomically replace `target` with `bytes`, via a sibling temp file. The same
/// temp-file + fsync + rename the note uses, exposed for the small settings
/// file so a crash or full disk mid-write can never truncate it to nothing.
pub fn write_atomically_bytes(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = target.with_extension("writing.tmp");
    let result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Exclusively creates the first free backup file name at or after `now`,
/// returning the reserved path and the timestamp it encodes.
fn reserve_backup_name(dir: &Path, now: DateTime<Local>) -> io::Result<(PathBuf, DateTime<Local>)> {
    let one_second = Duration::try_seconds(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid backup interval"))?;

    let mut stamp = now;
    for _ in 0..MAX_NAME_PROBES {
        let candidate = dir.join(backup_file_name(stamp));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok((candidate, stamp)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                stamp = stamp.checked_add_signed(one_second).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "backup timestamp overflow")
                })?;
            }
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free backup file name available",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use std::env;

    /// A fresh, uniquely named scratch directory for one test.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(test_name: &str) -> Self {
            let dir = env::temp_dir().join(format!(
                "gravitynote-persist-{}-{}",
                std::process::id(),
                test_name
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        /// Path of the note inside a nested directory that does not exist yet,
        /// exercising the `create_dir_all` path.
        fn note(&self) -> PathBuf {
            self.dir.join("Application Support").join("note.md")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read file")
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn seconds(n: i64) -> Duration {
        Duration::try_seconds(n).expect("valid duration")
    }

    #[test]
    fn save_writes_exact_bytes_including_utf8_and_trailing_newline() {
        let scratch = Scratch::new("save-round-trip");
        let mut persist = Persist::new(scratch.note());

        let text = "héllo — 日本語 🌍\nsecond line\tTabbed\n";
        persist.save(text).expect("save");

        assert_eq!(read(persist.path()), text);
        assert_eq!(
            fs::read(persist.path()).expect("read bytes"),
            text.as_bytes(),
            "bytes must round trip verbatim"
        );
    }

    #[test]
    fn save_is_a_noop_when_not_dirty_but_save_force_writes() {
        let scratch = Scratch::new("dirty-tracking");
        let mut persist = Persist::new(scratch.note());

        assert!(persist.is_dirty("hello"), "dirty before the first save");
        persist.save("hello").expect("first save");
        assert!(
            !persist.is_dirty("hello"),
            "clean after saving the same text"
        );
        assert!(persist.is_dirty("hello!"), "dirty for different text");

        // Tamper with the file behind the persister's back.
        fs::write(persist.path(), "TAMPERED").expect("tamper");

        persist.save("hello").expect("no-op save");
        assert_eq!(
            read(persist.path()),
            "TAMPERED",
            "save must not touch the file when the text is unchanged"
        );

        persist.save_force("hello").expect("forced save");
        assert_eq!(
            read(persist.path()),
            "hello",
            "save_force must write even when clean"
        );
    }

    #[test]
    fn successful_save_leaves_no_temp_files_behind() {
        let scratch = Scratch::new("no-temp-files");
        let mut persist = Persist::new(scratch.note());

        for i in 0..5 {
            persist.save(&format!("revision {i}")).expect("save");
        }

        let dir = parent_dir(persist.path());
        let names = file_names(&dir);
        assert_eq!(
            names,
            vec!["note.md".to_string()],
            "found stray files: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".tmp-")),
            "temp files left behind: {names:?}"
        );
    }

    #[test]
    fn backup_file_name_round_trips_to_second_precision() {
        let now = Local::now();
        let name = backup_file_name(now);

        assert!(name.starts_with("note-"), "unexpected name: {name}");
        assert!(name.ends_with(".md"), "unexpected name: {name}");
        assert_eq!(name.len(), "note-2026-08-05_14-30-05.md".len());

        let parsed = parse_backup_file_name(&name).expect("parse round trip");
        assert_eq!(
            parsed.timestamp(),
            now.with_nanosecond(0)
                .map(|t| t.timestamp())
                .expect("truncate")
        );
        assert_eq!(backup_file_name(parsed), name, "re-formatting is stable");
    }

    #[test]
    fn parse_backup_file_name_rejects_non_backup_names() {
        for name in [
            "note.md",
            "note-.md",
            "notes-2026-08-05_14-30-05.md",
            "note-2026-08-05_14-30-05.txt",
            "note-2026-13-05_14-30-05.md",
            "note-2026-08-05 14:30:05.md",
            "backups",
            "",
        ] {
            assert!(
                parse_backup_file_name(name).is_none(),
                "should not parse: {name}"
            );
        }
    }

    #[test]
    fn a_note_deleted_from_disk_is_written_again() {
        let scratch = Scratch::new("deleted-note");
        let mut persist = Persist::new(scratch.note());

        persist.save("the note").unwrap();
        assert_eq!(read(&scratch.note()), "the note");

        // Deleted underneath the running app, with no further edits.
        fs::remove_file(scratch.note()).unwrap();
        persist.save("the note").unwrap();
        assert_eq!(
            read(&scratch.note()),
            "the note",
            "an unchanged buffer must still restore a note that vanished"
        );
    }

    #[test]
    fn an_unchanged_note_is_only_re_read_once_per_interval() {
        let scratch = Scratch::new("idle-backup");
        let mut persist = Persist::new(scratch.note());
        let t0 = Local::now();

        assert!(persist.maybe_backup("note", t0).unwrap().is_some());

        // An hour later the interval has elapsed but nothing changed. The first
        // ask reads the newest backup; the ones after it must not — otherwise
        // an idle app re-reads the whole file five times a second, forever.
        let hour = t0 + chrono::Duration::seconds(BACKUP_INTERVAL_SECS);
        assert!(persist.maybe_backup("note", hour).unwrap().is_none());
        let checked = persist.last_checked.expect("the ask was recorded");

        for tick in 1..=5 {
            let later = hour + chrono::Duration::milliseconds(200 * tick);
            assert!(persist.maybe_backup("note", later).unwrap().is_none());
            assert_eq!(
                persist.last_checked,
                Some(checked),
                "tick {tick} asked again inside the interval"
            );
        }

        // A write reopens the question straight away: an edit must not have to
        // wait out another interval for its snapshot.
        persist.save_force("changed").unwrap();
        assert_eq!(persist.last_checked, None);
        let soon = hour + chrono::Duration::seconds(1);
        assert!(persist.maybe_backup("changed", soon).unwrap().is_some());
    }

    #[test]
    fn maybe_backup_respects_interval_and_content_changes() {
        let scratch = Scratch::new("maybe-backup");
        let mut persist = Persist::new(scratch.note());
        let t0 = Local::now();

        // First call always backs up.
        let first = persist
            .maybe_backup("version one", t0)
            .expect("first backup")
            .expect("first backup should be written");
        assert_eq!(read(&first), "version one");
        assert!(persist.last_backup_time().is_some());

        // Immediately after: interval has not elapsed.
        assert!(
            persist
                .maybe_backup("version two", t0)
                .expect("second call")
                .is_none(),
            "must not back up before the interval elapses"
        );

        // Interval elapsed but content unchanged.
        let t1 = t0 + seconds(BACKUP_INTERVAL_SECS + 5);
        assert!(
            persist
                .maybe_backup("version one", t1)
                .expect("unchanged call")
                .is_none(),
            "must not back up identical content"
        );

        // Interval elapsed and content changed. The write is what tells the
        // backup the note has moved on — an unchanged answer holds until one
        // happens, which is what keeps an idle app off the disk.
        persist.save_force("version two").unwrap();
        let second = persist
            .maybe_backup("version two", t1)
            .expect("changed call")
            .expect("changed content should be backed up");
        assert_eq!(read(&second), "version two");
        assert_ne!(first, second);
        assert_eq!(persist.backups().len(), 2);
        assert_eq!(
            persist.backups().first(),
            Some(&second),
            "backups() is newest first"
        );
    }

    #[test]
    fn backup_now_writes_distinct_content_and_handles_same_second_collisions() {
        let scratch = Scratch::new("backup-now-collision");
        let mut persist = Persist::new(scratch.note());
        let now = Local::now();

        // Three *different* snapshots taken in the same second must each land in
        // their own file — the timestamp is bumped to dodge the name collision.
        let a = persist.backup_now("alpha", now).expect("first").expect("written");
        let b = persist.backup_now("beta", now).expect("second").expect("written");
        let c = persist.backup_now("gamma", now).expect("third").expect("written");

        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(read(&a), "alpha");
        assert_eq!(read(&b), "beta");
        assert_eq!(read(&c), "gamma");
        assert_eq!(
            persist.backups().len(),
            3,
            "same-second backups must not clobber each other"
        );

        let dir = persist.backup_dir();
        let names = file_names(&dir);
        assert!(
            !names.iter().any(|n| n.contains(".tmp-")),
            "temp files left behind: {names:?}"
        );
    }

    #[test]
    fn backup_now_skips_a_duplicate_of_the_newest_backup() {
        let scratch = Scratch::new("backup-now-dedup");
        let mut persist = Persist::new(scratch.note());
        let now = Local::now();

        // First press writes the snapshot.
        assert!(
            persist.backup_now("same text", now).expect("first").is_some(),
            "the first backup of new content is written"
        );
        assert_eq!(persist.backups().len(), 1);

        // Mashing ⌘S on unchanged content must not add a second near-duplicate
        // and start evicting real history.
        assert!(
            persist
                .backup_now("same text", now)
                .expect("second")
                .is_none(),
            "an identical backup is skipped, not written again"
        );
        assert_eq!(
            persist.backups().len(),
            1,
            "no duplicate file should have been created"
        );

        // A genuine change is still backed up.
        assert!(
            persist
                .backup_now("different text", now)
                .expect("third")
                .is_some(),
            "changed content is backed up again"
        );
        assert_eq!(persist.backups().len(), 2);
    }

    #[test]
    fn disk_changed_notices_external_writes_only() {
        let scratch = Scratch::new("disk-changed");
        let mut persist = Persist::new(scratch.note());

        // Nothing stamped yet: without a baseline there is no change to report.
        assert!(persist.disk_changed().is_none());

        persist.save("original").expect("save");
        // Our own write is the baseline, not an external change.
        assert!(persist.disk_changed().is_none());

        // Another process rewrites the file (a different length guarantees the
        // size half of the stamp catches it, regardless of mtime resolution).
        fs::write(persist.path(), "rewritten by something else").expect("external write");
        let seen = persist.disk_changed().expect("external write is noticed");
        assert_eq!(seen, stamp_of(persist.path()).unwrap());

        // Adopting the current copy makes the check go quiet again.
        persist.sync_stamp();
        assert!(persist.disk_changed().is_none());

        // adopt_disk seeds the baseline for a freshly loaded note: the stamp
        // matches and the buffer is considered clean.
        fs::write(persist.path(), "loaded from disk").expect("seed");
        persist.adopt_disk("loaded from disk");
        assert!(persist.disk_changed().is_none());
        assert!(!persist.is_dirty("loaded from disk"));

        // A vanished file is not reported as an external edit: the next save
        // rewrites it.
        fs::remove_file(persist.path()).expect("delete");
        assert!(persist.disk_changed().is_none());
    }

    #[test]
    fn prune_backups_keeps_newest_and_ignores_foreign_files() {
        let scratch = Scratch::new("prune");
        let mut persist = Persist::new(scratch.note());
        let t0 = Local::now();

        let mut written = Vec::new();
        for i in 0..6 {
            let path = persist
                .backup_now(&format!("v{i}"), t0 + seconds(i * 60))
                .expect("backup")
                .expect("distinct content is written");
            written.push(path);
        }

        let dir = persist.backup_dir();
        let foreign = dir.join("README.txt");
        fs::write(&foreign, "not a backup").expect("write foreign file");
        let foreign_dir = dir.join("note-2026-01-01_00-00-00.md.d");
        fs::create_dir_all(&foreign_dir).expect("create foreign dir");

        assert_eq!(persist.backups().len(), 6, "foreign entries are ignored");

        // Six backups a minute apart: all inside the hourly window, all kept.
        assert_eq!(persist.prune_backups(t0 + seconds(360)).expect("prune"), 0);
        assert_eq!(persist.backups().len(), 6);

        // A week later, only the newest of that day survives the daily tier.
        let deleted = persist
            .prune_backups(t0 + seconds(7 * 24 * 3600))
            .expect("prune");
        assert_eq!(deleted, 5);

        let remaining = persist.backups();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0], written[5], "the day keeps its newest backup");
        for old in &written[..5] {
            assert!(!old.exists(), "old backup should be gone: {old:?}");
        }

        assert!(foreign.exists(), "non-backup file must be left untouched");
        assert!(foreign_dir.is_dir(), "subdirectory must be left untouched");

        // Pruning again is idempotent.
        assert_eq!(
            persist
                .prune_backups(t0 + seconds(7 * 24 * 3600))
                .expect("re-prune"),
            0
        );
    }

    /// The tiers: everything for two days, a day's newest for a month, a
    /// month's newest for two years, nothing beyond. A flat 48-file ring gave a
    /// twenty-year note a two-day safety net.
    #[test]
    fn retention_thins_out_with_age() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 16, 12, 0, 0)
            .single()
            .expect("a real instant");
        let at = |y, m, d, h| {
            Local
                .with_ymd_and_hms(y, m, d, h, 0, 0)
                .single()
                .expect("a real instant")
        };
        // Newest first, the order `backups()` returns.
        let stamps = vec![
            at(2026, 8, 16, 11), // an hour ago
            at(2026, 8, 16, 9),  // this morning
            at(2026, 8, 15, 20), // yesterday evening, still inside 48 h
            at(2026, 8, 12, 18), // four days ago: daily tier
            at(2026, 8, 12, 9),  // same day, older
            at(2026, 8, 11, 9),  // another day
            at(2026, 5, 30, 9),  // months back: monthly tier
            at(2026, 5, 2, 9),   // same month, older
            at(2026, 4, 2, 9),   // another month
            at(2020, 1, 2, 9),   // six years back: gone
        ];
        assert_eq!(
            keep_backups(&stamps, now),
            vec![true, true, true, true, false, true, true, false, true, false],
            "kept: three recent, two of the four-day-old days, two months"
        );
    }

    /// A background write that is overtaken must stand down rather than put its
    /// snapshot back over newer text. This is the quit path: the tick starts a
    /// write, the user types one more character and quits, and the blocking save
    /// on the way out has to be the version that survives.
    #[test]
    fn an_overtaken_write_does_not_publish_stale_text() {
        let scratch = Scratch::new("overtaken");
        let mut persist = Persist::new(scratch.note());
        persist.save("first").expect("seed");

        // The tick's write of "second" is prepared...
        let slow = persist.begin_save("second").expect("dirty");
        // ...then the quit path writes "third" and lands first.
        persist.save("third").expect("quit save");
        assert_eq!(fs::read_to_string(persist.path()).unwrap(), "third");

        // Now the overtaken job finishes.
        let outcome = slow.run();
        assert!(outcome.result.is_ok(), "standing down is not a failure");
        persist.finish_save(&outcome);

        assert_eq!(
            fs::read_to_string(persist.path()).unwrap(),
            "third",
            "the older write must not reappear over the newer one"
        );
        assert!(
            persist.is_dirty("second"),
            "the baseline must not claim the abandoned text was written"
        );
        assert!(
            !persist.is_dirty("third"),
            "the write that did land is the baseline"
        );
        // And no temp file was left behind.
        let leftovers: Vec<_> = fs::read_dir(persist.path().parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "abandoned temp file left behind");
    }

    /// A write must not rename over a version somebody else landed while it was
    /// being written. Nine megabytes take long enough that a sync service can
    /// get in between, and renaming over it would destroy that version *and*
    /// record our own stamp, so nothing would ever notice it had been there.
    #[test]
    fn a_write_stands_down_when_the_file_changed_underneath_it() {
        let scratch = Scratch::new("changed-underneath");
        let mut persist = Persist::new(scratch.note());
        persist.save("ours, version one").expect("seed");

        let job = persist.begin_save("ours, version two").expect("dirty");
        // Somebody else writes while the job is in flight.
        fs::write(persist.path(), "theirs").expect("external write");
        let outcome = job.run();
        persist.finish_save(&outcome);

        assert_eq!(
            fs::read_to_string(persist.path()).unwrap(),
            "theirs",
            "the external version must survive"
        );
        assert!(!outcome.published(), "and the caller must be told we stood down");
        assert!(persist.disk_changed().is_some(), "so it can still be reconciled");

        // `save_force` is the deliberate override, and still writes.
        persist.save_force("ours, forced").expect("force");
        assert_eq!(fs::read_to_string(persist.path()).unwrap(), "ours, forced");
    }

    /// A write that lands normally is the baseline, and the stamp recorded for
    /// it is the file it just wrote — not a stat taken some time later, which is
    /// how someone else's write gets mistaken for ours and then never noticed.
    #[test]
    fn a_finished_write_stamps_the_file_it_wrote() {
        let scratch = Scratch::new("stamp-at-rename");
        let mut persist = Persist::new(scratch.note());
        let job = persist.begin_save("ours").expect("dirty");
        let outcome = job.run();

        // Someone else rewrites the file before the outcome is recorded.
        fs::write(persist.path(), "theirs").expect("external write");
        persist.finish_save(&outcome);

        assert!(
            persist.disk_changed().is_some(),
            "an external write between the rename and the bookkeeping must \
             still be reported"
        );
    }

    /// Restoring a backup puts text in the buffer that the note file does not
    /// hold; the next save has to write it.
    #[test]
    fn mark_dirty_makes_the_next_save_write() {
        let scratch = Scratch::new("mark-dirty");
        let mut persist = Persist::new(scratch.note());
        persist.save("on disk").expect("seed");
        persist.adopt_disk("restored");
        assert!(
            persist.begin_save("restored").is_none(),
            "adopting says the file already holds it"
        );

        persist.mark_dirty();
        let job = persist.begin_save("restored").expect("a write is owed");
        let outcome = job.run();
        persist.finish_save(&outcome);
        assert_eq!(fs::read_to_string(persist.path()).unwrap(), "restored");
    }

    /// Whatever the tiers say, the directory has a ceiling: a sync service
    /// flipping the note between two versions writes a backup every tick, and
    /// all of them are inside the 48-hour window that keeps everything.
    #[test]
    fn retention_has_a_hard_ceiling() {
        let now = Local::now();
        let stamps: Vec<DateTime<Local>> = (0..KEEP_MOST + 500)
            .map(|i| now - Duration::try_seconds(i as i64).expect("a real span"))
            .collect();
        let kept = keep_backups(&stamps, now);
        assert_eq!(
            kept.iter().filter(|k| **k).count(),
            KEEP_MOST,
            "the newest tier keeps everything, so something else has to stop"
        );
        assert!(kept[0] && kept[KEEP_MOST - 1], "the newest are the ones kept");
        assert!(!kept[KEEP_MOST], "and the rest go");
    }

    #[test]
    fn prune_backups_on_missing_directory_is_not_an_error() {
        let scratch = Scratch::new("prune-missing");
        let persist = Persist::new(scratch.note());
        assert!(!persist.backup_dir().exists());
        assert_eq!(persist.prune_backups(Local::now()).expect("prune"), 0);
        assert!(persist.backups().is_empty());
        assert!(persist.last_backup_time().is_none());
    }

    #[test]
    fn new_picks_up_the_newest_existing_backup() {
        let scratch = Scratch::new("restart-scan");
        let note = scratch.note();

        let stamps = {
            let mut persist = Persist::new(note.clone());
            let t0 = Local::now();
            let mut stamps = Vec::new();
            for i in 0..3 {
                let path = persist
                    .backup_now(&format!("v{i}"), t0 + seconds(i * 3600))
                    .expect("backup")
                    .expect("distinct content is written");
                let name = path.file_name().and_then(|n| n.to_str()).expect("name");
                stamps.push(parse_backup_file_name(name).expect("parse"));
            }
            // Decoys the scan must ignore.
            let dir = persist.backup_dir();
            fs::write(dir.join("scratch.md"), "ignored").expect("decoy file");
            fs::create_dir_all(dir.join("note-2099-01-01_00-00-00.md")).expect("decoy dir");
            stamps
        };

        // Simulate an app restart.
        let restarted = Persist::new(note);
        assert_eq!(
            restarted.last_backup_time(),
            Some(stamps[2]),
            "newest backup timestamp must survive a restart"
        );
        assert_eq!(restarted.backups().len(), 3);

        // A fresh instance is dirty until it writes something itself.
        assert!(restarted.is_dirty("anything"));
    }

    #[test]
    fn maybe_backup_after_restart_waits_for_the_full_interval() {
        let scratch = Scratch::new("restart-interval");
        let note = scratch.note();
        let t0 = Local::now();

        {
            let mut persist = Persist::new(note.clone());
            persist.backup_now("original", t0).expect("seed backup");
        }

        let mut restarted = Persist::new(note);
        assert!(
            restarted
                .maybe_backup("changed", t0 + seconds(60))
                .expect("early call")
                .is_none(),
            "restart must not reset the backup clock"
        );
        assert!(
            restarted
                .maybe_backup("changed", t0 + seconds(BACKUP_INTERVAL_SECS))
                .expect("late call")
                .is_some(),
            "backup is due once the full interval has elapsed"
        );
    }

    #[test]
    fn backup_dir_is_a_sibling_of_the_note() {
        let persist = Persist::new(PathBuf::from("/tmp/gravitynote/note.md"));
        assert_eq!(
            persist.backup_dir(),
            PathBuf::from("/tmp/gravitynote/backups")
        );
        assert_eq!(persist.path(), Path::new("/tmp/gravitynote/note.md"));

        // A bare file name resolves against the current directory.
        let bare = Persist::new(PathBuf::from("note.md"));
        assert_eq!(bare.backup_dir(), PathBuf::from("./backups"));
    }
}
