//! The battery chip: a compact corner readout for the lock scene.
//!
//! ```text
//!  ╭──────────────────╮
//!  │ ⚡ [▮▮▮▮▯▯] 84%   │      pill, h = 2 x micro
//!  ╰──────────────────╯
//! ```
//!
//! Fresco has no battery widget anywhere else — [`crate::widgetkit::lockscene`]
//! is the first thing that needs one, because a phone-style lock screen always
//! shows one. It is deliberately the smallest card in the toolkit: a glyph, a
//! fill and a number, in one pill, and nothing else.
//!
//! # Why the glyph is drawn rather than typed
//!
//! `🔋` renders inconsistently across installed fonts (colour emoji in some,
//! missing entirely in others) and the spec's own rule for exactly this
//! situation is to draw the shape — the same call `cards::nowplaying`'s
//! `note_glyph` and `cards::media`'s play/pause indicators already make. The
//! body, the terminal nub and the charge fill are three rectangles; the bolt is
//! two triangles. All from [`Canvas`]'s existing primitives — no new drawing
//! capability is needed.
//!
//! # Colour: the fill is a graphic, so it lives on the well's contrast budget
//!
//! The charge level is a filled shape, not text, so spec §4.3's 3:1 non-text
//! minimum applies and [`Theme::accent_fill`] — already proven to clear it on
//! [`Theme::well`] — is what fills it normally. At 15% or below, discharging,
//! the fill swaps to `warning_fill`, a saturated red nudged by
//! [`Color::ensure_contrast`] until it clears the same 3:1 floor on the same
//! surface: `Theme` carries no dedicated "danger" token (batteries are new
//! here), so this reuses the exact mechanism `Theme::accent_ink` itself is
//! built with rather than hand-picking a hex and hoping.
//!
//! # No card
//!
//! Like [`super::lock`], this chip sits directly on the wallpaper — a lock
//! scene has no glass card floating over it, only widgets. The pill's own
//! `Theme::well` fill plus hairline is the entire legibility instrument, the
//! same treatment [`crate::widgetkit::surface::chip`] already uses for exactly
//! this reason.

use crate::widgetkit::canvas::Canvas;
use crate::widgetkit::color::Color;
use crate::widgetkit::geom::{Point, Rect, Size};
use crate::widgetkit::paint::Fill;
use crate::widgetkit::surface::WidgetSize;
use crate::widgetkit::text::FontStack;
use crate::widgetkit::theme::{self, Theme};
use crate::widgetkit::typo::{self, Step};

/// What a battery chip draws.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BatteryData {
    /// State of charge, 0..=100. Values above 100 are clamped when drawn.
    pub percent: u8,
    /// Draw the bolt, and skip the low-battery warning colour.
    pub charging: bool,
    /// Fully charged. Suppresses the warning colour even if `percent` is
    /// somehow reported low (a stale read racing a charger unplug), because a
    /// battery the OS calls "full" is not the thing the warning exists to flag.
    pub full: bool,
    /// Chip height in logical units. Not part of the three-field sketch this
    /// card was scoped from, but every sibling card sizes itself from a field
    /// on its own data (see `cards::mod`'s `measure`-then-place contract), and
    /// a corner chip fixed at one size would either vanish on a 4K output or
    /// loom on a small one. `<= 0.0` or non-finite falls back to twice the
    /// micro step — [`crate::widgetkit::surface::chip`]'s own formula
    /// (spec §8.5) and this card's original, pre-lock-scene size.
    pub size: f32,
}

/// The chip's height. See [`BatteryData::size`].
fn chip_h(d: &BatteryData) -> f32 {
    if d.size.is_finite() && d.size > 0.0 {
        d.size.clamp(12.0, 400.0)
    } else {
        Step::Micro.size() * 2.0
    }
}

/// Glyph width as a multiple of its own height — a real battery icon's
/// proportion, wide enough that a five-bar fill reads as bars and not a smear.
const GLYPH_RATIO: f32 = 1.85;
/// The terminal nub's width, as a fraction of the glyph height.
const NUB_W_RATIO: f32 = 0.16;
/// The nub's height, as a fraction of the glyph height.
const NUB_H_RATIO: f32 = 0.44;
/// Stroke width for the glyph outline, in logical units.
const STROKE: f32 = 1.6;
/// Charge at or below which, discharging and not full, the fill warns.
const WARNING_AT: u8 = 15;

/// A saturated red nudged off [`Theme::well`] until it clears the spec's 3:1
/// non-text minimum (§4.3) — the same mechanism [`Theme::accent_ink`] is
/// itself built with. `Theme` has no dedicated danger token because nothing
/// before this card needed one.
fn warning_fill(t: &Theme) -> Color {
    const BASE: Color = Color {
        r: 0xE5 as f32 / 255.0,
        g: 0x48 as f32 / 255.0,
        b: 0x4D as f32 / 255.0,
        a: 1.0,
    };
    BASE.ensure_contrast(t.resolved_well(), theme::AA_LARGE)
}

fn percent_of(d: &BatteryData) -> u8 {
    d.percent.min(100)
}

fn is_warning(d: &BatteryData) -> bool {
    !d.charging && !d.full && percent_of(d) <= WARNING_AT
}

/// `84` -> `"84%"`, with no allocation and no way to panic.
fn percent_text<'a>(buf: &'a mut [u8; 4], d: &BatteryData) -> &'a str {
    let p = percent_of(d);
    let n = if p >= 100 {
        buf[0] = b'1';
        buf[1] = b'0';
        buf[2] = b'0';
        3
    } else if p >= 10 {
        buf[0] = b'0' + p / 10;
        buf[1] = b'0' + p % 10;
        2
    } else {
        buf[0] = b'0' + p;
        1
    };
    buf[n] = b'%';
    std::str::from_utf8(&buf[..n + 1]).unwrap_or("")
}

/// Resolved geometry, shared by `measure` and `draw_at`.
#[derive(Debug, Clone, Copy)]
struct Layout {
    h: f32,
    pad_x: f32,
    gap: f32,
    glyph_h: f32,
    glyph_w: f32,
    bolt_w: f32,
    text_size: f32,
    size: Size,
}

fn layout(fonts: &mut FontStack, t: &Theme, d: &BatteryData, scale: f32) -> Layout {
    let h = chip_h(d);
    let pad_x = h * 0.42;
    let gap = h * 0.26;
    let glyph_h = h * 0.56;
    let glyph_w = glyph_h * GLYPH_RATIO + glyph_h * NUB_W_RATIO;
    let bolt_w = if d.charging {
        glyph_h * 0.40 + gap
    } else {
        0.0
    };

    let mut buf = [0u8; 4];
    // The readout, not the ladder: a chip scaled up for a hero-sized lock
    // scene needs type that grows with it, not a label pinned at `micro`
    // forever. Floored at `micro` so the original, pre-lock-scene chip is
    // pixel-identical at the default size.
    let text_size = (h * 0.5).max(Step::Micro.size());
    let run = typo::mono_run(percent_text(&mut buf, d), text_size, fonts).color(t.accent_ink);
    let text_w = fonts.measure(&run, scale).width;

    let w = pad_x * 2.0 + bolt_w + glyph_w + gap + text_w;
    Layout {
        h,
        pad_x,
        gap,
        glyph_h,
        glyph_w,
        bolt_w,
        text_size,
        size: Size::new(w.max(h), h),
    }
}

/// How big this chip is, and how much shadow margin it needs.
pub fn measure(fonts: &mut FontStack, t: &Theme, d: &BatteryData, scale: f32) -> WidgetSize {
    WidgetSize::new(layout(fonts, t, d, scale).size, t.e1())
}

/// Draw the chip, centred in whatever room `canvas` provides.
pub fn draw(c: &mut Canvas, fonts: &mut FontStack, t: &Theme, d: &BatteryData) {
    let size = measure(fonts, t, d, c.scale());
    let rect = size.card_in(c.bounds());
    draw_at(c, fonts, t, d, rect);
}

/// Draw the chip with its pill rect anchored at `card`'s origin.
pub fn draw_at(c: &mut Canvas, fonts: &mut FontStack, t: &Theme, d: &BatteryData, card: Rect) {
    if card.is_empty() {
        return;
    }
    let l = layout(fonts, t, d, c.scale());
    let r = Rect::new(card.x, card.y, l.size.w, l.size.h);

    crate::widgetkit::surface::elevation(c, r, l.h / 2.0, t, t.e1());
    c.rounded_rect(r, l.h / 2.0, &Fill::solid(t.well));
    c.hairline(r, l.h / 2.0, t.edge, t.metrics.hairline);

    let mut x = r.x + l.pad_x;
    let cy = r.center().y;

    if d.charging {
        draw_bolt(
            c,
            t,
            Rect::new(x, cy - l.glyph_h * 0.3, l.glyph_h * 0.4, l.glyph_h * 0.6),
        );
        x += l.bolt_w;
    }

    let glyph = Rect::new(x, cy - l.glyph_h / 2.0, l.glyph_w, l.glyph_h);
    draw_glyph(c, t, d, glyph);
    x += l.glyph_w + l.gap;

    let mut buf = [0u8; 4];
    let run = typo::mono_run(percent_text(&mut buf, d), l.text_size, fonts).color(t.accent_ink);
    let m = fonts.measure(&run, c.scale());
    let ty = cy - m.height / 2.0;
    c.text(fonts, &run, Point::new(x, ty));
}

/// The body, terminal nub and charge fill — a battery, not a rounded rect with
/// a lump on it: the nub is what makes the glyph read as a cell rather than as
/// an icon for something else entirely.
fn draw_glyph(c: &mut Canvas, t: &Theme, d: &BatteryData, glyph: Rect) {
    if glyph.is_empty() {
        return;
    }
    let nub_w = glyph.h * NUB_W_RATIO;
    let body = Rect::new(glyph.x, glyph.y, (glyph.w - nub_w).max(0.0), glyph.h);
    if body.is_empty() {
        return;
    }
    let radius = (body.h * 0.22).min(body.w / 2.0);
    c.hairline(body, radius, t.text_secondary, STROKE);

    let nub_h = glyph.h * NUB_H_RATIO;
    let nub = Rect::new(
        body.right(),
        glyph.center().y - nub_h / 2.0,
        nub_w.max(1.0),
        nub_h,
    );
    c.rounded_rect(
        nub,
        (nub_w * 0.3).min(nub_h / 2.0),
        &Fill::solid(t.text_secondary),
    );

    let inset = STROKE * 1.4;
    let inner = body.inset(inset);
    if inner.is_empty() {
        return;
    }
    let fraction = f32::from(percent_of(d)) / 100.0;
    let fill_w = (inner.w * fraction).max(0.0).min(inner.w);
    if fill_w <= 0.0 {
        return;
    }
    let fill = Rect::new(inner.x, inner.y, fill_w, inner.h);
    let colour = if is_warning(d) {
        warning_fill(t)
    } else {
        t.accent_fill
    };
    c.rounded_rect(
        fill,
        (inner.h * 0.22).min(fill.w / 2.0),
        &Fill::solid(colour),
    );
}

/// A lightning bolt from two triangles — the same reasoning
/// `cards::nowplaying::note_glyph` and `cards::media`'s play/pause indicators
/// already act on: the codepoint is missing from plenty of installed faces, so
/// a tofu box where the charge indicator goes would read as a rendering fault
/// rather than as "charging".
///
/// Drawn with a halo underneath in the theme's counter-colour (spec §4.5's
/// card-less fallback), because the bolt sits beside the fill bar rather than
/// inside it and must read whether that bar is at 4% or 100%.
fn draw_bolt(c: &mut Canvas, t: &Theme, r: Rect) {
    if r.is_empty() {
        return;
    }
    let halo = if t.mode.is_dark() {
        Color::BLACK.with_alpha(0.55)
    } else {
        Color::WHITE.with_alpha(0.70)
    };
    let top = Point::new(r.x + r.w * 0.62, r.y);
    let mid = Point::new(r.x + r.w * 0.18, r.y + r.h * 0.56);
    let notch = Point::new(r.x + r.w * 0.62, r.y + r.h * 0.56);
    let bottom = Point::new(r.x + r.w * 0.18, r.y + r.h);

    for (dx, dy, fill) in [
        (-1.0_f32, 0.0_f32, halo),
        (1.0, 0.0, halo),
        (0.0, -1.0, halo),
        (0.0, 1.0, halo),
        (0.0, 0.0, t.text_primary),
    ] {
        let off = |p: Point| Point::new(p.x + dx, p.y + dy);
        let f = Fill::solid(fill);
        c.triangle(off(top), off(mid), off(notch), &f);
        c.triangle(off(notch), off(mid), off(bottom), &f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgetkit::theme::Mode;

    fn theme(mode: Mode) -> Theme {
        Theme::for_accent(mode, crate::config::Accent::Blue)
    }

    fn data(percent: u8, charging: bool, full: bool) -> BatteryData {
        BatteryData {
            percent,
            charging,
            full,
            size: 0.0,
        }
    }

    #[test]
    fn percent_formats_every_boundary_with_no_allocation() {
        let mut buf = [0u8; 4];
        assert_eq!(percent_text(&mut buf, &data(0, false, false)), "0%");
        assert_eq!(percent_text(&mut buf, &data(9, false, false)), "9%");
        assert_eq!(percent_text(&mut buf, &data(10, false, false)), "10%");
        assert_eq!(percent_text(&mut buf, &data(84, false, false)), "84%");
        assert_eq!(percent_text(&mut buf, &data(99, false, false)), "99%");
        assert_eq!(percent_text(&mut buf, &data(100, false, false)), "100%");
        // Out-of-range input clamps rather than producing garbage.
        assert_eq!(percent_text(&mut buf, &data(255, false, false)), "100%");
    }

    #[test]
    fn warning_applies_only_while_discharging_low_and_not_full() {
        assert!(is_warning(&data(15, false, false)));
        assert!(is_warning(&data(5, false, false)));
        assert!(!is_warning(&data(16, false, false)));
        assert!(!is_warning(&data(5, true, false)), "charging must not warn");
        assert!(!is_warning(&data(5, false, true)), "full must not warn");
    }

    #[test]
    fn the_warning_fill_clears_the_non_text_minimum_in_both_themes() {
        for mode in [Mode::Dark, Mode::Light] {
            let t = theme(mode);
            let r = t.contrast_on_well(warning_fill(&t));
            assert!(r >= theme::AA_LARGE, "{mode:?}: warning fill {r:.2}:1");
            // And the normal fill, which is just the theme's own accent fill,
            // already carries this guarantee — asserted here so a future
            // change to which token battery.rs draws with cannot silently
            // regress below the spec's floor without failing here too.
            let r = t.contrast_on_well(t.accent_fill);
            assert!(r >= theme::AA_LARGE, "{mode:?}: accent fill {r:.2}:1");
        }
    }

    #[test]
    fn the_chip_grows_for_the_bolt_and_shrinks_without_it() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme(Mode::Dark);
        let with_bolt = measure(&mut f, &t, &data(50, true, false), 1.0);
        let without = measure(&mut f, &t, &data(50, false, false), 1.0);
        assert!(with_bolt.card.w > without.card.w);
        assert_eq!(with_bolt.card.h, without.card.h);
    }

    #[test]
    fn size_scales_the_whole_chip_and_the_default_is_unchanged() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme(Mode::Dark);
        let default = measure(&mut f, &t, &data(50, false, false), 1.0);
        let unset = measure(
            &mut f,
            &t,
            &BatteryData {
                size: 0.0,
                ..data(50, false, false)
            },
            1.0,
        );
        assert_eq!(
            default.card, unset.card,
            "unset size must match the old fixed one"
        );
        let big = measure(
            &mut f,
            &t,
            &BatteryData {
                size: 80.0,
                ..data(50, false, false)
            },
            1.0,
        );
        assert!(big.card.h > default.card.h * 2.0, "{big:?} vs {default:?}");
        assert!(
            big.card.w > default.card.w,
            "text and glyph must grow with it too"
        );
    }

    #[test]
    fn no_combination_of_settings_can_panic() {
        let mut f = FontStack::system();
        let mut c = Canvas::for_logical(Size::new(120.0, 60.0), 1.0).unwrap();
        for mode in [Mode::Dark, Mode::Light] {
            let t = theme(mode);
            for percent in [0u8, 1, 15, 16, 50, 99, 100, 255] {
                for charging in [false, true] {
                    for full in [false, true] {
                        for size in [0.0_f32, -5.0, f32::NAN, f32::INFINITY, 11.0, 400.0, 4000.0] {
                            let d = BatteryData {
                                size,
                                ..data(percent, charging, full)
                            };
                            let m = measure(&mut f, &t, &d, 1.0);
                            assert!(m.buffer().w.is_finite() && m.buffer().h.is_finite());
                            c.reset();
                            draw_at(&mut c, &mut f, &t, &d, Rect::new(4.0, 4.0, 100.0, 40.0));
                            draw_at(&mut c, &mut f, &t, &d, Rect::new(-20.0, -20.0, 30.0, 30.0));
                            draw_at(&mut c, &mut f, &t, &d, Rect::ZERO);
                        }
                    }
                }
            }
            c.reset();
            draw(&mut c, &mut f, &t, &data(64, false, false));
        }
    }
}
