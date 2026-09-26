//! The image store, and how a note refers to what is in it.
//!
//! An image is a file in `<app support>/images/`, named by a hash of its own
//! bytes. The note holds a markdown reference to that name and nothing else —
//! never pixels, never a path outside the store.
//!
//! Content addressing buys three things at once. Pasting the same screenshot
//! twice writes one file. A file is never rewritten, so a backup from any past
//! moment still resolves against the store as it is today. And nothing has to
//! decide what an image is *called*, which is the question every "pasted image
//! 3.png" scheme eventually gets wrong.
//!
//! # The reference
//!
//! ```text
//! ![|420x263](images/9f3a1c2b7e40d115.png)
//! ```
//!
//! Ordinary CommonMark: any other markdown tool reads it as an image with the
//! alt text `|420x263`. The `|WxH` is Obsidian's spelling of a display size,
//! which is the dialect this app should speak, and it is the *display box* —
//! what layout needs — not the intrinsic pixel size. The file name names the
//! bytes and nothing else, so a copy that picks up a `(1)` suffix, or a rename,
//! costs a file rather than silently describing the wrong geometry.
//!
//! A reference with no size is legal (someone typed it) and gets its box from
//! the image once it has been decoded.
//!
//! # Deletion
//!
//! Never automatic. Removing an image from the note removes the reference, not
//! the file: undo would otherwise resurrect a broken link, and every rolling
//! backup that mentions the image would decay into a dead one. The store only
//! grows, and reclaiming it is an explicit act.

use std::io;
use std::path::{Path, PathBuf};

/// Where the store sits, relative to whatever directory holds the note.
pub const DIR_NAME: &str = "images";

/// The prefix a reference's URL carries, so a reference into the store is
/// distinguishable from a link to anywhere else at a glance.
pub const URL_PREFIX: &str = "images/";

/// Formats stored as they arrive. Everything else that macOS can read — HEIC
/// above all, which is what an iPhone photo is — is transcoded to PNG on the
/// way in, because the decoder cannot open it later.
/// AVIF is deliberately absent: `image` 0.25 recognises its magic bytes without
/// the `avif-native` decoder this build does not enable, so treating it as
/// native stored the file and then failed to read it back. Left in [`ACCEPTED`],
/// it takes the transcode path and works.
const NATIVE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "qoi",
];

/// Formats accepted from a drop. A superset of [`NATIVE`]: these are the ones
/// worth trying, and the import either stores or transcodes them.
const ACCEPTED: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "avif", "qoi", "heic",
    "heif", "pdf", "psd", "tga",
];

/// A newly stored image: its name in the store, and the pixel size it turned
/// out to have. The size is used once, to choose the box the note will record;
/// after that the note is the only thing that says how big to draw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub name: String,
    pub w: u32,
    pub h: u32,
}

/// An image reference parsed out of a line: which file, and the box it asked
/// for. `None` for the box means the line never said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub box_: Option<(u32, u32)>,
    /// The alt text exactly as written, `|WxH` and all, so a rewrite can put
    /// back what was there rather than reconstructing an approximation of it.
    pub alt: String,
}

/// `<dir>/images`
pub fn store_dir(dir: &Path) -> PathBuf {
    dir.join(DIR_NAME)
}

/// The absolute path of a stored image. Absolute on purpose: a bundle launched
/// by `open` has `/` for a working directory, so the relative form in the note
/// text would resolve to nothing.
pub fn path(dir: &Path, name: &str) -> PathBuf {
    store_dir(dir).join(name)
}

/// Every name `text` refers to, whether or not the reference is alone on its
/// line: an image with a caption typed after it is still an image the note uses.
///
/// Accumulated across the note *and* every backup by the caller, one file at a
/// time — holding a hundred whole notes in memory to answer this, and rescanning
/// all of them per candidate file, was minutes of a frozen window on a note of
/// any size.
pub fn referenced_names(text: &str, into: &mut std::collections::HashSet<String>) {
    let mut rest = text;
    while let Some(at) = rest.find(URL_PREFIX) {
        rest = &rest[at + URL_PREFIX.len()..];
        // A store name runs to the first character that cannot be in one.
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_')
            .unwrap_or(rest.len());
        if end > 0 {
            into.insert(rest[..end].to_string());
        }
        rest = &rest[end..];
    }
}

/// Every image the store holds that nothing refers to, with what each one costs
/// on disk.
///
/// The store is append-only by design — that is what makes every past backup
/// still resolve — so nothing here is deleted without being asked for. But
/// nothing ever said how much had accumulated either, and a screenshot pasted
/// and then undone is a file nobody will ever see again.
pub fn unreferenced(dir: &Path, referenced: &std::collections::HashSet<String>) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(store_dir(dir)) else {
        return Vec::new();
    };
    let mut orphans: Vec<(PathBuf, u64)> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            // Only files this store wrote. A temp file mid-import lives here
            // too, and deleting one fails the import it belongs to.
            let extension = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)?;
            if !ACCEPTED.contains(&extension.as_str()) {
                return None;
            }
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            Some((name, e.path(), bytes))
        })
        .filter(|(name, _, _)| !referenced.contains(name))
        .map(|(_, path, bytes)| (path, bytes))
        .collect();
    orphans.sort();
    orphans
}

/// Move the image store from one note's folder to another.
///
/// References are relative — `images/<hash>.png` — so the pictures have to go
/// where the note goes, or every one of them turns into a missing plate. A
/// rename first, since that is instant on the same volume; a copy when it is
/// not, keeping the originals so a failure halfway leaves the old folder whole.
pub fn move_store(from: &Path, to: &Path) -> std::io::Result<()> {
    let (source, target) = (store_dir(from), store_dir(to));
    if source == target || !source.is_dir() {
        return Ok(());
    }
    if target.exists() {
        // Both folders hold a store: merge by copying what the target lacks.
        // Names are content hashes, so a name that exists on both sides is the
        // same picture.
        for entry in std::fs::read_dir(&source)?.flatten() {
            let landing = target.join(entry.file_name());
            if !landing.exists() {
                std::fs::copy(entry.path(), landing)?;
            }
        }
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&source, &target).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(&target)?;
    for entry in std::fs::read_dir(&source)?.flatten() {
        std::fs::copy(entry.path(), target.join(entry.file_name()))?;
    }
    Ok(())
}

/// Whether a dropped file is worth trying to import.
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| ACCEPTED.contains(&e.as_str()))
}

/// The line a reference is written as.
pub fn format_reference(name: &str, w: u32, h: u32) -> String {
    format!("![|{w}x{h}]({URL_PREFIX}{name})")
}

/// Parse a line that is *only* an image reference into the store.
///
/// Deliberately strict. A line with anything else on it is prose that happens
/// to contain an image, and prose is text: it keeps its ordinary row, its
/// caret, and its inline highlighting. Only a line that is nothing but the
/// reference becomes a picture.
pub fn parse_reference(line: &str) -> Option<Reference> {
    let line = line.trim();
    let rest = line.strip_prefix("![")?;
    let close = rest.find("](")?;
    let (alt, rest) = rest.split_at(close);
    let url = rest.strip_prefix("](")?.strip_suffix(')')?;
    // One reference per line, and nothing after it: a `)` inside the URL would
    // mean the line did not end where this parse thinks it did.
    if url.contains(')') || url.contains('(') {
        return None;
    }
    let name = url.strip_prefix(URL_PREFIX)?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(Reference {
        name: name.to_string(),
        box_: parse_box(alt),
        alt: alt.to_string(),
    })
}

/// Rewrite every reference into the store as an absolute path.
///
/// A copy saved elsewhere is no longer beside the store, so `images/x.png`
/// resolves to nothing from wherever it was put. Exporting the note with the
/// real paths in it is the difference between a copy that opens and a copy that
/// is full of dead links.
pub fn absolutise(text: &str, dir: &Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        // Inside a fenced block a reference is a sample of the syntax, not a
        // picture — `render_row` does not draw it, so an export must not rewrite
        // it either, or documenting the format mangles the documentation.
        if body.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        match parse_reference(body) {
            Some(reference) => {
                // Keep whatever sat either side of the reference — leading
                // indentation is part of the note, not noise to be tidied away
                // by an export.
                let lead = &body[..body.len() - body.trim_start().len()];
                let trail = &body[body.trim_end().len()..];
                out.push_str(lead);
                // Angle-bracketed, because the destination is an absolute path
                // and this app's own directory is "Application Support" — a
                // bare destination with a space in it is not a CommonMark link,
                // so every exported note carried dead links.
                out.push_str(&format!(
                    "![{}](<{}>)",
                    reference.alt,
                    path(dir, &reference.name).display()
                ));
                out.push_str(trail);
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

/// `…|420x263` → `(420, 263)`. Anything else is alt text, not a size.
fn parse_box(alt: &str) -> Option<(u32, u32)> {
    let (w, h) = alt.rsplit_once('|')?.1.split_once('x')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Store `bytes`, returning the name to refer to them by.
///
/// Runs off the main thread — hashing and writing twenty megabytes is not
/// something a frame should wait for.
pub fn import_bytes(dir: &Path, bytes: &[u8]) -> io::Result<Import> {
    let (bytes, ext) = match sniff(bytes) {
        Some(ext) => (bytes.to_vec(), ext),
        // Not something the decoder can read — HEIC, PDF, PSD. macOS can read
        // it, so hand it to macOS once, here, rather than failing at every
        // future draw.
        None => (transcode_to_png(bytes)?, "png".to_string()),
    };
    let (w, h) = dimensions(&bytes)?;
    let name = format!("{}.{ext}", hash_hex(&bytes));
    let path = path(dir, &name);
    // Content addressed: if it is already there, it is already identical.
    if !path.exists() {
        std::fs::create_dir_all(store_dir(dir))?;
        crate::persist::write_atomically_bytes(&path, &bytes)?;
    }
    Ok(Import { name, w, h })
}

/// Store the contents of a file that was dropped on the window.
pub fn import_file(dir: &Path, src: &Path) -> io::Result<Import> {
    import_bytes(dir, &std::fs::read(src)?)
}

/// The extension to store under, or `None` when the decoder cannot read this
/// at all. Sniffed from the bytes, never from the name a file arrived with.
fn sniff(bytes: &[u8]) -> Option<String> {
    let format = image::guess_format(bytes).ok()?;
    let ext = format.extensions_str().first()?.to_string();
    NATIVE.contains(&ext.as_str()).then_some(ext)
}

/// Read the pixel size out of the header. Cheap: it does not decode the image.
fn dimensions(bytes: &[u8]) -> io::Result<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Convert anything macOS can read into a PNG, via `sips`, which ships with the
/// system. This is the only path an iPhone photo can take: `image` cannot
/// decode HEIC and neither can GPUI, so a heic left as it arrived would be a
/// reference that never draws.
fn transcode_to_png(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("gravitynote-import-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let src = dir.join("in");
    let dst = dir.join("out.png");
    std::fs::write(&src, bytes)?;
    let status = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(&src)
        .arg("--out")
        .arg(&dst)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    let out = if status.success() {
        std::fs::read(&dst)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an image this Mac can read",
        ))
    };
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// The first 8 bytes of a SHA-256, hex. Sixteen hex digits name every image a
/// person will ever paste with room to spare, and a short name keeps the note
/// readable — the reference is text someone has to look at.
fn hash_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;

    #[test]
    fn unreferenced_finds_only_what_the_note_forgot() {
        let dir = std::env::temp_dir().join(format!("gn-reclaim-{}", std::process::id()));
        let store = dir.join(DIR_NAME);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&store).expect("scratch");
        for name in ["kept.png", "orphan.png"] {
            std::fs::write(store.join(name), b"x").expect("write");
        }
        // A reference with a caption after it, which is not a line that is
        // *only* a reference — and is still a reference.
        let mut referenced = std::collections::HashSet::new();
        referenced_names("text\n![|10x10](images/kept.png) — the crash\nmore", &mut referenced);
        assert!(referenced.contains("kept.png"));
        let found = unreferenced(&dir, &referenced);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].0.ends_with("orphan.png"));
        assert_eq!(found[0].1, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gravitynote-images-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A 2x1 PNG, encoded here so the tests need no fixture file.
    fn png_2x1() -> Vec<u8> {
        let mut out = Vec::new();
        let img = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_reference_round_trips() {
        let line = format_reference("9f3a1c2b7e40d115.png", 420, 263);
        assert_eq!(line, "![|420x263](images/9f3a1c2b7e40d115.png)");
        assert_eq!(
            parse_reference(&line),
            Some(Reference {
                name: "9f3a1c2b7e40d115.png".into(),
                box_: Some((420, 263)),
                alt: "|420x263".into(),
            })
        );
    }

    #[test]
    fn a_reference_without_a_size_is_legal() {
        assert_eq!(
            parse_reference("![](images/a.png)"),
            Some(Reference {
                name: "a.png".into(),
                box_: None,
                alt: String::new(),
            })
        );
        // Alt text that is not a size stays alt text.
        assert_eq!(
            parse_reference("![a photo](images/a.png)").unwrap().box_,
            None
        );
        // Alt text *and* a size.
        assert_eq!(
            parse_reference("![a photo|10x20](images/a.png)")
                .unwrap()
                .box_,
            Some((10, 20))
        );
    }

    #[test]
    fn only_a_line_that_is_nothing_but_a_reference_is_an_image() {
        for line in [
            "see ![](images/a.png)",
            "![](images/a.png) and more",
            "![](images/a.png)![](images/b.png)",
            "![](https://example.com/a.png)",   // not in the store
            "![](images/sub/a.png)",            // no directories in the store
            "![](images/)",                     // no name
            "[](images/a.png)",                 // a link, not an image
            "![](a.png)",                       // not in the store
            "",
            "plain text",
        ] {
            assert_eq!(parse_reference(line), None, "{line:?} must not be an image");
        }
        // Surrounding whitespace is not "something else on the line".
        assert!(parse_reference("   ![](images/a.png)  ").is_some());
    }

    #[test]
    fn a_zero_size_is_not_a_size() {
        assert_eq!(parse_reference("![|0x10](images/a.png)").unwrap().box_, None);
        assert_eq!(parse_reference("![|10x0](images/a.png)").unwrap().box_, None);
        assert_eq!(parse_reference("![|x](images/a.png)").unwrap().box_, None);
        assert_eq!(parse_reference("![|10](images/a.png)").unwrap().box_, None);
    }

    #[test]
    fn importing_twice_writes_one_file() {
        let dir = scratch("dedup");
        let bytes = png_2x1();
        let a = import_bytes(&dir, &bytes).unwrap();
        let b = import_bytes(&dir, &bytes).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.w, 2);
        assert_eq!(a.h, 1);
        assert!(a.name.ends_with(".png"));
        let files: Vec<_> = std::fs::read_dir(store_dir(&dir)).unwrap().collect();
        assert_eq!(files.len(), 1, "the same bytes must be one file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_bytes_get_different_names() {
        let dir = scratch("distinct");
        let mut other = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            2,
            image::Rgba([0, 0, 255, 255]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut other),
            image::ImageFormat::Png,
        )
        .unwrap();
        let a = import_bytes(&dir, &png_2x1()).unwrap();
        let b = import_bytes(&dir, &other).unwrap();
        assert_ne!(a.name, b.name);
        assert_eq!((b.w, b.h), (3, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stored_bytes_are_the_bytes_that_arrived() {
        let dir = scratch("verbatim");
        let bytes = png_2x1();
        let import = import_bytes(&dir, &bytes).unwrap();
        assert_eq!(std::fs::read(path(&dir, &import.name)).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn junk_is_not_an_image() {
        let dir = scratch("junk");
        assert!(import_bytes(&dir, b"this is not an image at all").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_exported_note_carries_paths_that_resolve() {
        let dir = Path::new("/tmp/x");
        let note = "before\n![|10x20](images/a.png)\nafter\n![](images/b.png)";
        assert_eq!(
            absolutise(note, dir),
            "before\n![|10x20](</tmp/x/images/a.png>)\nafter\n![](</tmp/x/images/b.png>)"
        );
        // The real store lives under "Application Support", so the path this
        // has to survive always contains a space.
        assert_eq!(
            absolutise("![|1x2](images/a.png)", Path::new("/U/Application Support/gn")),
            "![|1x2](</U/Application Support/gn/images/a.png>)"
        );
        // A line that is not a reference is left exactly as it was, including
        // one that merely mentions an image.
        let prose = "see ![](images/a.png) there\n\n# heading\n";
        assert_eq!(absolutise(prose, dir), prose);
        // A reference inside a code fence is a sample, and stays one.
        let fenced = "```\n![|1x2](images/a.png)\n```\n";
        assert_eq!(absolutise(fenced, dir), fenced);
        // Indentation and alt text both survive the rewrite.
        assert_eq!(
            absolutise("    ![a photo|10x20](images/a.png)\n", dir),
            "    ![a photo|10x20](</tmp/x/images/a.png>)\n"
        );
    }

    #[test]
    fn the_store_sits_beside_the_note() {
        let dir = PathBuf::from("/tmp/x");
        assert_eq!(path(&dir, "a.png"), PathBuf::from("/tmp/x/images/a.png"));
        assert!(path(&dir, "a.png").is_absolute());
    }

    #[test]
    fn droppable_files_are_recognised_by_extension() {
        assert!(is_image_file(Path::new("/a/b.PNG")));
        assert!(is_image_file(Path::new("/a/b.heic")));
        assert!(!is_image_file(Path::new("/a/b.md")));
        assert!(!is_image_file(Path::new("/a/b")));
    }
}
