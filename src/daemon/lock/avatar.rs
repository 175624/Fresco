//! The lock-screen avatar: decoding the picture [`crate::userinfo`] found, and
//! keeping it fresh for the one caller that outlives a lock.
//!
//! # Decoding
//!
//! [`decode_avatar_file`] reads the file through the same sniffing,
//! limit-bounded decoder the album-art path uses ([`crate::artwork::decode_art`]
//! — PNG, JPEG and WebP; the `image` crate is built with exactly those), then
//! makes the result fit the slot it is about to be drawn into:
//!
//! * **Centre-cropped to a square.** [`Canvas::image`] stretches its source
//!   to the destination rectangle, so a 4:3 `~/.face` photo drawn raw would be
//!   squashed into the disc.
//! * **Shrunk to [`AVATAR_MAX_PX`].** The same call refuses a side past
//!   `MAX_CANVAS_PX` outright, drawing nothing — which on a phone-camera
//!   `~/.face` would have left an empty ring instead of a face — and a disc a
//!   few dozen logical pixels wide has no use for 12 megapixels held in memory
//!   for the whole lock.
//!
//! # Freshness
//!
//! A real lock builds a new [`super::engine::LockEngine`] and so resolves the
//! avatar afresh every time the screen locks — nothing to cache there. The
//! preview renderer is different: it lives as long as the daemon, so a picture
//! resolved once at startup would outlive the user changing it in their
//! settings. [`AvatarCache`] is what it holds instead. It re-asks where the
//! picture is every [`RESOLVE_TTL`] (the path itself changes when a desktop
//! writes each new choice to a fresh file, as deepin does) and, on every call,
//! re-decodes only if the file's modification time or length moved — so a
//! preview that fires many times a second while a slider is dragged costs one
//! `stat`, not a decode.
//!
//! [`Canvas::image`]: crate::widgetkit::canvas::Canvas::image

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use image::RgbaImage;

use crate::artwork;

/// Longest side kept after decoding. The disc is a few dozen to a couple of
/// hundred device pixels; 512 leaves a bilinear sampler plenty of detail on a
/// HiDPI output and bounds the retained image at 1 MiB.
const AVATAR_MAX_PX: u32 = 512;

/// How long [`AvatarCache`] trusts the path it last resolved. Resolution costs
/// a few `gdbus` round trips, so it is rate-limited; a changed *file* is
/// noticed immediately regardless (see the module docs).
const RESOLVE_TTL: Duration = Duration::from_secs(3);

/// Decode the avatar at `path`, or `None` for anything unreadable, oversized,
/// not a PNG/JPEG/WebP, or corrupt — the caller falls back to initials.
pub(super) fn decode_avatar_file(path: &Path) -> Option<Arc<RgbaImage>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(artwork::MAX_ART_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() as u64 > artwork::MAX_ART_BYTES {
        return None;
    }
    let img = artwork::decode_art(&bytes).ok()?;
    Some(Arc::new(fit_square(img)))
}

/// Centre-crop to a square, then shrink to at most [`AVATAR_MAX_PX`] a side.
fn fit_square(img: RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let side = w.min(h);
    if side == 0 {
        return img;
    }
    let square = if w == h {
        img
    } else {
        image::imageops::crop_imm(&img, (w - side) / 2, (h - side) / 2, side, side).to_image()
    };
    if side <= AVATAR_MAX_PX {
        return square;
    }
    // Triangle, not Nearest: this runs once per picture, and a 10x
    // nearest-neighbour reduction of a photo aliases visibly.
    image::imageops::resize(
        &square,
        AVATAR_MAX_PX,
        AVATAR_MAX_PX,
        image::imageops::FilterType::Triangle,
    )
}

/// What `stat` says about the file a decode came from. Compared, never
/// interpreted: any difference means "decode again".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

/// One resolved picture: where it is, which version of the file was decoded,
/// and the result (`None` when that version would not decode).
struct Entry {
    path: PathBuf,
    /// The file's stamp when `image` was produced. Meaningful only once
    /// `loaded`.
    stamp: Option<Stamp>,
    loaded: bool,
    image: Option<Arc<RgbaImage>>,
}

/// A decoded avatar kept current across many reads. See the module docs.
pub(super) struct AvatarCache {
    /// Where is the picture right now? [`crate::userinfo::current_avatar`] in
    /// production; a closure in tests.
    resolve: Box<dyn Fn() -> Option<PathBuf> + Send>,
    resolved_at: Option<Instant>,
    entry: Option<Entry>,
}

impl AvatarCache {
    pub(super) fn new() -> Self {
        Self::with_resolver(Box::new(crate::userinfo::current_avatar))
    }

    fn with_resolver(resolve: Box<dyn Fn() -> Option<PathBuf> + Send>) -> Self {
        AvatarCache {
            resolve,
            resolved_at: None,
            entry: None,
        }
    }

    /// The current avatar as of `now`: re-resolved if the last resolution is
    /// older than [`RESOLVE_TTL`], re-decoded if the file changed since it was
    /// last decoded, otherwise the very same [`Arc`] as last time.
    pub(super) fn get(&mut self, now: Instant) -> Option<Arc<RgbaImage>> {
        let due = self
            .resolved_at
            .is_none_or(|at| now.saturating_duration_since(at) >= RESOLVE_TTL);
        if due {
            self.resolved_at = Some(now);
            self.adopt((self.resolve)());
        }

        let entry = self.entry.as_mut()?;
        // Stamp *before* decoding: a write that lands mid-decode then shows up
        // as a changed stamp on the next call rather than being papered over.
        let stamp = stamp_of(&entry.path);
        if !entry.loaded || stamp != entry.stamp {
            entry.image = decode_avatar_file(&entry.path);
            entry.stamp = stamp;
            entry.loaded = true;
        }
        entry.image.clone()
    }

    /// Switch to a freshly resolved `path`. The same path keeps its decoded
    /// image (and its stamp) so [`AvatarCache::get`] can tell whether the
    /// file itself moved.
    fn adopt(&mut self, path: Option<PathBuf>) {
        match path {
            None => self.entry = None,
            Some(p) if self.entry.as_ref().is_some_and(|e| e.path == p) => {}
            Some(p) => {
                self.entry = Some(Entry {
                    path: p,
                    stamp: None,
                    loaded: false,
                    image: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fresco-avatar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_png(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        img.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    // -- decoding -----------------------------------------------------------

    #[test]
    fn a_square_png_decodes_as_is() {
        let d = scratch("square");
        let p = d.join("a.png");
        write_png(&p, 64, 64, [10, 20, 30]);
        let img = decode_avatar_file(&p).unwrap();
        assert_eq!(img.dimensions(), (64, 64));
        assert_eq!(img.get_pixel(5, 5).0, [10, 20, 30, 255]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wide_photo_is_centre_cropped_not_squashed() {
        let d = scratch("wide");
        let p = d.join("wide.png");
        // 300x100: red | green | blue thirds. The centre-square is the green
        // third; a stretched copy would still show red and blue.
        let img = RgbaImage::from_fn(300, 100, |x, _| match x / 100 {
            0 => image::Rgba([255, 0, 0, 255]),
            1 => image::Rgba([0, 255, 0, 255]),
            _ => image::Rgba([0, 0, 255, 255]),
        });
        img.save_with_format(&p, image::ImageFormat::Png).unwrap();
        let out = decode_avatar_file(&p).unwrap();
        assert_eq!(out.dimensions(), (100, 100));
        assert!(out.pixels().all(|px| px.0 == [0, 255, 0, 255]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tall_photo_is_centre_cropped_too() {
        let img = RgbaImage::from_fn(50, 150, |_, y| match y / 50 {
            1 => image::Rgba([9, 9, 9, 255]),
            _ => image::Rgba([200, 0, 0, 255]),
        });
        let out = fit_square(img);
        assert_eq!(out.dimensions(), (50, 50));
        assert!(out.pixels().all(|px| px.0 == [9, 9, 9, 255]));
    }

    #[test]
    fn a_large_photo_is_shrunk_to_the_cap() {
        let big = RgbaImage::from_pixel(2000, 1500, image::Rgba([1, 2, 3, 255]));
        let out = fit_square(big);
        assert_eq!(out.dimensions(), (AVATAR_MAX_PX, AVATAR_MAX_PX));
        // A source at exactly the cap is left alone.
        let edge = RgbaImage::new(AVATAR_MAX_PX, AVATAR_MAX_PX);
        assert_eq!(
            fit_square(edge).dimensions(),
            (AVATAR_MAX_PX, AVATAR_MAX_PX)
        );
    }

    #[test]
    fn degenerate_images_do_not_panic() {
        assert_eq!(fit_square(RgbaImage::new(0, 0)).dimensions(), (0, 0));
        assert_eq!(fit_square(RgbaImage::new(0, 7)).dimensions(), (0, 7));
        assert_eq!(fit_square(RgbaImage::new(1, 1)).dimensions(), (1, 1));
    }

    #[test]
    fn things_that_are_not_decodable_pictures_are_none() {
        let d = scratch("bad");
        let svg = d.join("me.svg");
        std::fs::write(
            &svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"/>",
        )
        .unwrap();
        let junk = d.join("junk.png");
        std::fs::write(&junk, b"definitely not a png").unwrap();
        let truncated = d.join("trunc.png");
        write_png(&truncated, 32, 32, [1, 1, 1]);
        let bytes = std::fs::read(&truncated).unwrap();
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        let empty = d.join("empty");
        std::fs::write(&empty, "").unwrap();

        for p in [&svg, &junk, &truncated, &empty, &d.join("missing.png"), &d] {
            assert!(decode_avatar_file(p).is_none(), "{p:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_format_is_judged_by_content_not_extension() {
        let d = scratch("sniff");
        let p = d.join("looks-like.jpg");
        write_png(&p, 16, 16, [4, 5, 6]);
        assert!(decode_avatar_file(&p).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    // -- the cache ----------------------------------------------------------

    /// A cache whose resolver reads `target` and counts its calls.
    fn cache_over(target: Arc<Mutex<Option<PathBuf>>>) -> (AvatarCache, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let cache = AvatarCache::with_resolver(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            target.lock().unwrap().clone()
        }));
        (cache, calls)
    }

    #[test]
    fn repeated_reads_reuse_the_decode_and_the_resolution() {
        let d = scratch("reuse");
        let p = d.join("a.png");
        write_png(&p, 32, 32, [1, 2, 3]);
        let (mut cache, calls) = cache_over(Arc::new(Mutex::new(Some(p))));

        let t0 = Instant::now();
        let a = cache.get(t0).unwrap();
        let b = cache.get(t0 + Duration::from_millis(500)).unwrap();
        let c = cache
            .get(t0 + RESOLVE_TTL - Duration::from_millis(1))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c), "decoded once");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "resolved once inside the TTL"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_path_is_re_resolved_after_the_ttl_but_an_unchanged_file_is_not_re_decoded() {
        let d = scratch("ttl");
        let p = d.join("a.png");
        write_png(&p, 32, 32, [1, 2, 3]);
        let (mut cache, calls) = cache_over(Arc::new(Mutex::new(Some(p))));

        let t0 = Instant::now();
        let a = cache.get(t0).unwrap();
        let b = cache.get(t0 + RESOLVE_TTL).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(Arc::ptr_eq(&a, &b), "same file, same decode");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_edited_file_is_re_decoded_without_waiting_for_the_ttl() {
        let d = scratch("edit");
        let p = d.join("a.png");
        write_png(&p, 32, 32, [1, 2, 3]);
        let (mut cache, calls) = cache_over(Arc::new(Mutex::new(Some(p.clone()))));

        let t0 = Instant::now();
        let a = cache.get(t0).unwrap();
        // A different size, so the stamp differs even on a filesystem whose
        // mtime is too coarse to tell two writes in one test apart.
        write_png(&p, 48, 48, [200, 100, 50]);
        let b = cache.get(t0 + Duration::from_millis(10)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no re-resolution needed");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(b.dimensions(), (48, 48));
        assert_eq!(b.get_pixel(0, 0).0, [200, 100, 50, 255]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_picture_at_a_new_path_is_picked_up_at_the_next_resolution() {
        // deepin writes every new choice to a fresh <login>-<ns>.png.
        let d = scratch("newpath");
        let old = d.join("roy-aaa.png");
        let new = d.join("roy-bbb.png");
        write_png(&old, 32, 32, [1, 1, 1]);
        write_png(&new, 32, 32, [9, 9, 9]);
        let target = Arc::new(Mutex::new(Some(old)));
        let (mut cache, _) = cache_over(target.clone());

        let t0 = Instant::now();
        assert_eq!(cache.get(t0).unwrap().get_pixel(0, 0).0, [1, 1, 1, 255]);
        *target.lock().unwrap() = Some(new);
        // Inside the TTL the old path is still trusted...
        assert_eq!(
            cache
                .get(t0 + Duration::from_secs(1))
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [1, 1, 1, 255]
        );
        // ...and after it, the new one wins.
        assert_eq!(
            cache.get(t0 + RESOLVE_TTL).unwrap().get_pixel(0, 0).0,
            [9, 9, 9, 255]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn losing_and_regaining_the_picture() {
        let d = scratch("lose");
        let p = d.join("a.png");
        write_png(&p, 32, 32, [1, 2, 3]);
        let target = Arc::new(Mutex::new(Some(p.clone())));
        let (mut cache, _) = cache_over(target.clone());

        let t0 = Instant::now();
        assert!(cache.get(t0).is_some());
        // The file vanishes under a still-trusted path: no image, no panic.
        std::fs::remove_file(&p).unwrap();
        assert!(cache.get(t0 + Duration::from_secs(1)).is_none());
        // It comes back (same path): decoded again.
        write_png(&p, 40, 40, [7, 7, 7]);
        assert_eq!(
            cache.get(t0 + Duration::from_secs(2)).unwrap().dimensions(),
            (40, 40)
        );
        // The resolver stops finding any picture: initials time.
        *target.lock().unwrap() = None;
        assert!(cache.get(t0 + RESOLVE_TTL).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_undecodable_file_is_not_retried_until_it_changes() {
        let d = scratch("noretry");
        let p = d.join("a.png");
        std::fs::write(&p, b"\x89PNG\r\n\x1a\nbroken").unwrap();
        let (mut cache, _) = cache_over(Arc::new(Mutex::new(Some(p.clone()))));

        let t0 = Instant::now();
        assert!(cache.get(t0).is_none());
        assert!(cache.get(t0 + Duration::from_secs(1)).is_none());
        write_png(&p, 20, 20, [3, 3, 3]);
        assert!(cache.get(t0 + Duration::from_secs(2)).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_production_cache_never_panics() {
        // Whatever this machine's accounts service looks like.
        let mut cache = AvatarCache::new();
        let _ = cache.get(Instant::now());
    }
}
