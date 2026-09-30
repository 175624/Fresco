#!/usr/bin/env bash
# Build the mpvpaper backend so Fresco can render live wallpapers on Wayland
# layer-shell compositors (COSMIC, Hyprland, Sway, KDE Plasma 6, …).
#
# Usage:
#   scripts/build-mpvpaper.sh          # builds target/release/mpvpaper
#   CARGO_TARGET_DIR=foo scripts/build-mpvpaper.sh
#
# Requires: git, meson, ninja, gcc, pkg-config, libmpv-dev, libwayland-dev,
#           libwayland-egl1-mesa-dev (or equivalent), libegl1-mesa-dev.
set -euo pipefail

# Pinned upstream mpvpaper release. Keep this in sync with install.sh's
# local-rebuild fallback.
#
# 1.9 (not 1.4) because 1.4 renders a *black* wallpaper on the NVIDIA
# proprietary driver: it brings up EGL, reports success and then never presents
# a frame. Upstream's own 1.4 notes admit "some Nvidia GPU users still
# experiencing issues" after the render-loop rewrite; 1.6 shipped the fix ("fix
# support for the Nvidia proprietary drivers", 3 commits) and 1.7 reworked the
# compositor render-loop handshake, again calling out Nvidia. Nothing in Fresco
# can work around it — the bug is entirely inside mpvpaper.
#
# One pin for BOTH release runners (ubuntu-24.04 → libmpv2, ubuntu-22.04 →
# libmpv1). Verified 1.9 still builds on the 22.04 base:
#   * meson.build declares no meson_version and no dependency version floors.
#   * The only 1.9 build change is get_pkgconfig_variable() → get_variable(),
#     which meson has supported since 0.58; 22.04 ships 0.61.
#   * 1.9's extra protocol (wlr-foreign-toplevel-management) is vendored in the
#     repo's proto/, so wayland-protocols 1.25 on 22.04 is still enough (only
#     stable/xdg-shell is pulled from it).
#   * The only libmpv API 1.9 adds over 1.4 is mpv_free() and
#     mpv_render_context_report_swap(), both present in mpv 0.34 (libmpv1).
VERSION="1.9"
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}/release"

# EGL context fallback for older GPUs (see packaging/mpvpaper/ for the full
# rationale). Upstream 1.9 only tries desktop GL core contexts 4.6-3.0 and
# gives up; this adds a compat-profile and GLES 2.0 fallback so mpvpaper
# doesn't exit 1 on hardware that can't do GL 3.0 core. Applying it is not
# optional: a version bump that silently drops it must fail the build, not
# ship a renderer that regresses to the pre-fix black-screen behavior.
PATCH="$ROOT/packaging/mpvpaper/0001-egl-context-gl-compat-and-gles-fallback.patch"

# Opt-in lock-screen wallpaper support (see packaging/mpvpaper/ for the full
# rationale). Adds a MPVPAPER_SHOW_ON_LOCK env var that, when set, binds
# cosmic-comp's cosmic_session_lock_layer_manager_v1 global and asks it to
# keep showing each output's layer surface while the session is locked. Also
# not optional to apply: unlike the EGL patch this is a no-op unless Fresco's
# daemon sets the env var, but skipping the patch would silently break the
# feature for every compositor rather than just failing loudly.
PATCH2="$ROOT/packaging/mpvpaper/0002-cosmic-session-lock-show-on-lock.patch"

command -v meson >/dev/null 2>&1 || { echo "meson is required"; exit 1; }
command -v ninja >/dev/null 2>&1 || { echo "ninja is required"; exit 1; }
command -v wayland-scanner >/dev/null 2>&1 || { echo "wayland-scanner is required"; exit 1; }
[[ -f "$PATCH" ]] || { echo "missing mpvpaper EGL fallback patch: $PATCH"; exit 1; }
[[ -f "$PATCH2" ]] || { echo "missing mpvpaper show-on-lock patch: $PATCH2"; exit 1; }

mkdir -p "$TARGET"
BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT

echo "Building mpvpaper $VERSION into $TARGET ..."
cd "$BUILD_DIR"
git clone --depth 1 --branch "$VERSION" https://github.com/GhostNaN/mpvpaper.git mpvpaper
cd mpvpaper
git apply --verbose "$PATCH" || { echo "mpvpaper EGL fallback patch failed to apply against $VERSION — fix the patch before shipping"; exit 1; }
git apply --verbose "$PATCH2" || { echo "mpvpaper show-on-lock patch failed to apply against $VERSION — fix the patch before shipping"; exit 1; }
meson setup build
meson compile -C build
cp build/mpvpaper "$TARGET/mpvpaper"
echo "Built: $TARGET/mpvpaper"
