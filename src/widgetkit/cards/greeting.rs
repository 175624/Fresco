//! The greeting: a salutation, and the person it belongs to.
//!
//! ```text
//!    ╭───╮
//!    │ R │  Good evening, Roy
//!    ╰───╯
//! ```
//!
//! The lock scene's one purely social widget — every other card on it reports
//! a fact (the time, the track, the charge); this one is the only line that is
//! addressed to somebody. `docs/widget-design-spec.md`'s cards are all reused
//! or adapted from an existing widget; this one has no ancestor, so its
//! treatment borrows the two closest cousins rather than inventing a third:
//! the avatar is the disc of [`crate::widgetkit::surface::badge`] (spec §8.6)
//! at a larger size — well, picture, hairline ring — and the text sits on
//! [`crate::widgetkit::surface::text_scrim`] (spec §2.3), exactly as
//! [`super::clock`]'s micro-label does.
//!
//! # A person without a picture
//!
//! With no image the disc carries the person's **initials**
//! ([`GreetingData::initials`]), sized to the disc. It used to borrow
//! `badge`'s no-icon fallback — the first letter of the *label* — which for a
//! greeting is the first letter of "Good evening": a "G" in a grey circle,
//! which reads as an application's icon, not as the user.
//!
//! # No card, a scrim anyway
//!
//! Like [`super::lock`] and [`super::battery`], this widget has no card body —
//! a lock scene is bare wallpaper with widgets on it, not a stack of glass
//! panels. But unlike the Lock clock (which gets its contrast from a text
//! shadow, spec's card-less fallback for a *single, large, short* string),
//! a greeting can be a full sentence at body size, where a shadow alone is not
//! the sanctioned instrument (spec §4.5 reserves the shadow-only treatment for
//! large display type). So the text still gets a scrim — spec calls this out
//! by name — sized to its own ink via [`ScrimSpec`], clamped to a generous
//! synthetic bound rather than to a drawn card, because there is no card to
//! clamp to.
//!
//! # Why the avatar's layout is a draw-time choice, not a data field
//!
//! "Beside or above the text" is the arrangement's call (a wide Classic row
//! wants beside; a narrow Glass column wants above), and arrangement is
//! something [`crate::widgetkit::lockscene`] knows and this module does not —
//! this module stays pure widgetkit, the same reason every other card here
//! takes no config. [`GreetingLayout`] is a parameter to `measure`/`draw_at`
//! for exactly that reason, the same way [`super::media::MediaLayer`] is a
//! parameter, not baked into the geometry.

use crate::widgetkit::canvas::Canvas;
use crate::widgetkit::geom::{HAlign, Point, Rect, Size, VAlign};
use crate::widgetkit::paint::Fill;
use crate::widgetkit::surface::{self, ScrimSpec, WidgetSize};
use crate::widgetkit::text::FontStack;
use crate::widgetkit::theme::Theme;
use crate::widgetkit::typo::{self, Script};

/// What a greeting draws.
#[derive(Debug, Clone, Copy, Default)]
pub struct GreetingData<'a> {
    /// The salutation, e.g. `"Good evening, Roy"`. Sentence case, not a label —
    /// never transformed.
    pub text: &'a str,
    /// The avatar image. Drawn as a centre-cropped cover, so a non-square
    /// source is cropped, never squashed. `None` falls back to a well-filled
    /// circle carrying [`GreetingData::initials`].
    pub avatar: Option<&'a image::RgbaImage>,
    /// The person's initials (`"RD"`; see `userinfo::initials`), drawn in the
    /// disc when there is no `avatar`. Empty draws a plain disc — never the
    /// greeting's own first letter, which is not a fact about the person.
    pub initials: &'a str,
    /// Text size in logical units. Not part of the two-field sketch this card
    /// was scoped from, but every other card in this toolkit sizes itself from
    /// a field on its own data rather than from the rect it is handed (see
    /// `cards::mod`'s `measure`-then-place contract) — a greeting card with no
    /// size input at all could not implement that contract, so this is the
    /// minimal addition that lets it.
    pub text_size: f32,
}

/// Where the avatar sits relative to the text — an arrangement's decision
/// (spec: "beside or above the text depending on the variant/arrangement"),
/// so it travels as a draw-time parameter rather than a field on the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GreetingLayout {
    /// Avatar to the left, text to its right, both vertically centred.
    #[default]
    Row,
    /// Avatar above, text centred beneath it.
    Column,
}

/// The floor for `text_size`, and what a non-finite or non-positive one falls
/// back to.
const DEFAULT_SIZE: f32 = 20.0;
/// Avatar diameter as a multiple of the text size.
const AVATAR_RATIO: f32 = 1.9;
/// Gap between the avatar and the text, as a multiple of the text size.
const GAP_RATIO: f32 = 0.55;
/// Initials' font size as a multiple of the disc diameter: large enough to
/// read as a monogram, small enough that two capitals clear the ring.
const MONOGRAM_RATIO: f32 = 0.42;

fn text_size(d: &GreetingData) -> f32 {
    if d.text_size.is_finite() && d.text_size > 0.0 {
        d.text_size.clamp(8.0, 200.0)
    } else {
        DEFAULT_SIZE
    }
}

fn text_run(
    d: &GreetingData,
    size: f32,
    t: &Theme,
    fonts: &mut FontStack,
) -> crate::widgetkit::text::TextRun {
    typo::styled(d.text, size, 600, false, fonts).color(t.text_primary)
}

/// The resolved geometry, shared by `measure` and `draw_at`.
#[derive(Debug, Clone, Copy)]
struct Layout {
    size: Size,
    avatar_d: f32,
    /// The avatar's rect, relative to the block's own origin.
    avatar: Rect,
    /// The text's cap-top origin, relative to the block's own origin.
    text_at: Point,
    text_size: f32,
}

fn layout(
    fonts: &mut FontStack,
    t: &Theme,
    d: &GreetingData,
    mode: GreetingLayout,
    scale: f32,
) -> Layout {
    let ts = text_size(d);
    let avatar_d = ts * AVATAR_RATIO;
    let gap = ts * GAP_RATIO;
    let has_text = !d.text.is_empty();
    let script = Script::of(d.text);
    let cap = typo::cap_height(ts, script);
    let desc = typo::descender(ts, script);
    let cap_gap = typo::cap_gap(ts, script);

    let text_w = if has_text {
        let run = text_run(d, ts, t, fonts);
        fonts.measure(&run, scale).width
    } else {
        0.0
    };

    match mode {
        GreetingLayout::Row => {
            let block_h = avatar_d.max(cap + desc);
            let avatar = Rect::new(0.0, (block_h - avatar_d) / 2.0, avatar_d, avatar_d);
            let text_y = (block_h - (cap + desc)) / 2.0;
            let text_x = if avatar_d > 0.0 { avatar_d + gap } else { 0.0 };
            Layout {
                size: Size::new(text_x + text_w, block_h),
                avatar_d,
                avatar,
                text_at: Point::new(text_x, text_y - cap_gap),
                text_size: ts,
            }
        }
        GreetingLayout::Column => {
            let avatar_gap = if avatar_d > 0.0 && has_text { gap } else { 0.0 };
            let overall_w = text_w.max(avatar_d);
            let avatar = Rect::new((overall_w - avatar_d) / 2.0, 0.0, avatar_d, avatar_d);
            let text_y = avatar_d + avatar_gap;
            Layout {
                size: Size::new(overall_w, avatar_d + avatar_gap + cap + desc),
                avatar_d,
                avatar,
                text_at: Point::new((overall_w - text_w) / 2.0, text_y - cap_gap),
                text_size: ts,
            }
        }
    }
}

/// The avatar: [`surface::badge`]'s disc (elevation, well, hairline ring) with
/// the picture — or, without one, the initials — inside.
///
/// The well is filled under a picture as well as behind initials, so a PNG
/// with transparent corners (cut-out avatars are common) shows the disc
/// through them rather than the wallpaper.
fn avatar_disc(
    c: &mut Canvas,
    fonts: &mut FontStack,
    t: &Theme,
    r: Rect,
    image: Option<&image::RgbaImage>,
    initials: &str,
) {
    let d = r.min_side();
    if d <= 0.0 {
        return;
    }
    let sq = r.align(Size::new(d, d), HAlign::Center, VAlign::Middle);
    surface::elevation(c, sq, d / 2.0, t, t.e1());
    c.rounded_rect(sq, d / 2.0, &Fill::solid(t.well));
    match image {
        Some(img) => c.image_cover(img, sq, d / 2.0),
        None => {
            let initials = initials.trim();
            if !initials.is_empty() {
                let run = typo::styled(initials, d * MONOGRAM_RATIO, 600, false, fonts)
                    .color(t.text_primary);
                let m = fonts.measure(&run, c.scale());
                c.text(
                    fonts,
                    &run,
                    sq.align(m.size(), HAlign::Center, VAlign::Middle).origin(),
                );
            }
        }
    }
    c.hairline(sq, d / 2.0, t.edge, t.metrics.hairline);
}

/// How big this greeting is, and how much shadow margin it needs.
pub fn measure(
    fonts: &mut FontStack,
    t: &Theme,
    d: &GreetingData,
    mode: GreetingLayout,
    scale: f32,
) -> WidgetSize {
    let l = layout(fonts, t, d, mode, scale);
    // E1: the avatar carries its own small elevation (as a badge would); the
    // text has none — a scrim, not a shadow, is its legibility instrument.
    WidgetSize::new(l.size, t.e1())
}

/// Draw, centred in whatever room `canvas` provides.
pub fn draw(
    c: &mut Canvas,
    fonts: &mut FontStack,
    t: &Theme,
    d: &GreetingData,
    mode: GreetingLayout,
) {
    let size = measure(fonts, t, d, mode, c.scale());
    let rect = size.card_in(c.bounds());
    draw_at(c, fonts, t, d, mode, rect);
}

/// Draw with the block anchored at `card`'s origin.
pub fn draw_at(
    c: &mut Canvas,
    fonts: &mut FontStack,
    t: &Theme,
    d: &GreetingData,
    mode: GreetingLayout,
    card: Rect,
) {
    if card.is_empty() {
        return;
    }
    let l = layout(fonts, t, d, mode, c.scale());

    if l.avatar_d > 0.0 {
        let avatar = l.avatar.offset(card.x, card.y);
        avatar_disc(c, fonts, t, avatar, d.avatar, d.initials);
    }

    if d.text.is_empty() {
        return;
    }
    let run = text_run(d, l.text_size, t, fonts);
    let text_w = fonts.measure(&run, c.scale()).width;
    let script = Script::of(d.text);
    let block = Rect::new(
        card.x + l.text_at.x,
        card.y + l.text_at.y + typo::cap_gap(l.text_size, script),
        text_w.max(1.0),
        typo::cap_height(l.text_size, script) + typo::descender(l.text_size, script),
    );
    // No drawn card to clamp the scrim to, so the synthetic bound is the
    // block inflated generously — big enough that the feather always finishes
    // before it, never so big that `scrim_rect`'s own §4.4 inflation reads as
    // clipped against it.
    let synthetic_card = block.inset(-l.text_size * 2.0);
    surface::text_scrim(
        c,
        t,
        block,
        ScrimSpec {
            card: synthetic_card,
            radius: (l.text_size * 0.5).clamp(8.0, 24.0),
            pad: 0.0,
            largest: l.text_size,
            script,
        },
    );
    c.text(
        fonts,
        &run,
        Point::new(card.x + l.text_at.x, card.y + l.text_at.y),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgetkit::theme::Mode;

    fn theme(mode: Mode) -> Theme {
        Theme::for_accent(mode, crate::config::Accent::Teal)
    }

    fn data(text: &'static str) -> GreetingData<'static> {
        GreetingData {
            text,
            avatar: None,
            initials: "RD",
            text_size: 20.0,
        }
    }

    #[test]
    fn row_layout_places_the_avatar_left_of_the_text() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme(Mode::Dark);
        let l = layout(
            &mut f,
            &t,
            &data("Good evening, Roy"),
            GreetingLayout::Row,
            1.0,
        );
        assert!(l.text_at.x >= l.avatar.right() - 0.01, "{l:?}");
        assert!(l.size.w > l.avatar_d);
    }

    #[test]
    fn column_layout_places_the_avatar_above_the_text_and_centres_both() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme(Mode::Dark);
        let l = layout(
            &mut f,
            &t,
            &data("Good evening"),
            GreetingLayout::Column,
            1.0,
        );
        assert!(l.text_at.y >= l.avatar.bottom() - 0.01, "{l:?}");
        let avatar_c = l.avatar.center().x;
        assert!((avatar_c - l.size.w / 2.0).abs() < 1.0, "{l:?}");
    }

    #[test]
    fn no_avatar_still_sizes_and_draws_with_no_hole_where_it_would_go() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme(Mode::Dark);
        let with = layout(
            &mut f,
            &t,
            &GreetingData {
                avatar: None,
                ..data("Hi")
            },
            GreetingLayout::Row,
            1.0,
        );
        // Even with no image, the disc still draws a well + initials, so the
        // avatar diameter is still reserved — the fallback is a real object,
        // not a hole.
        assert!(with.avatar_d > 0.0);
    }

    #[test]
    fn text_over_the_scrim_clears_aa_on_both_themes_worst_case() {
        let f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        for mode in [Mode::Dark, Mode::Light] {
            let t = theme(mode);
            let r = t.contrast_on_text(t.text_primary);
            assert!(r >= crate::widgetkit::theme::AA_TEXT, "{mode:?}: {r:.2}:1");
        }
    }

    #[test]
    fn no_combination_of_settings_can_panic() {
        let mut f = FontStack::system();
        let mut c = Canvas::for_logical(Size::new(200.0, 120.0), 1.0).unwrap();
        let avatar = image::RgbaImage::from_pixel(48, 48, image::Rgba([120, 90, 200, 255]));
        let tiny = image::RgbaImage::new(0, 0);
        let texts = [
            "",
            "Good evening, Roy",
            "晚上好,罗伊",
            "🎉",
            &"x".repeat(400),
        ];
        for mode in [Mode::Dark, Mode::Light] {
            let t = theme(mode);
            for layout_mode in [GreetingLayout::Row, GreetingLayout::Column] {
                for size in [f32::NAN, 0.0, -5.0, 1.0, 20.0, 200.0] {
                    for text in texts {
                        for art in [None, Some(&avatar), Some(&tiny)] {
                            let d = GreetingData {
                                text,
                                avatar: art,
                                initials: "RD",
                                text_size: size,
                            };
                            let m = measure(&mut f, &t, &d, layout_mode, 1.0);
                            assert!(m.buffer().w.is_finite() && m.buffer().h.is_finite());
                            c.reset();
                            draw_at(
                                &mut c,
                                &mut f,
                                &t,
                                &d,
                                layout_mode,
                                Rect::new(8.0, 8.0, 150.0, 80.0),
                            );
                            draw_at(
                                &mut c,
                                &mut f,
                                &t,
                                &d,
                                layout_mode,
                                Rect::new(-40.0, -40.0, 60.0, 60.0),
                            );
                            draw_at(&mut c, &mut f, &t, &d, layout_mode, Rect::ZERO);
                        }
                    }
                }
            }
            c.reset();
            draw(
                &mut c,
                &mut f,
                &t,
                &data("Good evening, Roy"),
                GreetingLayout::Row,
            );
        }
    }
}
