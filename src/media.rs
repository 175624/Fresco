//! Which file extensions Fresco treats as which kind of media, and the folder
//! scan built on them.
//!
//! This is the one list. The GUI classifies files with it when importing
//! (`gui::library::{is_image, is_video}`) and the daemon resolves a slideshow
//! folder with it ([`slideshow_frames`]). They used to keep a list each, and
//! the lists drifted: the daemon's slideshow scan accepted `.gif` while the
//! GUI's `is_image` did not, so the GUI could look at a folder, find no images,
//! and thumbnail/preview nothing while the daemon happily played it. Parity is
//! now structural, and the GUI library's `gui_and_daemon_agree_on_slideshow_media`
//! test pins it.

use std::path::{Path, PathBuf};

/// Still images: one frame per file, shown for a slideshow interval.
pub const STILL_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "tiff"];

/// Moving media that a video or playlist entry plays.
///
/// `gif` is deliberately here and not in [`STILL_EXTS`]: it is animated, so
/// importing one as a still would freeze it on frame one (or flash past at
/// image-slideshow pace). A single GIF therefore becomes a looping video entry.
pub const VIDEO_EXTS: &[&str] = &["mp4", "webm", "mkv", "avi", "mov", "flv", "gif"];

/// The one animated format a *slideshow* also accepts. A folder-backed
/// slideshow has always shown the GIFs in its folder (the daemon plays each
/// one, looping, until the interval cuts to the next), and existing
/// slideshows must keep doing so; what changed is only that the GUI now
/// agrees about it. Real video files stay out: a slideshow cuts every
/// `interval_s`, which is the wrong sequencer for a clip (that is what a
/// playlist is for).
const SLIDESHOW_ANIMATED_EXT: &str = "gif";

fn has_ext(p: &Path, list: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// A still image (never a GIF).
pub fn is_still(p: &Path) -> bool {
    has_ext(p, STILL_EXTS)
}

/// Moving media a video/playlist entry can play (GIF included).
pub fn is_video(p: &Path) -> bool {
    has_ext(p, VIDEO_EXTS)
}

/// Any file Fresco can import.
pub fn is_supported(p: &Path) -> bool {
    is_still(p) || is_video(p)
}

/// A file a timed slideshow cycles through: a still, or a GIF.
pub fn is_slideshow_frame(p: &Path) -> bool {
    is_still(p) || has_ext(p, &[SLIDESHOW_ANIMATED_EXT])
}

/// How deep a recursive scan descends. A bound rather than a full walk:
/// `is_dir` follows symlinks, so an unbounded descent can loop forever on a
/// self-referential link, and a wallpaper folder nested more than a few levels
/// deep is not what "add this folder" means.
pub const MAX_SCAN_DEPTH: usize = 4;

/// Every file under `dir` that `keep` accepts, in directory order (callers sort).
/// Non-recursive unless `recursive`, and then bounded by [`MAX_SCAN_DEPTH`].
/// A missing or unreadable directory yields nothing rather than an error: both
/// callers want "no media" either way.
pub fn scan(dir: &Path, recursive: bool, keep: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_into(
        dir,
        if recursive { MAX_SCAN_DEPTH } else { 0 },
        keep,
        &mut out,
    );
    out
}

fn scan_into(dir: &Path, depth: usize, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                scan_into(&p, depth - 1, keep, out);
            }
        } else if keep(&p) {
            out.push(p);
        }
    }
}

/// The files a folder-backed slideshow plays, in play order (sorted by path).
///
/// The daemon builds its image list from this and the GUI uses it for the
/// card thumbnail, the editor preview and the "does this slideshow have
/// anything to show" health check, so what the user is shown is exactly what
/// will play.
pub fn slideshow_frames(folder: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut v = scan(folder, recursive, &is_slideshow_frame);
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_lists_do_not_overlap_and_gif_is_not_a_still() {
        for e in STILL_EXTS {
            assert!(
                !VIDEO_EXTS.contains(e),
                "{e} is in both lists; a file must have exactly one category"
            );
        }
        assert!(!is_still(Path::new("a.gif")));
        assert!(is_video(Path::new("a.GIF")));
        assert!(is_slideshow_frame(Path::new("a.gif")));
        assert!(!is_slideshow_frame(Path::new("a.mp4")));
        assert!(!is_supported(Path::new("notes.txt")));
        assert!(!is_supported(Path::new("noextension")));
    }

    #[test]
    fn slideshow_frames_are_stills_and_gifs_sorted_and_depth_aware() {
        let dir = std::env::temp_dir().join(format!("fresco-media-frames-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for f in ["b.png", "a.GIF", "clip.mp4", "notes.txt"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::write(sub.join("c.jpg"), b"x").unwrap();

        let names = |v: Vec<PathBuf>| {
            v.iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        // Videos and text never reach a slideshow; the GIF does (as it always
        // has); upper-case sorts before lower-case, as the daemon always did.
        assert_eq!(names(slideshow_frames(&dir, false)), ["a.GIF", "b.png"]);
        assert_eq!(
            names(slideshow_frames(&dir, true)),
            ["a.GIF", "b.png", "c.jpg"]
        );
        assert!(slideshow_frames(&dir.join("missing"), true).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
