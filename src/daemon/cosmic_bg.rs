//! COSMIC lock-screen background sync — the same problem `overview` solves
//! for GNOME/Cinnamon/MATE, adapted to how COSMIC stores its background.
//!
//! On COSMIC, Fresco's patched mpvpaper draws the live wallpaper on
//! `wlr-layer-shell` surfaces ABOVE `cosmic-bg`, the desktop's own background
//! renderer — so on the live desktop itself nothing here is even necessary.
//! The catch is the lock screen: `cosmic-greeter`'s in-session locker does not
//! show whatever is composited on screen, it draws its own surface and reads
//! `cosmic-bg`'s config directly to decide what picture to show
//! (cosmic-greeter PR #528, "locker: Rely on cosmic-bg for showing background
//! by ids"). `cosmic-bg`'s config still points at whatever the user had before
//! Fresco — a stock distro image on a fresh install — so the lock screen shows
//! that instead of the live wallpaper. This module keeps `cosmic-bg`'s config
//! pointed at a still frame of Fresco's own wallpaper, the way `overview`
//! keeps `org.gnome.desktop.background` pointed at one.
//!
//! HARD RULE (feature-wide, not just this file): Fresco never touches
//! authentication. `cosmic-greeter` still owns the password prompt and PAM;
//! this module only ever writes an image path into `cosmic-bg`'s own config
//! files, the same files its own settings UI would write.
//!
//! # The on-disk format
//!
//! `cosmic-config` (the settings store both `cosmic-bg` and `cosmic-greeter`
//! are built on) keeps one RON-encoded value per file under
//! `$XDG_CONFIG_HOME/cosmic/com.system76.CosmicBackground/v1/`, and watches
//! that directory for live reload. Confirmed directly against the real
//! source, `config/src/lib.rs` in <https://github.com/pop-os/cosmic-bg>
//! (fetched 2026-09-28, `master` branch):
//!
//! - `same-on-all` (bool) — when `true`, only the `all` entry is read.  When
//!   `false`, `Config::load` also calls `load_backgrounds`, which reads the
//!   `backgrounds` key (a `Vec<String>` of output names) and, for each name,
//!   loads `output.<name>`. An `output.<name>` file that is never listed in
//!   `backgrounds` is never loaded even though it exists on disk — which is
//!   exactly the "leftover `output.eDP-1` file, but `same-on-all` is `true`,
//!   so nothing reads it" state observed on a real machine before this module
//!   ever ran. Because of this, per-output mode must write `backgrounds` too,
//!   not just the `output.*` files — the one place this module goes beyond a
//!   literal reading of "write `output.<connector>` entries, set
//!   `same-on-all` false".
//! - `all` and `output.<connector>` each hold one `Entry`: `output` (its own
//!   name again), `source` (`Path(PathBuf)` or `Color(..)` — Fresco only ever
//!   writes `Path`), `filter_by_theme`, `rotation_frequency`, `filter_method`
//!   (`Nearest | Linear | Lanczos`, default `Lanczos`), `scaling_mode`
//!   (`Fit([f32;3]) | Stretch | Zoom`, default `Zoom`), and `sampling_method`
//!   (`Alphanumeric | Random`, default `Alphanumeric`). `Config::load` sets
//!   `default_background` (the `all` entry) unconditionally, `same-on-all` or
//!   not — so this module always keeps `all` pointed at the global
//!   wallpaper's still even in per-output mode, as the fallback for any
//!   output without its own override.
//!
//! No `ron` crate dependency is added for this — `cosmic-config`'s writer is
//! predictable enough (one field per line) that hand-written text, generated
//! and scanned the same ad hoc way `overview` handles GVariant text and
//! Cinnamon's `picture-uri-list`, is enough.
//!
//! # Backup and restore
//!
//! Every file this module is about to write is copied byte-for-byte into
//! Fresco's own state directory the first time it is touched (or recorded as
//! "did not exist"), guarded per file rather than once globally — see
//! [`backup_before_write`] — so an apply that touches a file for the first
//! time only partway through a daemon's run (e.g. a monitor override added
//! after the first apply) still gets its true original recorded. [`restore`]
//! puts every tracked file back exactly and deletes the ones that did not
//! exist, then clears the state — idempotent, and file-based so it survives a
//! daemon restart, the same contract `overview::restore` has. As with
//! `overview::is_own_frame_list`, a file already holding Fresco's own frame is
//! never mistaken for the original (e.g. if the state directory was lost
//! while Fresco's frame was live) — it is simply left untracked rather than
//! restored, the same honest gap `overview` accepts for the same reason.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

/// `cosmic-config` application id `cosmic-bg` stores its settings under.
const COSMIC_BG_NAME: &str = "com.system76.CosmicBackground";

/// Prefix on every still-frame file this module writes into the shared
/// `~/.cache/fresco` directory. Distinct from `overview`'s `overview-` prefix
/// so the two modules' cleanup passes never delete each other's cache files —
/// harmless in practice (a session is only ever one desktop) but free to keep
/// separate.
const OUR_FRAME_PREFIX: &str = "cosmic-bg-";

const DEFAULT_ROTATION_FREQUENCY: u64 = 300;
const DEFAULT_FILTER_METHOD: &str = "Lanczos";
const DEFAULT_SCALING_MODE: &str = "Zoom";
const DEFAULT_SAMPLING_METHOD: &str = "Alphanumeric";

/// Whether `current_desktop` (an `XDG_CURRENT_DESKTOP` value — a
/// colon-separated list, e.g. `"COSMIC"`) names the COSMIC desktop. Segment
/// membership is case-insensitive substring containment, mirroring
/// `capability::is_gnome`/`is_cinnamon_name`'s tolerance for desktop names
/// carrying extra prefixes.
fn is_cosmic(current_desktop: Option<&str>) -> bool {
    current_desktop
        .map(|d| {
            d.split(':')
                .any(|seg| seg.trim().to_ascii_lowercase().contains("cosmic"))
        })
        .unwrap_or(false)
}

fn is_active() -> bool {
    is_cosmic(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

/// Keep `cosmic-bg`'s config pointed at a still frame of `config`'s
/// wallpaper(s). No-op off COSMIC — see the module doc.
pub fn apply(config: &Config) {
    if !is_active() {
        return;
    }
    apply_in(&config_root(), &state_root(), config);
}

/// Put every `cosmic-bg` file this module has ever written back exactly as it
/// was, and forget its own state. Unconditional — like `overview::restore`,
/// it is safe (and a no-op) to call even when nothing was ever applied, or off
/// COSMIC entirely: restoring must never depend on re-detecting the desktop,
/// only on whether there is anything left on disk to undo.
pub fn restore() {
    restore_in(&config_root(), &state_root());
}

fn config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cosmic")
        .join(COSMIC_BG_NAME)
        .join("v1")
}

fn state_root() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fresco")
        .join("cosmic-bg-backup")
}

fn own_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("fresco")
}

/// Render stills for `config` and write them into `root`, backing up whatever
/// was there into `state` first.
///
/// Split from [`write_backgrounds`] — the part with real state-machine logic
/// worth testing — purely so this half, which needs
/// `ffmpegthumbnailer`/`ffmpeg` through `overview::render_still`, can stay
/// thin and untested, the same way `overview::apply` itself is never
/// unit-tested.
fn apply_in(root: &Path, state: &Path, config: &Config) {
    let dir = own_cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        log::warn!("cosmic-bg: could not create {}", dir.display());
        return;
    }
    clear_previous_frames(&dir);

    let all_still = super::overview::render_still(&config.wallpaper)
        .and_then(|frame| stash_still(&frame, &dir, "all"));

    let mut per_output = Vec::new();
    for (connector, wallpaper) in &config.monitors {
        if !is_valid_connector(connector) {
            log::warn!("cosmic-bg: skipping monitor key {connector:?} — not a safe connector name");
            continue;
        }
        let Some(frame) = super::overview::render_still(wallpaper) else {
            log::debug!("cosmic-bg: no still available for output {connector}; leaving it as-is");
            continue;
        };
        if let Some(still) = stash_still(&frame, &dir, connector) {
            per_output.push((connector.clone(), still));
        }
    }

    let same_on_all = config.monitors.is_empty();
    write_backgrounds(root, state, same_on_all, all_still.as_deref(), &per_output);

    if same_on_all {
        log::info!("cosmic-bg: background synced (same-on-all)");
    } else {
        log::info!(
            "cosmic-bg: background synced ({} output(s), per-output)",
            per_output.len()
        );
    }
}

/// Delete every still frame this module previously produced. Mirrors
/// `overview::render_still`'s own "drop previous frames" pass, scoped to
/// [`OUR_FRAME_PREFIX`] so the two modules never touch each other's files.
fn clear_previous_frames(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let is_ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(OUR_FRAME_PREFIX));
        if is_ours {
            std::fs::remove_file(path).ok();
        }
    }
}

/// Copy `rendered` (an `overview::render_still` scratch file) into our own
/// timestamped, tagged cache file.
///
/// A copy rather than reusing that path directly, for two reasons: (1)
/// `overview::render_still` deletes every `overview-*` file at the START of
/// its NEXT call, so calling it in a loop for several monitors would delete
/// an earlier monitor's frame before `cosmic-bg` ever read it; (2) a fresh
/// filename each time is what makes a live-reloading reader reliably pick up
/// the change — the same reason `render_still` itself always mints a new one.
fn stash_still(rendered: &Path, dir: &Path, tag: &str) -> Option<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = dir.join(format!(
        "{OUR_FRAME_PREFIX}{}-{stamp}.png",
        sanitize_filename_tag(tag)
    ));
    std::fs::copy(rendered, &dest).ok()?;
    Some(dest)
}

/// Make `tag` (a connector name, or `"all"`) safe as a filename component.
/// RandR/wl-output connector names are always simple ASCII identifiers in
/// practice, but this is cheap insurance against a future oddball name
/// containing a path separator.
fn sanitize_filename_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Longest connector name [`is_valid_connector`] accepts. RandR/wl-output
/// names are always short in practice (`"eDP-1"`, `"HDMI-A-1"`); this is
/// just a sanity ceiling against a config file with a pathologically long
/// value, not a limit anything real should ever approach.
const MAX_CONNECTOR_LEN: usize = 64;

/// Whether `connector` (a `Config.monitors` key — free-form TOML text with
/// nothing validating it on the way in) is safe to embed as a path component
/// in `output.<connector>` (see [`write_backgrounds`], ~line 537 before this
/// check existed).
///
/// Unlike [`sanitize_filename_tag`] (which *replaces* unsafe characters so
/// `stash_still`'s own cache filenames always succeed), this is a strict
/// allow-list gate: `write_backgrounds` skips (and logs) any connector that
/// fails it entirely, rather than silently writing to some mangled-but-still
/// real path. A hand-edited monitor key containing `/` or `..` could
/// otherwise point `output.<connector>` — and, via
/// [`backup_before_write`]/[`restore_in`]'s manifest, a later *delete* —
/// outside the `cosmic-bg` config directory entirely. Only
/// `[A-Za-z0-9._-]` is allowed, with no leading `.` (so this can never start
/// like `..` or collide with this module's own `.{name}.tmp-*` atomic-write
/// temp files), and length is capped at [`MAX_CONNECTOR_LEN`].
///
/// A connector containing `\t`/`\n` is the sharper case this same check
/// closes: [`append_manifest`] writes a relname into a tab-separated,
/// newline-terminated manifest line with no escaping at all, so an
/// unvalidated key such as `"HDMI-1\nM\tsame-on-all"` would inject a
/// *second*, forged manifest line — `str::lines()` splits the one
/// appended blob back into two on [`restore_in`]'s next read — telling
/// restore that the user's real `same-on-all` file never existed and should
/// be deleted. Neither `\t` nor `\n` is in this allow-list, so that key is
/// rejected before it ever reaches [`append_manifest`] at all. See
/// [`is_valid_relname`] for the second, independent gate on the restore
/// side, in case a bad relname ever reaches the manifest some other way.
fn is_valid_connector(connector: &str) -> bool {
    !connector.is_empty()
        && connector.len() <= MAX_CONNECTOR_LEN
        && !connector.starts_with('.')
        && connector
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Whether `relname` — one manifest line's second field, or a filename this
/// module is about to write/delete under the `cosmic-bg` config root — is one
/// this module could legitimately have recorded itself: exactly
/// `"same-on-all"`, `"backgrounds"`, `"all"`, or `"output.<connector>"` for a
/// `connector` that itself passes [`is_valid_connector`].
///
/// [`restore_in`] checks every manifest line against this before touching the
/// filesystem — never trusting the manifest just because it lives under
/// Fresco's own state directory. [`is_valid_connector`] already stops a bad
/// connector from ever being written into a relname or a manifest line in
/// the first place; this is the independent check on the *reading* side, so
/// a manifest corrupted some other way (hand-edited, or written by a future
/// version with a different bug) still can never make `restore_in` write to
/// or delete a path it did not legitimately record.
fn is_valid_relname(relname: &str) -> bool {
    match relname {
        "same-on-all" | "backgrounds" | "all" => true,
        _ => relname
            .strip_prefix("output.")
            .is_some_and(is_valid_connector),
    }
}

/// The four cosmetic `cosmic-bg` `Entry` fields Fresco has no opinion on,
/// lifted from whatever was on disk before Fresco's write (or defaulted).
/// `output`/`source`/`filter_by_theme` are deliberately not here: Fresco
/// always sets those itself — see [`render_entry`].
#[derive(Debug, Clone, PartialEq)]
struct PreservedFields {
    rotation_frequency: u64,
    filter_method: String,
    scaling_mode: String,
    sampling_method: String,
}

impl Default for PreservedFields {
    fn default() -> Self {
        PreservedFields {
            rotation_frequency: DEFAULT_ROTATION_FREQUENCY,
            filter_method: DEFAULT_FILTER_METHOD.to_string(),
            scaling_mode: DEFAULT_SCALING_MODE.to_string(),
            sampling_method: DEFAULT_SAMPLING_METHOD.to_string(),
        }
    }
}

fn valid_filter_method(s: &str) -> bool {
    matches!(s, "Nearest" | "Linear" | "Lanczos")
}

fn valid_sampling_method(s: &str) -> bool {
    matches!(s, "Alphanumeric" | "Random")
}

/// `Fit` carries an `[f32; 3]` payload (`Fit((1.0, 0.0, 0.0))` in RON), so
/// unlike the other two enums it cannot be checked against a fixed list of
/// exact strings. A prefix check is as far as "simple, robust" parsing goes
/// without a real RON parser; anything that fails it falls back to the
/// documented default (`Zoom`) rather than risk preserving something that no
/// longer parses as a `ScalingMode` at all.
fn valid_scaling_mode(s: &str) -> bool {
    s == "Stretch" || s == "Zoom" || s.starts_with("Fit(")
}

/// Extract the raw value text of a `<field>: <value>,` line from an existing
/// `Entry` RON blob.
///
/// Deliberately not a RON parser: `cosmic-config`'s own writer is predictable
/// (one field per line), so a line scan is enough, and a real parser would
/// mean a new crate dependency for four cosmetic fields. The trailing comma
/// is stripped only when it sits outside any paren/bracket nesting, so a
/// tuple-payload value like `Fit((1.0, 0.0, 0.0))` is not truncated at its
/// own inner comma.
fn field_value<'a>(text: &'a str, field: &str) -> Option<&'a str> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(field) else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        return Some(trim_trailing_comma(rest.trim()));
    }
    None
}

fn trim_trailing_comma(value: &str) -> &str {
    let mut depth = 0i32;
    for (i, c) in value.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth <= 0 => return value[..i].trim_end(),
            _ => {}
        }
    }
    value.trim_end()
}

/// Read the four preserved fields out of `existing` (an already-loaded
/// `Entry` file's text, if it existed and was readable), falling back to
/// `cosmic-bg`'s own documented defaults field by field — a corrupt or
/// partial file loses only the fields it corrupted, not all four.
fn preserved_fields_from(existing: Option<&str>) -> PreservedFields {
    let out = PreservedFields::default();
    let Some(text) = existing else {
        return out;
    };
    let mut out = out;
    if let Some(v) = field_value(text, "rotation_frequency").and_then(|v| v.parse().ok()) {
        out.rotation_frequency = v;
    }
    if let Some(v) = field_value(text, "filter_method").filter(|v| valid_filter_method(v)) {
        out.filter_method = v.to_string();
    }
    if let Some(v) = field_value(text, "scaling_mode").filter(|v| valid_scaling_mode(v)) {
        out.scaling_mode = v.to_string();
    }
    if let Some(v) = field_value(text, "sampling_method").filter(|v| valid_sampling_method(v)) {
        out.sampling_method = v.to_string();
    }
    out
}

/// Escape `s` for a RON double-quoted string literal: only `\` and `"` need
/// it, and non-ASCII text passes through as-is — mirrors
/// `overview::gvariant_string_literal`'s treatment of GVariant strings.
fn escape_ron_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Render one `cosmic-bg` `Entry`, in the exact shape `cosmic-config`'s own
/// pretty-printer produces (see the module doc). Field order does not matter
/// to RON's deserializer, but matching it keeps a manual diff of the file
/// readable.
fn render_entry(output: &str, source: &Path, preserved: &PreservedFields) -> String {
    let output = escape_ron_string(output);
    let source = escape_ron_string(&source.to_string_lossy());
    format!(
        "(\n    output: \"{output}\",\n    source: Path(\"{source}\"),\n    filter_by_theme: false,\n    rotation_frequency: {},\n    filter_method: {},\n    scaling_mode: {},\n    sampling_method: {},\n)",
        preserved.rotation_frequency,
        preserved.filter_method,
        preserved.scaling_mode,
        preserved.sampling_method,
    )
}

/// Render the `backgrounds` key: a plain RON list of output names.
fn render_string_list(names: &[String]) -> String {
    let items: Vec<String> = names
        .iter()
        .map(|n| format!("\"{}\"", escape_ron_string(n)))
        .collect();
    format!("[{}]", items.join(", "))
}

/// Write `bytes` to `path` atomically (temp file in the same directory, then
/// rename). `cosmic-config` watches these files for live reload, so a torn
/// write could hand `cosmic-bg` (or `cosmic-greeter`, mid-lock-screen render)
/// a half-written RON blob.
fn write_atomic(path: &Path, bytes: &[u8]) {
    let Some(dir) = path.parent() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::warn!("cosmic-bg: could not create {}: {e}", dir.display());
        return;
    }
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("fresco"),
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        log::warn!("cosmic-bg: could not write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        log::warn!(
            "cosmic-bg: could not replace {} with the new version: {e}",
            path.display()
        );
    }
}

/// Whether `text` (an existing `all`/`output.*` file's content) already
/// points at one of Fresco's own frames — the same check
/// `overview::is_own_frame_list` does for the GNOME/Cinnamon schema. Used so a
/// file already holding Fresco's own output is never mistaken for "the
/// original" (e.g. the state directory was lost while Fresco's frame was
/// live).
fn is_own_source(text: &str) -> bool {
    let marker = own_cache_dir().join(OUR_FRAME_PREFIX);
    text.contains(&marker.to_string_lossy().into_owned())
}

/// Files whose content can itself be one of Fresco's own frames (embeds a
/// `source: Path(...)`). `same-on-all` and `backgrounds` never do — they only
/// ever hold a bool / a list of output names.
fn is_path_bearing(relname: &str) -> bool {
    relname == "all" || relname.starts_with("output.")
}

/// Back up `root.join(relname)` into `state` the first time this module is
/// about to write it.
///
/// Guarded per file (via the manifest), not once globally: a file that only
/// becomes relevant partway through a daemon's run (e.g. a monitor override
/// added after the first apply) still gets its true original recorded,
/// rather than being skipped just because some OTHER file already has a
/// backup on record.
fn backup_before_write(root: &Path, state: &Path, relname: &str) {
    let manifest_path = state.join("manifest");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    if manifest_has(&manifest, relname) {
        return; // already recorded this session
    }
    let real = root.join(relname);
    match std::fs::read(&real) {
        Ok(bytes) => {
            if is_path_bearing(relname) && is_own_source(&String::from_utf8_lossy(&bytes)) {
                log::warn!(
                    "cosmic-bg: {relname} already holds Fresco's own frame with no backup on \
                     record (state was likely lost while a wallpaper was applied); leaving it \
                     untracked rather than recording it as the original"
                );
                return;
            }
            let files_dir = state.join("files");
            if std::fs::create_dir_all(&files_dir).is_err() {
                return;
            }
            write_atomic(&files_dir.join(relname), &bytes);
            append_manifest(&manifest_path, "E", relname);
        }
        Err(_) => append_manifest(&manifest_path, "M", relname),
    }
}

fn manifest_has(manifest: &str, relname: &str) -> bool {
    manifest
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .any(|(_, n)| n.trim() == relname)
}

fn append_manifest(manifest_path: &Path, tag: &str, relname: &str) {
    let mut text = std::fs::read_to_string(manifest_path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(tag);
    text.push('\t');
    text.push_str(relname);
    text.push('\n');
    write_atomic(manifest_path, text.as_bytes());
}

fn backup_and_write(root: &Path, state: &Path, relname: &str, bytes: &[u8]) {
    backup_before_write(root, state, relname);
    write_atomic(&root.join(relname), bytes);
}

fn write_entry(root: &Path, state: &Path, relname: &str, output_name: &str, still: &Path) {
    let existing = std::fs::read_to_string(root.join(relname)).ok();
    let preserved = preserved_fields_from(existing.as_deref());
    let text = render_entry(output_name, still, &preserved);
    backup_and_write(root, state, relname, text.as_bytes());
}

/// The pure state-machine core: given already-rendered still-frame paths,
/// decide exactly what to write and in what order.
///
/// Kept free of `overview::render_still` (see [`apply_in`]) so it can be
/// exercised with plain temp-directory fixtures and fake paths — no
/// `ffmpegthumbnailer` needed.
///
/// Write order matters one way: per-output mode writes the `output.*`
/// entries and `backgrounds` BEFORE flipping `same-on-all` to `false`, so
/// nothing can ever observe `same-on-all: false` while the per-output data it
/// depends on is still stale (or missing). The reverse direction has no such
/// hazard — `all` is loaded unconditionally regardless of `same-on-all` (see
/// the module doc) — so switching back to it needs no particular ordering,
/// and a prior per-output apply's `output.*`/`backgrounds` files are simply
/// left as they are (inert while `same-on-all` is `true`; the same "leftover
/// `output.eDP-1`" state already observed in the wild).
fn write_backgrounds(
    root: &Path,
    state: &Path,
    same_on_all: bool,
    all_still: Option<&Path>,
    per_output: &[(String, PathBuf)],
) {
    if let Some(still) = all_still {
        write_entry(root, state, "all", "all", still);
    }
    if !per_output.is_empty() {
        // Re-validated here, not just trusted from the caller: this is the
        // one place a connector actually becomes a path
        // (`format!("output.{connector}")`) or a manifest relname, so this is
        // where an unsafe key must be stopped even if some future caller
        // ever reaches this function without going through `apply_in`'s own
        // check first. Only names that pass are recorded in `backgrounds`
        // too — an unsafe key must leave no trace anywhere `cosmic-bg` or
        // `restore_in` would later read back.
        let mut names = Vec::new();
        for (connector, still) in per_output {
            if !is_valid_connector(connector) {
                log::warn!(
                    "cosmic-bg: skipping monitor key {connector:?} — not a safe connector name"
                );
                continue;
            }
            let relname = format!("output.{connector}");
            write_entry(root, state, &relname, connector, still);
            names.push(connector.clone());
        }
        backup_and_write(
            root,
            state,
            "backgrounds",
            render_string_list(&names).as_bytes(),
        );
    }
    backup_and_write(
        root,
        state,
        "same-on-all",
        if same_on_all { b"true" } else { b"false" },
    );
}

/// Put every file recorded in `state`'s manifest back exactly, delete the
/// ones that did not exist before Fresco touched them, then clear the state.
///
/// A missing manifest means nothing was ever applied (or a previous restore
/// already ran): silently does nothing, exactly like `overview::restore`'s
/// missing-state-file case.
fn restore_in(root: &Path, state: &Path) {
    let manifest_path = state.join("manifest");
    let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
        return;
    };
    for line in manifest.lines() {
        let Some((tag, relname)) = line.split_once('\t') else {
            continue; // malformed line: ignore rather than panic
        };
        let (tag, relname) = (tag.trim(), relname.trim());
        if relname.is_empty() {
            continue;
        }
        if !is_valid_relname(relname) {
            // Never trust the manifest blindly — see `is_valid_relname`'s own
            // doc comment for exactly how an unvalidated connector could
            // otherwise forge a line here (e.g. a fake `same-on-all` delete)
            // that this module never legitimately wrote.
            log::warn!(
                "cosmic-bg: ignoring manifest entry for {relname:?} — not a name this module \
                 could have written"
            );
            continue;
        }
        let real = root.join(relname);
        match tag {
            "E" => {
                if let Ok(bytes) = std::fs::read(state.join("files").join(relname)) {
                    write_atomic(&real, &bytes);
                }
            }
            "M" => {
                std::fs::remove_file(&real).ok();
            }
            _ => {} // unrecognized tag: ignore rather than panic
        }
    }
    std::fs::remove_dir_all(state).ok();
    log::info!("cosmic-bg: original background configuration restored");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty temp directory for one test. Never under `~/.config` or
    /// `~/.local` — always `std::env::temp_dir()`, mirroring
    /// `overview`'s own ignored integration test's tempdir convention.
    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresco-cosmic-bg-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- detection -------------------------------------------------------

    #[test]
    fn cosmic_desktop_detection_positive() {
        for d in [
            "COSMIC",
            "cosmic",
            "Cosmic",
            "custom:COSMIC",
            "COSMIC:GNOME",
            " COSMIC ",
        ] {
            assert!(
                is_cosmic(Some(d)),
                "expected {d:?} to be detected as COSMIC"
            );
        }
    }

    #[test]
    fn cosmic_desktop_detection_negative() {
        for d in ["GNOME", "KDE", "X-Cinnamon", "", "sway:wlroots"] {
            assert!(!is_cosmic(Some(d)), "did not expect {d:?} to be COSMIC");
        }
        assert!(!is_cosmic(None));
    }

    // --- entry rendering + escaping ---------------------------------------

    #[test]
    fn render_entry_uses_the_confirmed_ron_shape_with_defaults() {
        let text = render_entry(
            "all",
            Path::new("/home/u/.cache/fresco/cosmic-bg-all-1.png"),
            &PreservedFields::default(),
        );
        assert_eq!(
            text,
            "(\n    output: \"all\",\n    source: Path(\"/home/u/.cache/fresco/cosmic-bg-all-1.png\"),\n    filter_by_theme: false,\n    rotation_frequency: 300,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)"
        );
    }

    #[test]
    fn render_entry_embeds_escaped_output_and_source_consistently() {
        // Self-consistent check (built from the same escape function) rather
        // than a hand-transcribed expected literal, since hand-escaping
        // backslashes/quotes for a test is itself error-prone.
        let path = Path::new("/home/u/My \"Wallpaper\" \\ 日本語.png");
        let output_name = "weird\"output";
        let text = render_entry(output_name, path, &PreservedFields::default());
        assert!(text.contains(&format!("output: \"{}\"", escape_ron_string(output_name))));
        assert!(text.contains(&format!(
            "source: Path(\"{}\")",
            escape_ron_string(&path.to_string_lossy())
        )));
    }

    #[test]
    fn escape_ron_string_leaves_plain_text_alone() {
        assert_eq!(escape_ron_string("plain text 123"), "plain text 123");
    }

    #[test]
    fn escape_ron_string_escapes_a_lone_quote_and_a_lone_backslash() {
        assert_eq!(escape_ron_string("\""), "\\\"");
        assert_eq!(escape_ron_string("\\"), "\\\\");
    }

    #[test]
    fn escape_ron_string_escapes_backslash_and_quote_together_in_order() {
        assert_eq!(escape_ron_string("a\\b\"c"), "a\\\\b\\\"c");
    }

    #[test]
    fn escape_ron_string_roundtrips_unicode_and_spaces() {
        let s = "日本語 with spaces and % signs";
        assert_eq!(escape_ron_string(s), s);
    }

    #[test]
    fn render_string_list_escapes_and_joins() {
        assert_eq!(render_string_list(&[]), "[]");
        assert_eq!(render_string_list(&["eDP-1".to_string()]), "[\"eDP-1\"]");
        assert_eq!(
            render_string_list(&["eDP-1".to_string(), "HDMI-A-1".to_string()]),
            "[\"eDP-1\", \"HDMI-A-1\"]"
        );
    }

    #[test]
    fn sanitize_filename_tag_keeps_safe_characters_and_replaces_the_rest() {
        assert_eq!(sanitize_filename_tag("eDP-1"), "eDP-1");
        assert_eq!(sanitize_filename_tag("HDMI-A-1"), "HDMI-A-1");
        assert_eq!(sanitize_filename_tag("weird/name:here"), "weird_name_here");
    }

    // --- field preservation ------------------------------------------------

    #[test]
    fn preserved_fields_reads_the_real_machine_example_verbatim() {
        // The exact "all" entry observed on a real COSMIC 1.9 / cosmic-bg
        // 0.1.0 machine (Pop!_OS 24.04), byte-for-byte.
        let text = "(\n    output: \"all\",\n    source: Path(\"/usr/share/backgrounds/cosmic/otherworldly_earth_nasa_ISS064-E-29444.jpg\"),\n    filter_by_theme: true,\n    rotation_frequency: 300,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)";
        let p = preserved_fields_from(Some(text));
        assert_eq!(
            p,
            PreservedFields {
                rotation_frequency: 300,
                filter_method: "Lanczos".to_string(),
                scaling_mode: "Zoom".to_string(),
                sampling_method: "Alphanumeric".to_string(),
            }
        );
    }

    #[test]
    fn preserved_fields_defaults_when_nothing_to_read() {
        assert_eq!(preserved_fields_from(None), PreservedFields::default());
    }

    #[test]
    fn preserved_fields_keeps_a_non_default_rotation_frequency_and_filter() {
        let text = "(\n    rotation_frequency: 900,\n    filter_method: Nearest,\n)";
        let p = preserved_fields_from(Some(text));
        assert_eq!(p.rotation_frequency, 900);
        assert_eq!(p.filter_method, "Nearest");
        // Fields absent from this (deliberately partial) blob still default.
        assert_eq!(p.scaling_mode, DEFAULT_SCALING_MODE);
        assert_eq!(p.sampling_method, DEFAULT_SAMPLING_METHOD);
    }

    #[test]
    fn preserved_fields_handles_a_fit_scaling_mode_without_truncating_its_inner_comma() {
        let text = "(\n    scaling_mode: Fit((1.0, 0.5, 0.0)),\n    sampling_method: Random,\n)";
        let p = preserved_fields_from(Some(text));
        assert_eq!(p.scaling_mode, "Fit((1.0, 0.5, 0.0))");
        assert_eq!(p.sampling_method, "Random");
    }

    #[test]
    fn preserved_fields_ignores_an_unrecognized_variant_and_falls_back() {
        let text = "(\n    filter_method: SomethingMadeUp,\n    sampling_method: Bogus,\n)";
        let p = preserved_fields_from(Some(text));
        assert_eq!(p.filter_method, DEFAULT_FILTER_METHOD);
        assert_eq!(p.sampling_method, DEFAULT_SAMPLING_METHOD);
    }

    #[test]
    fn preserved_fields_and_field_value_never_panic_on_garbage_or_partial_text() {
        let garbage_inputs = [
            "",
            "not even ron",
            "(\n    rotation_frequency\n)",   // no colon
            "(\n    rotation_frequency: \n)", // no value
            "(\n    rotation_frequency: notanumber,\n)",
            "(\n    filter_method: SomethingMadeUp,\n", // unterminated
            "(\n    scaling_mode: Fit((1.0, 2.0,\n",    // truncated mid-tuple
            "\u{0}garbage\u{0}",
            "::::\n\t\t\n",
        ];
        for text in garbage_inputs {
            // Must not panic; the exact result doesn't matter here beyond that.
            let _ = preserved_fields_from(Some(text));
            let _ = field_value(text, "rotation_frequency");
        }
    }

    #[test]
    fn field_value_handles_crlf_line_endings() {
        let text = "(\r\n    rotation_frequency: 42,\r\n)";
        assert_eq!(field_value(text, "rotation_frequency"), Some("42"));
    }

    #[test]
    fn field_value_does_not_false_match_a_field_name_that_is_a_prefix_of_another() {
        // "filter_method" must not match a line for "filter_by_theme".
        let text = "(\n    filter_by_theme: false,\n)";
        assert_eq!(field_value(text, "filter_method"), None);
    }

    // --- backup / restore ---------------------------------------------------

    #[test]
    fn write_then_restore_restores_exact_bytes() {
        let root = tempdir("root-a");
        let state = tempdir("state-a");
        let original_same_on_all = "true";
        let original_all = "(\n    output: \"all\",\n    source: Path(\"/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg\"),\n    filter_by_theme: true,\n    rotation_frequency: 900,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)";
        std::fs::write(root.join("same-on-all"), original_same_on_all).unwrap();
        std::fs::write(root.join("all"), original_all).unwrap();

        write_backgrounds(
            &root,
            &state,
            true,
            Some(Path::new("/fake/cache/cosmic-bg-all-1.png")),
            &[],
        );
        let after_apply = std::fs::read_to_string(root.join("all")).unwrap();
        assert_ne!(after_apply, original_all);
        assert!(after_apply.contains("cosmic-bg-all-1.png"));
        // Preserved the original's non-default rotation_frequency.
        assert!(after_apply.contains("rotation_frequency: 900"));

        restore_in(&root, &state);
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            original_same_on_all
        );
        assert_eq!(
            std::fs::read_to_string(root.join("all")).unwrap(),
            original_all
        );
        assert!(!state.join("manifest").exists(), "state must be cleared");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn restore_deletes_files_that_did_not_exist_before() {
        let root = tempdir("root-b");
        let state = tempdir("state-b");
        // Nothing pre-exists under `root` at all.

        write_backgrounds(
            &root,
            &state,
            false,
            Some(Path::new("/fake/all.png")),
            &[("eDP-1".to_string(), PathBuf::from("/fake/edp1.png"))],
        );
        assert!(root.join("all").exists());
        assert!(root.join("output.eDP-1").exists());
        assert!(root.join("backgrounds").exists());
        assert!(root.join("same-on-all").exists());

        restore_in(&root, &state);
        assert!(!root.join("all").exists());
        assert!(!root.join("output.eDP-1").exists());
        assert!(!root.join("backgrounds").exists());
        assert!(!root.join("same-on-all").exists());
        assert!(!state.exists(), "state directory must be removed");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn second_write_does_not_re_back_up_frescos_own_first_write() {
        let root = tempdir("root-c");
        let state = tempdir("state-c");
        let original = "(\n    output: \"all\",\n    source: Path(\"/usr/share/backgrounds/original.jpg\"),\n    filter_by_theme: true,\n    rotation_frequency: 300,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)";
        std::fs::write(root.join("all"), original).unwrap();

        write_backgrounds(&root, &state, true, Some(Path::new("/fake/one.png")), &[]);
        write_backgrounds(&root, &state, true, Some(Path::new("/fake/two.png")), &[]);
        let after_second = std::fs::read_to_string(root.join("all")).unwrap();
        assert!(after_second.contains("two.png"));

        restore_in(&root, &state);
        // Restored to the TRUE original, not the first apply's own frame.
        assert_eq!(std::fs::read_to_string(root.join("all")).unwrap(), original);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn backup_skips_a_file_already_holding_our_own_frame_when_state_was_lost() {
        let root = tempdir("root-d");
        let state = tempdir("state-d"); // exists as a dir, but has no manifest yet
        let our_frame = own_cache_dir().join(format!("{OUR_FRAME_PREFIX}all-999.png"));
        let leftover = format!(
            "(\n    output: \"all\",\n    source: Path(\"{}\"),\n    filter_by_theme: false,\n    rotation_frequency: 300,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)",
            our_frame.display()
        );
        std::fs::write(root.join("all"), &leftover).unwrap();
        // No manifest at all: simulates the state directory having been lost
        // while Fresco's own frame was still the live value.

        backup_before_write(&root, &state, "all");

        let manifest = std::fs::read_to_string(state.join("manifest")).unwrap_or_default();
        assert!(
            !manifest_has(&manifest, "all"),
            "must not record our own leftover frame as the original"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn per_output_mode_then_flip_to_same_on_all_then_restore() {
        let root = tempdir("root-e");
        let state = tempdir("state-e");
        let original_same_on_all = "true";
        let original_all = "(\n    output: \"all\",\n    source: Path(\"/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg\"),\n    filter_by_theme: true,\n    rotation_frequency: 3600,\n    filter_method: Lanczos,\n    scaling_mode: Zoom,\n    sampling_method: Alphanumeric,\n)";
        std::fs::write(root.join("same-on-all"), original_same_on_all).unwrap();
        std::fs::write(root.join("all"), original_all).unwrap();
        // output.eDP-1 and backgrounds do not exist yet.

        // 1) Per-output apply.
        write_backgrounds(
            &root,
            &state,
            false,
            Some(Path::new("/fake/all-1.png")),
            &[("eDP-1".to_string(), PathBuf::from("/fake/edp1-1.png"))],
        );
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            "false"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("backgrounds")).unwrap(),
            "[\"eDP-1\"]"
        );
        assert!(std::fs::read_to_string(root.join("output.eDP-1"))
            .unwrap()
            .contains("edp1-1.png"));

        // 2) Flip back to same-on-all (e.g. the per-monitor override was
        // removed from Fresco's config).
        write_backgrounds(&root, &state, true, Some(Path::new("/fake/all-2.png")), &[]);
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            "true"
        );
        assert!(std::fs::read_to_string(root.join("all"))
            .unwrap()
            .contains("all-2.png"));
        // output.eDP-1 / backgrounds are left as they were from step 1 —
        // inert while same-on-all is true (see the module doc's write-order
        // note), not this call's concern.
        assert_eq!(
            std::fs::read_to_string(root.join("backgrounds")).unwrap(),
            "[\"eDP-1\"]"
        );

        // 3) Restore must undo BOTH calls' worth of backed-up files.
        restore_in(&root, &state);
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            original_same_on_all
        );
        assert_eq!(
            std::fs::read_to_string(root.join("all")).unwrap(),
            original_all
        );
        assert!(
            !root.join("output.eDP-1").exists(),
            "did not exist originally"
        );
        assert!(
            !root.join("backgrounds").exists(),
            "did not exist originally"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restore_ignores_a_malformed_manifest_without_panicking() {
        let root = tempdir("root-f");
        let state = tempdir("state-f");
        std::fs::write(root.join("all"), "original").unwrap();
        std::fs::create_dir_all(state.join("files")).unwrap();
        // A line with no tab, an unknown tag, an empty relname, and a line
        // naming a file with no corresponding backup under files/.
        std::fs::write(
            state.join("manifest"),
            "not a valid line\nZ\tall\nE\t\nE\tsame-on-all\n",
        )
        .unwrap();

        restore_in(&root, &state); // must not panic
        assert!(!state.join("manifest").exists(), "state still gets cleared");

        std::fs::remove_dir_all(&root).ok();
    }

    // --- connector / relname validation --------------------------------------

    #[test]
    fn is_valid_connector_accepts_real_world_names() {
        for name in ["eDP-1", "HDMI-A-1", "DP-2", "VGA-1", "eDP.1", "a", "A0"] {
            assert!(is_valid_connector(name), "{name:?} should be valid");
        }
    }

    #[test]
    fn is_valid_connector_rejects_path_separators_and_traversal() {
        for name in ["../../etc/passwd", "a/b", "/etc/passwd", "..", "."] {
            assert!(!is_valid_connector(name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn is_valid_connector_rejects_a_leading_dot() {
        assert!(!is_valid_connector(".hidden"));
        assert!(!is_valid_connector(".eDP-1"));
    }

    #[test]
    fn is_valid_connector_rejects_embedded_tab_or_newline() {
        // The exact injection shape the security review flagged: a connector
        // that would forge a second, unrelated manifest line if it ever
        // reached `format!("output.{connector}")` unvalidated.
        assert!(!is_valid_connector("HDMI-1\nM\tsame-on-all"));
        assert!(!is_valid_connector("a\tb"));
        assert!(!is_valid_connector("a\nb"));
    }

    #[test]
    fn is_valid_connector_rejects_empty_and_overlong_names() {
        assert!(!is_valid_connector(""));
        assert!(!is_valid_connector(&"a".repeat(MAX_CONNECTOR_LEN + 1)));
        assert!(is_valid_connector(&"a".repeat(MAX_CONNECTOR_LEN)));
    }

    #[test]
    fn is_valid_relname_accepts_the_fixed_names_and_valid_outputs() {
        for name in [
            "same-on-all",
            "backgrounds",
            "all",
            "output.eDP-1",
            "output.HDMI-A-1",
        ] {
            assert!(is_valid_relname(name), "{name:?} should be valid");
        }
    }

    #[test]
    fn is_valid_relname_rejects_anything_else() {
        for name in [
            "",
            "output.",
            "output..",
            "output./etc/passwd",
            "output.HDMI-1\nM\tsame-on-all",
            "same-on-all\nM\tall",
            "../all",
            "random-file",
        ] {
            assert!(!is_valid_relname(name), "{name:?} should be rejected");
        }
    }

    // --- write_backgrounds: invalid connectors never reach a path -----------

    #[test]
    fn write_backgrounds_skips_an_unsafe_connector_and_keeps_the_valid_one() {
        let root = tempdir("root-unsafe-connector");
        let state = tempdir("state-unsafe-connector");

        write_backgrounds(
            &root,
            &state,
            false,
            Some(Path::new("/fake/all.png")),
            &[
                ("../escape".to_string(), PathBuf::from("/fake/bad.png")),
                ("eDP-1".to_string(), PathBuf::from("/fake/edp1.png")),
            ],
        );

        assert!(root.join("output.eDP-1").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("backgrounds")).unwrap(),
            "[\"eDP-1\"]",
            "the unsafe key must not appear in the backgrounds list either"
        );
        // Every file actually written under `root` must be one of the
        // expected, safe names — ruling out anything written under any path
        // shape the rejected key's `/` could otherwise have produced (e.g. a
        // nested `output..` directory from `write_atomic`'s own
        // `create_dir_all` on the parent of `output../escape`).
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["all", "backgrounds", "output.eDP-1", "same-on-all"]);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    // --- restore_in: a forged/invalid manifest relname is never acted on ----

    #[test]
    fn restore_ignores_a_manifest_line_naming_an_invalid_relname() {
        let root = tempdir("root-invalid-relname");
        let state = tempdir("state-invalid-relname");
        let original = "true";
        std::fs::write(root.join("same-on-all"), original).unwrap();
        std::fs::create_dir_all(state.join("files")).unwrap();
        // A forged-looking "M\tsame-on-all" would be legitimate on its own —
        // it is only ever safe here because *this* line's relname
        // (`"../../etc/whatever"`) fails `is_valid_relname`, standing in for
        // any bad relname that should never be acted on regardless of how it
        // got into the manifest.
        std::fs::write(state.join("manifest"), "M\t../../etc/whatever\n").unwrap();

        restore_in(&root, &state);

        assert!(
            root.join("same-on-all").exists(),
            "an invalid relname must never cause a delete"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            original
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // --- end-to-end regression: the exact injection from the security review

    #[test]
    fn malicious_connector_key_cannot_forge_a_manifest_line_that_deletes_same_on_all() {
        // Regression test for the exact injection flagged in review:
        // `append_manifest` writes a relname into a tab/newline-delimited
        // line with no escaping. Before `is_valid_connector` gated the
        // per-output loop, a monitor key of `"HDMI-1\nM\tsame-on-all"` would
        // turn into the manifest line `M\toutput.HDMI-1\nM\tsame-on-all\n` —
        // which `str::lines()` splits back into TWO lines on restore, the
        // second one a forged `M\tsame-on-all` ("this file did not exist,
        // delete it"). Worse, because that forged line would land in the
        // manifest BEFORE this function's own, legitimate
        // `backup_and_write(.., "same-on-all", ..)` call further down,
        // `manifest_has` would see `same-on-all` as "already recorded" and
        // skip the real backup entirely — so the user's real content would
        // never be saved AND would be marked for deletion on restore.
        let malicious = "HDMI-1\nM\tsame-on-all";
        assert!(
            !is_valid_connector(malicious),
            "the injected key must be rejected outright"
        );

        let root = tempdir("root-inject");
        let state = tempdir("state-inject");
        let original_same_on_all = "true";
        std::fs::write(root.join("same-on-all"), original_same_on_all).unwrap();

        write_backgrounds(
            &root,
            &state,
            false,
            Some(Path::new("/fake/all.png")),
            &[
                (malicious.to_string(), PathBuf::from("/fake/bad.png")),
                ("eDP-1".to_string(), PathBuf::from("/fake/edp1.png")),
            ],
        );

        // The malicious key must never reach a path or the manifest at all.
        assert!(!root.join(format!("output.{malicious}")).exists());
        let manifest = std::fs::read_to_string(state.join("manifest")).unwrap();
        assert!(
            !manifest.contains("HDMI-1"),
            "the rejected key must leave no trace in the manifest: {manifest:?}"
        );
        // The legitimate `same-on-all` backup must actually have happened —
        // not been shadowed by a forged manifest entry claiming it was
        // already recorded.
        assert!(
            state.join("files").join("same-on-all").exists(),
            "the real same-on-all must still get backed up"
        );

        restore_in(&root, &state);
        assert!(
            root.join("same-on-all").exists(),
            "restore must not delete the real same-on-all file"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("same-on-all")).unwrap(),
            original_same_on_all,
            "restore must put back the true original content, not leave Fresco's own write in place"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&state).ok();
    }
}
