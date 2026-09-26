//! Decoded images, and how many of them are allowed to be resident.
//!
//! # Why this exists at all
//!
//! GPUI will happily load an image for you: `img(path)` goes through
//! `App::fetch_asset`, whose cache is an `FxHashMap` that is **never evicted**
//! (`app.rs:548`). `RetainAllImageCache` is the same promise in its name. A
//! decoded image is `w × h × 4` bytes with no compression, so fifty pasted
//! screenshots is half a gigabyte that is never given back, for the life of the
//! process. In an app whose whole performance contract is about surviving
//! twenty years of accumulation, that is the same class of mistake as
//! highlighting the whole document every frame.
//!
//! So images are resolved here instead. Three things follow from that, and each
//! is load-bearing:
//!
//! * **Decoding downscales.** Nothing is kept at more than [`MAX_EDGE`] on its
//!   longest side, which is well past what this app can ever draw. That bounds
//!   memory *and* decode time with one number, and it is what lets the store
//!   keep the pristine original on disk without paying for it in RAM.
//! * **Eviction happens between frames, never during one.** Dropping an image
//!   returns its slot to the sprite atlas, and the atlas recycles slots
//!   immediately — so freeing one that a primitive in the current frame still
//!   points at would corrupt what is on screen. [`Bank::begin_frame`] only ever
//!   evicts something that has not been asked for in two frames.
//! * **A decode in flight is remembered.** Without that, every frame would
//!   start a fresh decode of every visible image, forever, because none of them
//!   would ever be finished when the next frame asked.
//!
//! # What it is not
//!
//! Not an `impl gpui::ImageCache`. That trait is only reachable through the
//! `img()` element, and its `load` is called *during* layout and again during
//! paint — exactly where eviction must not happen. Resolving first and handing
//! the result to `ImageSource::Render` keeps one path, and keeps the eviction
//! decision outside the frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::Shared;
use futures::FutureExt;
use gpui::{App, Entity, RenderImage, Task, Window};

/// The longest edge anything is kept at, in pixels.
///
/// The widest this app can draw an image is the reading measure, which at the
/// largest text size is about 1450pt — 2900 device pixels on a Retina display.
/// 3072 clears that with room, and caps one image at about 37 MB rather than
/// whatever a modern phone camera produces (a 48-megapixel photo decodes to
/// 190 MB).
pub const MAX_EDGE: u32 = 3072;

/// How much decoded image is allowed to be resident, in bytes.
///
/// Deliberately modest. Every resident image also has a copy in the Metal
/// sprite atlas, whose pages are never handed back to the OS, so the real cost
/// of a resident set is roughly twice what is accounted here.
pub const BUDGET: usize = 64 * 1024 * 1024;

enum Entry {
    Loading {
        task: Shared<Task<Option<Arc<RenderImage>>>>,
        /// The last frame this image was asked for, exactly as `Ready` records
        /// it. A decode that lands after its row has scrolled away would
        /// otherwise sit here forever: `Shared` holds the finished image alive,
        /// so it would be resident, uncounted, and impossible to evict — the
        /// very failure this module exists to prevent, with the accounting
        /// blind to it.
        used: u64,
    },
    Ready {
        image: Arc<RenderImage>,
        bytes: usize,
        /// The last frame this image was asked for. The eviction rule is
        /// expressed against this and nothing else.
        used: u64,
    },
    /// Missing, unreadable, or not an image. Remembered so a broken reference
    /// costs one failed decode rather than one per frame forever.
    Failed,
}

/// What the bank can say about one reference, this frame.
pub enum Resolved {
    /// Being decoded. The view is notified when it lands.
    Loading,
    Ready(Arc<RenderImage>),
    /// Not there, or not readable. Remembered, so a broken reference costs
    /// one failed decode rather than one per frame for the life of the session.
    Missing,
}

/// Decoded images, bounded.
pub struct Bank {
    entries: HashMap<PathBuf, Entry>,
    resident: usize,
    frame: u64,
    /// How many entries are still decoding. Only when this is non-zero does
    /// `begin_frame` need to walk the map looking for one that has landed —
    /// otherwise every frame would scan every image the session has ever
    /// touched, `Failed` ones included, to find nothing.
    loading: usize,
}

impl Default for Bank {
    fn default() -> Self {
        Self::new()
    }
}

impl Bank {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            resident: 0,
            frame: 0,
            loading: 0,
        }
    }

    /// What the caller should draw this frame. Never blocks, never decodes on
    /// this thread.
    ///
    /// The three answers are distinct because they look different on screen: a
    /// decode in flight is a quiet placeholder that will fill in, and a
    /// reference to something that is not there is a fault the reader has to be
    /// told about, not an image that is taking its time.
    pub fn get(&mut self, path: &Path, window: &mut Window, cx: &mut App) -> Resolved {
        let frame = self.frame;
        match self.entries.get_mut(path) {
            Some(Entry::Ready { image, used, .. }) => {
                *used = frame;
                return Resolved::Ready(image.clone());
            }
            Some(Entry::Failed) => return Resolved::Missing,
            Some(Entry::Loading { task, used }) => {
                *used = frame;
                // Finished since the last frame asked: promote it in place.
                let task = task.clone();
                if let Some(done) = task.now_or_never() {
                    return self.store(path.to_path_buf(), done);
                }
                return Resolved::Loading;
            }
            None => {}
        }

        let owned = path.to_path_buf();
        let task = cx
            .background_executor()
            .spawn(async move { decode(&owned, MAX_EDGE) })
            .shared();
        self.entries.insert(
            path.to_path_buf(),
            Entry::Loading {
                task: task.clone(),
                used: frame,
            },
        );
        self.loading += 1;

        // Wake the view when the decode lands, or the image would sit there
        // undrawn until something else happened to cause a frame.
        let view = window.current_view();
        window
            .spawn(cx, async move |cx| {
                task.await;
                cx.on_next_frame(move |_, cx| cx.notify(view));
            })
            .detach();
        Resolved::Loading
    }

    fn store(&mut self, path: PathBuf, decoded: Option<Arc<RenderImage>>) -> Resolved {
        if matches!(self.entries.get(&path), Some(Entry::Loading { .. })) {
            self.loading -= 1;
        }
        match decoded {
            Some(image) => {
                let bytes = bytes_of(&image);
                self.resident += bytes;
                self.entries.insert(
                    path,
                    Entry::Ready {
                        image: image.clone(),
                        bytes,
                        used: self.frame,
                    },
                );
                Resolved::Ready(image)
            }
            None => {
                self.entries.insert(path, Entry::Failed);
                Resolved::Missing
            }
        }
    }

    /// Open a frame, and make room if the last one left too much resident.
    ///
    /// Called at the top of `render`, which is *after* the previous frame was
    /// painted. Nothing used in the previous frame is touched — its pixels may
    /// still be on screen, and its atlas slot must not be recycled underneath
    /// them.
    pub fn begin_frame(&mut self, window: &mut Window) {
        self.frame += 1;

        // Collect anything that finished decoding but was never asked for
        // again, so it enters the accounting and becomes evictable. Without
        // this a fast scroll through a note full of pictures leaves every one
        // of them resident and invisible to the budget.
        let landed: Vec<PathBuf> = if self.loading == 0 {
            Vec::new()
        } else {
            self
            .entries
            .iter()
            .filter_map(|(path, entry)| match entry {
                Entry::Loading { task, .. } if task.peek().is_some() => Some(path.clone()),
                _ => None,
            })
            .collect()
        };
        for path in landed {
            let done = match self.entries.get(&path) {
                Some(Entry::Loading { task, .. }) => task.clone().now_or_never().flatten(),
                _ => continue,
            };
            self.store(path, done);
        }

        if self.resident <= BUDGET {
            return;
        }
        let cutoff = self.frame.saturating_sub(2);
        let mut cold: Vec<(u64, PathBuf)> = self
            .entries
            .iter()
            .filter_map(|(path, entry)| match entry {
                Entry::Ready { used, .. } if *used <= cutoff => Some((*used, path.clone())),
                _ => None,
            })
            .collect();
        // Least recently asked for goes first.
        cold.sort_unstable_by_key(|(used, _)| *used);
        for (_, path) in cold {
            if self.resident <= BUDGET {
                break;
            }
            if let Some(Entry::Ready { image, bytes, .. }) = self.entries.remove(&path) {
                self.resident -= bytes;
                let _ = window.drop_image(image);
            }
        }
    }

    #[cfg(test)]
    pub fn resident_bytes(&self) -> usize {
        self.resident
    }
}

/// What one decoded image costs, counting every frame of an animation we kept.
fn bytes_of(image: &Arc<RenderImage>) -> usize {
    (0..image.frame_count())
        .map(|i| {
            let size = image.size(i);
            (size.width.0 as usize) * (size.height.0 as usize) * 4
        })
        .sum()
}

/// Read, decode, downscale, and hand back something the renderer can draw.
///
/// Runs on the background executor. Only the first frame of an animation is
/// kept: this is a note app, and a two-hundred-frame GIF would evict every
/// other image by itself.
fn decode(path: &Path, max_edge: u32) -> Option<Arc<RenderImage>> {
    let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let image = reader.decode().ok()?;
    let (w, h) = (image.width(), image.height());
    let image = if w.max(h) > max_edge {
        // `Triangle` rather than `Lanczos3`: for a downscale to a size nobody
        // will pixel-peep, the difference is invisible and the cost is not.
        let scale = max_edge as f32 / w.max(h) as f32;
        image.resize(
            (w as f32 * scale).round().max(1.) as u32,
            (h as f32 * scale).round().max(1.) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let mut rgba = image.into_rgba8();
    // `RenderImage` is BGRA, which is what the GPU wants; the decoder gives
    // RGBA. This is the same swap GPUI's own loader does.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

/// The bank lives in the view, so the view is what owns its lifetime.
pub type BankHandle = Entity<Bank>;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, w: u32, h: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            w,
            h,
            image::Rgba([1, 2, 3, 255]),
        ))
        .save(path)
        .unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gravitynote-bank-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn decoding_downscales_to_the_cap_and_keeps_the_aspect_ratio() {
        let dir = scratch("downscale");
        let path = dir.join("wide.png");
        write_png(&path, 800, 400);
        let image = decode(&path, 100).expect("decodes");
        let size = image.size(0);
        assert_eq!(size.width.0, 100);
        assert_eq!(size.height.0, 50);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_under_the_cap_is_left_alone() {
        let dir = scratch("small");
        let path = dir.join("small.png");
        write_png(&path, 40, 20);
        let image = decode(&path, 100).expect("decodes");
        assert_eq!((image.size(0).width.0, image.size(0).height.0), (40, 20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_broken_file_decodes_to_nothing_rather_than_panicking() {
        let dir = scratch("broken");
        assert!(decode(&dir.join("nope.png"), 100).is_none());
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"not a png").unwrap();
        assert!(decode(&junk, 100).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_resident_total_is_the_sum_of_what_is_held() {
        // The one invariant with a silent failure mode: `resident` must equal
        // the sum of the live `Ready` entries, or the budget is enforced
        // against a number that means nothing.
        let dir = scratch("accounting");
        let mut bank = Bank::new();
        assert_eq!(bank.resident_bytes(), 0);

        let mut expected = 0;
        for (i, (w, h)) in [(100u32, 50u32), (60, 60), (10, 200)].iter().enumerate() {
            let path = dir.join(format!("{i}.png"));
            write_png(&path, *w, *h);
            let image = decode(&path, MAX_EDGE).unwrap();
            expected += bytes_of(&image);
            bank.store(path, Some(image));
        }
        assert_eq!(bank.resident_bytes(), expected);

        // A failed decode is remembered but costs nothing.
        bank.store(dir.join("gone.png"), None);
        assert_eq!(bank.resident_bytes(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_byte_count_is_the_decoded_size_not_the_file_size() {
        let dir = scratch("bytes");
        let path = dir.join("flat.png");
        // A flat colour compresses to almost nothing on disk and to exactly
        // w*h*4 in memory — which is the whole reason this accounting exists.
        write_png(&path, 200, 100);
        let image = decode(&path, MAX_EDGE).unwrap();
        assert_eq!(bytes_of(&image), 200 * 100 * 4);
        assert!(std::fs::metadata(&path).unwrap().len() < 200 * 100 * 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
