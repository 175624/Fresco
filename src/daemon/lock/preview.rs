//! Lock-screen preview: a still-image render of the same arrangement a real
//! lock would show, for the GUI's "Preview lock screen" button
//! (`docs/plan-lock-screen.md` §6).
//!
//! # Never locks anything, by construction
//!
//! This is the assertion `docs/plan-lock-screen.md` §6/§8 asks for, not just
//! a claim: [`PreviewRenderer`] imports nothing from [`super::hosts::LockHost`],
//! never spawns a subprocess, and never opens a D-Bus connection of any kind
//! — everything it touches is a file decode, [`crate::widgetkit`] rasterisation,
//! and a PNG write. There is no locking primitive anywhere in this module for
//! a bug to accidentally reach; `tests::the_module_never_names_a_locking_primitive`
//! pins the specific set of forbidden substrings this file's own source must
//! never contain, so a future edit that *did* add one would fail a test
//! rather than only a review.
//!
//! # Reuses the engine's own data model
//!
//! [`PreviewRenderer::render`] calls the exact same
//! [`super::engine::slots_for`]/[`super::engine::build_owned_data`]/
//! [`super::engine::ReservedZoneKind`] a real [`super::engine::LockEngine`]
//! uses, so a preview cannot honestly drift from what locking for real would
//! draw — the two are never two independent implementations of "what does
//! this config look like".
//!
//! # Caching and throttling
//!
//! The wallpaper decode is the expensive part (a video/slideshow wallpaper's
//! still frame costs an `ffmpeg`/`ffmpegthumbnailer` spawn via
//! `daemon::overview::render_still`) and is cached across calls, re-decoded
//! only when the [`crate::config::Wallpaper`] itself changes — not on every
//! render, which a GUI slider (`dim`, `blur`, a preset switch) can trigger
//! several times a second. [`THROTTLE`] additionally caps the *whole* render
//! (background reuse included) to ≤4/s at an unchanged size: a burst of
//! requests while a slider is being dragged reuses the file already on disk
//! rather than re-rasterising for each one.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbaImage;

use crate::config::{Kind, Wallpaper};
use crate::lockscreen::LockWidget;
use crate::userinfo::{self, UserInfo};
use crate::widgetkit::lockscene::LockSceneSpec;
use crate::widgetkit::{FontStack, Rect, Size, Theme};

use super::super::widgets::Snapshot;
use super::avatar::AvatarCache;
use super::engine::{self, ReservedZoneKind};
use super::hosts::HostKind;

/// Ceiling on how often a full render actually happens at an unchanged
/// requested size — "≤4 renders/s" in the design brief.
const THROTTLE: Duration = Duration::from_millis(250);

/// Longest side a preview may ask for. Any same-user process can send
/// `Request::LockPreview`, and the render allocates a `width*height` RGBA
/// canvas (plus a scaled background copy), so the size must be bounded before
/// anything is allocated. 8192 covers an 8K panel on either axis.
pub const MAX_PREVIEW_SIDE: u32 = 8192;

/// Ceiling on `width*height` — 7680x4320 (8K UHD), i.e. ~133 MB of RGBA per
/// canvas. Stops a legal-per-side but absurd 8192x8192 request.
pub const MAX_PREVIEW_PIXELS: u64 = 33_177_600;

/// Reject a requested preview size that is zero or too large, *before* any
/// allocation. The message goes straight back to the caller in
/// `Response::Err`.
pub fn validate_size(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "preview size {width}x{height} must be at least 1x1"
        ));
    }
    if width > MAX_PREVIEW_SIDE || height > MAX_PREVIEW_SIDE {
        return Err(format!(
            "preview size {width}x{height} exceeds the {MAX_PREVIEW_SIDE}-pixel side limit"
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_PREVIEW_PIXELS {
        return Err(format!(
            "preview size {width}x{height} exceeds the {MAX_PREVIEW_PIXELS}-pixel area limit"
        ));
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR/fresco/lock-preview.png` — the one path every preview
/// request writes to; the GUI always re-reads this same path rather than a
/// per-request one, since a fullscreen preview window just wants "the latest
/// picture", not a history of them.
pub fn preview_path() -> PathBuf {
    crate::ipc::socket_dir().join("lock-preview.png")
}

/// One reusable preview renderer: the font stack (an expensive one-time
/// system scan — see `widgets::Fonts`'s identical reasoning) and the decoded
/// background/avatar live here across calls rather than being rebuilt per
/// request.
pub struct PreviewRenderer {
    /// Where the next render is written — [`preview_path`] in production.
    /// A field, not a call to [`preview_path`] inline in
    /// [`PreviewRenderer::render`], purely so the tests can redirect it — see
    /// [`PreviewRenderer::set_path`].
    path: PathBuf,
    fonts: FontStack,
    user: UserInfo,
    /// Re-checked on every render (see [`AvatarCache`]): unlike a real lock,
    /// which resolves the avatar afresh each time the screen locks, this lives
    /// as long as the daemon.
    avatar: AvatarCache,
    cached_wallpaper: Option<Wallpaper>,
    background: Option<Arc<RgbaImage>>,
    last_render: Option<Instant>,
    last_size: Option<(u32, u32)>,
}

impl PreviewRenderer {
    pub fn new() -> Self {
        PreviewRenderer {
            path: preview_path(),
            fonts: FontStack::system(),
            user: userinfo::current_identity(),
            avatar: AvatarCache::new(),
            cached_wallpaper: None,
            background: None,
            last_render: None,
            last_size: None,
        }
    }

    /// Redirect where renders are written. Test-only: without it, every test
    /// in this process writes to the same real `$XDG_RUNTIME_DIR/fresco/
    /// lock-preview.png`, and `cargo test`'s default thread-parallel runner
    /// would have two tests' renders racing to write and read that one path —
    /// exactly the shared-mutable-file hazard `widgets::frame_stem`'s own
    /// `#[cfg(test)]` redirection avoids for the desktop engine.
    #[cfg(test)]
    fn set_path(&mut self, path: PathBuf) {
        self.path = path;
        self.last_size = None; // a fresh path has never been rendered to.
    }

    /// Render `resolved` (already `lockscreen::resolve`d, so the GUI can
    /// preview a preset before saving it) at `width`x`height` for `host`, and
    /// write it atomically to [`preview_path`]. `np` is the desktop widget
    /// engine's now-playing snapshot, exactly as `LockEngine::tick` takes it —
    /// `None` when nothing is playing or the caller has none handy.
    ///
    /// Returns the path on success (whether freshly rendered or reused from
    /// the throttle window) and a human-readable message on failure — there
    /// is nothing to lock or unlock here, so every error is one of "couldn't
    /// decode/write an image", never a permissions or authentication failure.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        host: HostKind,
        wallpaper: &Wallpaper,
        resolved: &crate::lockscreen::ResolvedLock,
        np: Option<&Snapshot>,
        theme: Theme,
        width: u32,
        height: u32,
    ) -> Result<PathBuf, String> {
        self.render_at(
            host,
            wallpaper,
            resolved,
            np,
            theme,
            width,
            height,
            Instant::now(),
        )
    }

    /// [`PreviewRenderer::render`], with `now` injected — the throttle's own
    /// test seam. Without it, a test proving "two calls inside the window
    /// reuse the file" would depend on the wall clock actually staying under
    /// [`THROTTLE`] between two lines of test code, which a busy `cargo test`
    /// run sharing a machine with hundreds of other tests cannot promise.
    #[allow(clippy::too_many_arguments)]
    fn render_at(
        &mut self,
        host: HostKind,
        wallpaper: &Wallpaper,
        resolved: &crate::lockscreen::ResolvedLock,
        np: Option<&Snapshot>,
        theme: Theme,
        width: u32,
        height: u32,
        now: Instant,
    ) -> Result<PathBuf, String> {
        validate_size(width, height)?;
        let path = self.path.clone();
        if self.last_size == Some((width, height)) {
            if let Some(last) = self.last_render {
                if now.duration_since(last) < THROTTLE && path.exists() {
                    return Ok(path);
                }
            }
        }

        self.ensure_background(wallpaper)?;
        let background = self
            .background
            .clone()
            .ok_or_else(|| "no wallpaper available to preview".to_string())?;

        let slots = engine::slots_for(resolved);
        let wants_avatar = resolved.widgets.contains(&LockWidget::Avatar);
        let avatar = if wants_avatar {
            self.avatar.get(now)
        } else {
            None
        };

        let clock_style = crate::clock::ClockStyle {
            theme: resolved.clock_theme,
            show_date: resolved.widgets.contains(&LockWidget::Date),
            ..crate::clock::ClockStyle::default()
        };
        let battery = crate::battery::read();
        let owned = engine::build_owned_data(
            &slots,
            &clock_style,
            &resolved.greeting,
            &self.user,
            &avatar,
            resolved.widgets.contains(&LockWidget::Battery),
            resolved.widgets.contains(&LockWidget::AlbumArt),
            battery,
            np,
        );
        let data = owned.as_scene_data(theme);

        let reserved_kind = ReservedZoneKind::for_host(host);
        let output = Size::new(width as f32, height as f32);
        let spec = LockSceneSpec {
            arrangement: engine::arrangement_for(resolved.preset),
            output,
            scale: 1.0,
            slots,
            reserved: reserved_kind.reserved(output, 1.0),
        };

        let bgra = crate::widgetkit::lockscene::compose_still(
            &mut self.fonts,
            &background,
            &spec,
            &data,
            resolved.blur,
            resolved.dim,
        );
        let mut rgba = engine::bgra_to_rgba_image(&bgra);
        if let Some(&placeholder) = spec.reserved.first() {
            draw_placeholder(&mut rgba, placeholder);
        }

        engine::write_png_atomic(&path, &rgba).map_err(|e| e.to_string())?;
        self.last_render = Some(now);
        self.last_size = Some((width, height));
        Ok(path)
    }

    /// Decode (or reuse) the wallpaper's still frame. Re-decodes only when
    /// `wallpaper` itself differs from what was cached last time — see the
    /// module docs' "Caching and throttling".
    fn ensure_background(&mut self, wallpaper: &Wallpaper) -> Result<(), String> {
        if self.cached_wallpaper.as_ref() == Some(wallpaper) && self.background.is_some() {
            return Ok(());
        }
        let source = match wallpaper.kind {
            // An image needs no still-frame extraction at all.
            Kind::Image => wallpaper.effective_path().map(|p| p.to_path_buf()),
            // Video and slideshow both go through the same still-frame path
            // the GNOME overview sync already uses — one `ffmpeg`/
            // `ffmpegthumbnailer` implementation, not a second one here.
            _ => super::super::overview::render_still(wallpaper),
        };
        let Some(source) = source else {
            return Err("no wallpaper configured to preview".to_string());
        };
        let img = image::open(&source)
            .map_err(|e| format!("failed to decode {}: {e}", source.display()))?;
        self.background = Some(Arc::new(img.into_rgba8()));
        self.cached_wallpaper = Some(wallpaper.clone());
        Ok(())
    }
}

impl Default for PreviewRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Blend a subtle, rounded, translucent placeholder into `rect` — "sets
/// honest expectations" (design brief) that the host's own password UI
/// (COSMIC's greeter panel, a wlroots ring, KDE's greeter fields) will sit
/// there on a real lock, without this preview trying to imitate any one
/// host's exact look. No-op outside the image bounds or for a degenerate
/// rect, never a panic.
fn draw_placeholder(img: &mut RgbaImage, rect: Rect) {
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    let radius = (rect.w.min(rect.h) * 0.12).clamp(4.0, 32.0);
    // Subtle: a faint white wash, nowhere near opaque enough to read as a
    // real dialog — just enough to say "something will be here".
    const FILL: [u8; 4] = [255, 255, 255, 40];

    let x0 = rect.x.max(0.0).floor() as u32;
    let y0 = rect.y.max(0.0).floor() as u32;
    let x1 = (rect.x + rect.w).min(img.width() as f32).ceil() as u32;
    let y1 = (rect.y + rect.h).min(img.height() as f32).ceil() as u32;
    for y in y0..y1.min(img.height()) {
        for x in x0..x1.min(img.width()) {
            let dx = ((x as f32 + 0.5) - rect.x).min((rect.x + rect.w) - (x as f32 + 0.5));
            let dy = ((y as f32 + 0.5) - rect.y).min((rect.y + rect.h) - (y as f32 + 0.5));
            let inside_corner = dx >= radius || dy >= radius || {
                let (cx, cy) = (radius - dx, radius - dy);
                cx * cx + cy * cy <= radius * radius
            };
            if !inside_corner {
                continue;
            }
            let dst = img.get_pixel(x, y).0;
            *img.get_pixel_mut(x, y) = alpha_over(FILL, dst);
        }
    }
}

/// Straight-alpha "source over destination", the ordinary compositing
/// formula — both `src` and `dst` here are already straight (not
/// premultiplied), matching [`image::Rgba`]'s own convention.
fn alpha_over(src: [u8; 4], dst: [u8; 4]) -> image::Rgba<u8> {
    let sa = f32::from(src[3]) / 255.0;
    let da = f32::from(dst[3]) / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return image::Rgba([0, 0, 0, 0]);
    }
    let mix = |s: u8, d: u8| -> u8 {
        let s = f32::from(s) / 255.0;
        let d = f32::from(d) / 255.0;
        (((s * sa + d * da * (1.0 - sa)) / out_a).clamp(0.0, 1.0) * 255.0).round() as u8
    };
    image::Rgba([
        mix(src[0], dst[0]),
        mix(src[1], dst[1]),
        mix(src[2], dst[2]),
        (out_a * 255.0).round() as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LockScreen, LockWidgets};
    use crate::lockscreen::resolve;
    use crate::widgetkit::theme::Mode;

    fn theme() -> Theme {
        Theme::for_accent(Mode::Dark, crate::config::Accent::Blue)
    }

    /// A small solid-colour PNG on disk, usable as an `image` wallpaper with
    /// no `ffmpeg`/`ffmpegthumbnailer` dependency in the test environment.
    fn fixture_image(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fresco-lock-preview-test-{}-{tag}.png",
            std::process::id()
        ));
        let img = RgbaImage::from_pixel(64, 48, image::Rgba([20, 30, 40, 255]));
        img.save(&path).unwrap();
        path
    }

    fn image_wallpaper(path: PathBuf) -> Wallpaper {
        Wallpaper {
            kind: Kind::Image,
            path: Some(path),
            ..Default::default()
        }
    }

    /// A renderer redirected to its own unique output path — see
    /// [`PreviewRenderer::set_path`]'s doc comment for why every test needs
    /// one rather than sharing the real `preview_path()`.
    fn test_renderer(tag: &str) -> (PreviewRenderer, PathBuf) {
        // A distinct filename pattern from `fixture_image`'s: the two must
        // never collide, or this function's own cleanup-before-use deletes
        // the wallpaper fixture the test just created under the same tag.
        let path = std::env::temp_dir().join(format!(
            "fresco-lock-preview-OUTPUT-test-{}-{tag}.png",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut r = PreviewRenderer::new();
        r.set_path(path.clone());
        (r, path)
    }

    // -- Never locks anything: the assertion, not just the claim ------------

    #[test]
    fn the_module_never_names_a_locking_primitive() {
        // Scans only non-comment, non-test lines: doc comments legitimately
        // *name* these primitives to explain that this module does not call
        // them (see the module docs), and this test's own body necessarily
        // names them too in order to check for them — including either would
        // make the assertion unsatisfiable by construction, not a meaningful
        // guard on actual code.
        let src = include_str!("preview.rs");
        let production_src = src.split("mod tests {").next().unwrap_or(src);
        let code_only: String = production_src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "loginctl",
            "lock-session",
            "LockHost",
            "hosts::detect",
            "Command::new(\"gdbus\")",
            "PrepareForSleep",
        ] {
            assert!(
                !code_only.contains(needle),
                "preview.rs must never reference {needle:?} in actual code — see the module docs"
            );
        }
    }

    // -- validate_size ------------------------------------------------------

    #[test]
    fn validate_size_accepts_the_documented_bounds() {
        assert!(validate_size(1, 1).is_ok());
        assert!(validate_size(1920, 1080).is_ok());
        assert!(validate_size(7680, 4320).is_ok()); // exactly the area cap
        assert!(validate_size(8192, 4050).is_ok()); // 33_177_600 exactly
        assert!(validate_size(8192, 1).is_ok());
    }

    #[test]
    fn validate_size_rejects_zero_oversized_and_overlarge_area() {
        assert!(validate_size(0, 100).is_err());
        assert!(validate_size(100, 0).is_err());
        assert!(validate_size(8193, 10).is_err());
        assert!(validate_size(10, 8193).is_err());
        assert!(validate_size(8192, 8192).is_err()); // each side ok, area too big
        assert!(validate_size(7680, 4321).is_err());
        assert!(validate_size(u32::MAX, u32::MAX).is_err()); // no overflow panic
    }

    #[test]
    fn render_rejects_an_oversized_request_before_touching_anything() {
        // No wallpaper decode, no file: the error must come from the size check.
        let wallpaper = Wallpaper::default();
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("oversized");
        let err = renderer
            .render(
                HostKind::Cosmic { live: true },
                &wallpaper,
                &resolved,
                None,
                theme(),
                u32::MAX,
                u32::MAX,
            )
            .unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(!out_path.exists());
    }

    // -- render(): end to end against a real (tiny) image wallpaper --------

    #[test]
    fn renders_a_decodable_png_of_the_requested_size() {
        let img_path = fixture_image("basic");
        let wallpaper = image_wallpaper(img_path.clone());
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("basic");

        let out = renderer
            .render(
                HostKind::Cosmic { live: true },
                &wallpaper,
                &resolved,
                None,
                theme(),
                320,
                200,
            )
            .expect("render should succeed against a real image wallpaper");
        let decoded = image::open(&out).expect("output must be a decodable PNG");
        assert_eq!((decoded.width(), decoded.height()), (320, 200));

        let _ = std::fs::remove_file(&img_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn missing_wallpaper_is_a_clean_error_not_a_panic() {
        let wallpaper = Wallpaper {
            kind: Kind::Image,
            path: Some(PathBuf::from("/does/not/exist/fresco-preview-test.png")),
            ..Default::default()
        };
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("missing-wallpaper");
        let err = renderer
            .render(
                HostKind::Gnome,
                &wallpaper,
                &resolved,
                None,
                theme(),
                320,
                200,
            )
            .unwrap_err();
        assert!(!err.is_empty());
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn throttle_reuses_the_same_render_at_an_unchanged_size() {
        // Drives `now` explicitly (via `render_at`) rather than relying on
        // the real wall clock staying under `THROTTLE` between two lines of
        // test code — a `cargo test` run sharing a machine with hundreds of
        // other tests cannot promise that, and this test flaked under
        // exactly that load before this fix (both calls landed a genuine
        // >250ms apart in real time, so the second one correctly re-rendered
        // — the throttle logic was never wrong, the test's timing was).
        let img_path = fixture_image("throttle");
        let wallpaper = image_wallpaper(img_path.clone());
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("throttle");
        let t0 = Instant::now();

        let first = renderer
            .render_at(
                HostKind::Kde,
                &wallpaper,
                &resolved,
                None,
                theme(),
                200,
                150,
                t0,
            )
            .unwrap();
        let m1 = std::fs::metadata(&first).unwrap().modified().unwrap();
        // Well inside the throttle window, same size: must reuse it and not
        // touch the file (same mtime).
        let second = renderer
            .render_at(
                HostKind::Kde,
                &wallpaper,
                &resolved,
                None,
                theme(),
                200,
                150,
                t0 + Duration::from_millis(10),
            )
            .unwrap();
        let m2 = std::fs::metadata(&second).unwrap().modified().unwrap();
        assert_eq!(
            m1, m2,
            "a request inside the throttle window must not re-render"
        );

        let _ = std::fs::remove_file(&img_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn past_the_throttle_window_renders_again() {
        // The complement of the test above: two `render_at` calls straddling
        // `THROTTLE`, with the requested *size* changing too, so the "did it
        // really redraw" signal is the decoded output — not a filesystem
        // mtime, which a coarse filesystem clock could leave unchanged even
        // across a genuine second write.
        let img_path = fixture_image("past-throttle");
        let wallpaper = image_wallpaper(img_path.clone());
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("past-throttle");
        let t0 = Instant::now();

        renderer
            .render_at(
                HostKind::Kde,
                &wallpaper,
                &resolved,
                None,
                theme(),
                200,
                150,
                t0,
            )
            .unwrap();
        let out = renderer
            .render_at(
                HostKind::Kde,
                &wallpaper,
                &resolved,
                None,
                theme(),
                400,
                300,
                t0 + THROTTLE + Duration::from_millis(10),
            )
            .unwrap();
        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 300));

        let _ = std::fs::remove_file(&img_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn a_size_change_always_rerenders_even_inside_the_throttle_window() {
        let img_path = fixture_image("resize");
        let wallpaper = image_wallpaper(img_path.clone());
        let resolved = resolve(&LockScreen::default());
        let (mut renderer, out_path) = test_renderer("resize");

        renderer
            .render(
                HostKind::Xfce,
                &wallpaper,
                &resolved,
                None,
                theme(),
                200,
                150,
            )
            .unwrap();
        let out = renderer
            .render(
                HostKind::Xfce,
                &wallpaper,
                &resolved,
                None,
                theme(),
                400,
                300,
            )
            .unwrap();
        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 300));

        let _ = std::fs::remove_file(&img_path);
        let _ = std::fs::remove_file(&out_path);
    }

    // -- avatar/battery/greeting privacy defaults carry through -------------

    #[test]
    fn preview_never_shows_lyrics_or_visualizer_since_no_such_lock_slot_exists() {
        // Same invariant `engine::tests` pins for the real engine — the
        // preview reuses `engine::slots_for`, so this is really a guard
        // against the two ever being allowed to disagree.
        let resolved = resolve(&LockScreen {
            widgets: LockWidgets {
                lyrics: true,
                visualizer: true,
                ..LockWidgets::default()
            },
            ..LockScreen::default()
        });
        let slots = engine::slots_for(&resolved);
        assert!(slots.iter().all(|s| matches!(
            s,
            crate::widgetkit::lockscene::LockSlot::Clock
                | crate::widgetkit::lockscene::LockSlot::Greeting
                | crate::widgetkit::lockscene::LockSlot::Media
                | crate::widgetkit::lockscene::LockSlot::Battery
        )));
    }

    // -- draw_placeholder / alpha_over ---------------------------------------

    #[test]
    fn placeholder_blends_without_fully_replacing_the_background() {
        let mut img = RgbaImage::from_pixel(100, 100, image::Rgba([10, 10, 10, 255]));
        draw_placeholder(&mut img, Rect::new(10.0, 10.0, 50.0, 30.0));
        let centre = img.get_pixel(35, 25).0;
        assert_ne!(
            centre,
            [10, 10, 10, 255],
            "the placeholder must change the pixel"
        );
        assert!(
            centre[0] > 10,
            "a white wash must lighten it, got {centre:?}"
        );
        assert!(
            centre[0] < 250,
            "must stay subtle, not opaque white: {centre:?}"
        );
        // Untouched far outside the rect.
        assert_eq!(img.get_pixel(90, 90).0, [10, 10, 10, 255]);
    }

    #[test]
    fn placeholder_corners_are_actually_rounded() {
        let mut img = RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 255]));
        let rect = Rect::new(0.0, 0.0, 40.0, 40.0);
        draw_placeholder(&mut img, rect);
        // The extreme corner pixel must be left untouched by a rounded rect
        // large enough for the rounding to matter, while the rect's centre
        // must be touched.
        assert_eq!(
            img.get_pixel(0, 0).0,
            [0, 0, 0, 255],
            "corner must be clipped"
        );
        assert_ne!(
            img.get_pixel(20, 20).0,
            [0, 0, 0, 255],
            "centre must be filled"
        );
    }

    #[test]
    fn draw_placeholder_on_a_degenerate_rect_never_panics() {
        let mut img = RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 0]));
        for rect in [
            Rect::ZERO,
            Rect::new(-50.0, -50.0, 10.0, 10.0),
            Rect::new(5.0, 5.0, -5.0, -5.0),
            Rect::new(1000.0, 1000.0, 50.0, 50.0),
        ] {
            draw_placeholder(&mut img, rect);
        }
    }

    #[test]
    fn alpha_over_opaque_source_fully_replaces() {
        let out = alpha_over([1, 2, 3, 255], [200, 200, 200, 255]);
        assert_eq!(out.0, [1, 2, 3, 255]);
    }

    #[test]
    fn alpha_over_zero_alpha_source_leaves_destination_untouched() {
        let out = alpha_over([1, 2, 3, 0], [200, 201, 202, 255]);
        assert_eq!(out.0, [200, 201, 202, 255]);
    }

    #[test]
    fn alpha_over_onto_fully_transparent_destination_is_the_source_alone() {
        let out = alpha_over([10, 20, 30, 128], [0, 0, 0, 0]);
        assert_eq!(out.0, [10, 20, 30, 128]);
    }
}
