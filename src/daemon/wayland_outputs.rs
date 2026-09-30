//! Minimal Wayland output enumeration (static snapshot).
//!
//! Phase 2 needs the current output connector names to spawn one mpvpaper per
//! output. We bind `wl_output` (v4 for the `name` event, e.g. "DP-1") and read
//! its geometry/mode, producing the same neutral [`Monitor`] shape RandR fills
//! on X11 so per-monitor config keys match across backends.
//!
//! # Output scale
//!
//! Each [`Monitor`] also carries a HiDPI `scale_milli`, which only the lock
//! engine consumes (COSMIC's greeter panel is sized in *logical* pixels, so
//! reserving room for it needs the real factor). Sources, best first:
//!
//! 1. `zwlr_output_manager_v1` — the fractional scale of the output's head
//!    (COSMIC implements it), matched to the `wl_output` by connector name;
//! 2. `wl_output.scale` — the integer factor, which on a fractional setup is
//!    the rounded-up value, so it *over*-estimates and the reserved zone only
//!    gets larger (safe for the overlap concern);
//! 3. `1.0`.
//!
//! The head events ride one extra roundtrip, and only when the compositor
//! advertises the manager at all.
//!
//! One-shot snapshot at startup / explicit reconcile. Live hotplug (Phase 3)
//! will hook the same registry globals instead of disconnecting.

use std::collections::HashMap;

use anyhow::{Context, Result};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_head_v1 as head, zwlr_output_manager_v1 as manager, zwlr_output_mode_v1 as mode,
};

use super::monitors::Monitor;

#[derive(Default)]
struct OutputInfo {
    name: Option<String>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    /// `wl_output.scale` (integer; `0` until the event arrives).
    scale: i32,
}

/// What a `zwlr_output_head_v1` has told us so far.
#[derive(Default)]
struct HeadInfo {
    name: Option<String>,
    scale: Option<f64>,
}

#[derive(Default)]
struct State {
    outputs: HashMap<u32, OutputInfo>,
    heads: HashMap<u32, HeadInfo>,
    /// Whether `zwlr_output_manager_v1` was advertised and bound.
    manager_bound: bool,
}

/// Fractional scales in thousandths keyed by head name — the matching table
/// [`resolve_scale_milli`] consults.
fn head_scales(heads: &HashMap<u32, HeadInfo>) -> HashMap<String, f64> {
    heads
        .values()
        .filter_map(|h| Some((h.name.clone()?, h.scale?)))
        .collect()
}

/// The scale (in thousandths) to report for one output: the wlr head of the
/// same `name` if it gave a sane fractional scale, else the `wl_output`
/// integer factor, else 1.0. "Sane" is 0.25..=16 and finite — a compositor
/// bug must not be able to make the lock zone degenerate. Pure, so the
/// precedence is unit-tested.
fn resolve_scale_milli(name: Option<&str>, wlr: &HashMap<String, f64>, int_scale: i32) -> u16 {
    if let Some(s) = name.and_then(|n| wlr.get(n)).copied() {
        if s.is_finite() && (0.25..=16.0).contains(&s) {
            return (s * 1000.0).round() as u16;
        }
    }
    if (1..=16).contains(&int_scale) {
        return (int_scale * 1000) as u16;
    }
    1000
}

/// Whether a [`list_outputs`] error means the compositor itself could not be
/// reached (no socket, or `WAYLAND_DISPLAY` pointing at a dead one) — as
/// opposed to a protocol hiccup mid-roundtrip, which says nothing about the
/// displays.
pub fn is_unreachable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<wayland_client::ConnectError>().is_some()
}

/// Enumerate connected Wayland outputs into the neutral [`Monitor`] set.
pub fn list_outputs() -> Result<Vec<Monitor>> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = State::default();
    // 1st roundtrip: registry globals → bind wl_outputs.
    queue
        .roundtrip(&mut state)
        .context("wayland registry roundtrip")?;
    // 2nd roundtrip: each wl_output's geometry/mode/name events arrive.
    queue
        .roundtrip(&mut state)
        .context("wayland output roundtrip")?;

    // 3rd roundtrip, only when the compositor has the wlr manager: its head
    // events (name + fractional scale) can trail the first two. Failure here
    // is not fatal — the integer scale is an acceptable fallback.
    if state.manager_bound {
        if let Err(e) = queue.roundtrip(&mut state) {
            log::debug!("wayland output scale roundtrip failed: {e}");
        }
    }
    let wlr_scales = head_scales(&state.heads);

    let mut monitors: Vec<Monitor> = state
        .outputs
        .into_iter()
        .filter(|(_, o)| o.width > 0 && o.height > 0)
        .map(|(id, o)| Monitor {
            scale_milli: resolve_scale_milli(o.name.as_deref(), &wlr_scales, o.scale),
            connector: o.name.unwrap_or_else(|| format!("output-{id}")),
            x: o.x as i16,
            y: o.y as i16,
            width: o.width as u16,
            height: o.height as u16,
        })
        .collect();
    monitors.sort_by(|a, b| a.connector.cmp(&b.connector));
    Ok(monitors)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_output" {
                let output =
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                state
                    .outputs
                    .insert(output.id().protocol_id(), OutputInfo::default());
            } else if interface == "zwlr_output_manager_v1" {
                // Version 1 has everything we read (head name + scale).
                registry.bind::<manager::ZwlrOutputManagerV1, _, _>(name, version.min(2), qh, ());
                state.manager_bound = true;
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let info = state.outputs.entry(output.id().protocol_id()).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                info.x = x;
                info.y = y;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Only the current mode defines the output's pixel size.
                let current =
                    matches!(flags, WEnum::Value(m) if m.contains(wl_output::Mode::Current));
                if current || info.width == 0 {
                    info.width = width;
                    info.height = height;
                }
            }
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Scale { factor } => info.scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<manager::ZwlrOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &manager::ZwlrOutputManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        // Heads are registered by `event_created_child` below; `done` needs
        // no action for a one-shot snapshot.
    }

    event_created_child!(State, manager::ZwlrOutputManagerV1, [
        manager::EVT_HEAD_OPCODE => (head::ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<head::ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        hd: &head::ZwlrOutputHeadV1,
        event: head::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let info = state.heads.entry(hd.id().protocol_id()).or_default();
        match event {
            head::Event::Name { name } => info.name = Some(name),
            head::Event::Scale { scale } => info.scale = Some(scale),
            _ => {}
        }
    }

    // Each head announces its modes as child objects; they must be given
    // user-data or wayland-client aborts on the unknown child.
    event_created_child!(State, head::ZwlrOutputHeadV1, [
        head::EVT_MODE_OPCODE => (mode::ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<mode::ZwlrOutputModeV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &mode::ZwlrOutputModeV1,
        _: mode::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wlr(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(n, s)| ((*n).to_string(), *s)).collect()
    }

    #[test]
    fn fractional_head_scale_wins_over_the_integer_one() {
        let w = wlr(&[("eDP-1", 1.5)]);
        assert_eq!(resolve_scale_milli(Some("eDP-1"), &w, 2), 1500);
        assert_eq!(
            resolve_scale_milli(Some("eDP-1"), &wlr(&[("eDP-1", 1.25)]), 2),
            1250
        );
    }

    #[test]
    fn unmatched_name_falls_back_to_the_integer_scale() {
        let w = wlr(&[("DP-1", 1.5)]);
        assert_eq!(resolve_scale_milli(Some("eDP-1"), &w, 2), 2000);
        assert_eq!(resolve_scale_milli(None, &w, 2), 2000);
        assert_eq!(resolve_scale_milli(Some("eDP-1"), &HashMap::new(), 1), 1000);
    }

    #[test]
    fn nothing_known_is_one_point_zero() {
        assert_eq!(resolve_scale_milli(None, &HashMap::new(), 0), 1000);
        assert_eq!(resolve_scale_milli(Some("X"), &HashMap::new(), -3), 1000);
    }

    #[test]
    fn insane_head_scales_are_ignored() {
        for bad in [0.0, -1.0, 0.1, 99.0, f64::NAN, f64::INFINITY] {
            let w = wlr(&[("eDP-1", bad)]);
            assert_eq!(resolve_scale_milli(Some("eDP-1"), &w, 2), 2000, "{bad}");
            assert_eq!(resolve_scale_milli(Some("eDP-1"), &w, 0), 1000, "{bad}");
        }
    }

    /// Read-only probe of the live session (`cargo test -- --ignored live_`):
    /// prints each output's resolved scale. Ignored because CI has no compositor.
    #[test]
    #[ignore = "needs a live Wayland session"]
    fn live_outputs_report_a_scale() {
        for m in list_outputs().unwrap() {
            eprintln!(
                "{} {}x{} scale_milli={}",
                m.connector, m.width, m.height, m.scale_milli
            );
            assert!(m.scale_milli >= 250);
        }
    }

    #[test]
    fn head_table_needs_both_name_and_scale() {
        let mut heads = HashMap::new();
        heads.insert(
            1,
            HeadInfo {
                name: Some("A".into()),
                scale: Some(2.0),
            },
        );
        heads.insert(
            2,
            HeadInfo {
                name: Some("B".into()),
                scale: None,
            },
        );
        heads.insert(
            3,
            HeadInfo {
                name: None,
                scale: Some(1.5),
            },
        );
        let t = head_scales(&heads);
        assert_eq!(t.len(), 1);
        assert_eq!(t["A"], 2.0);
    }
}
