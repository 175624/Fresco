//! Parity tests for every place mpvpaper's two vendored patches get applied:
//! `install.sh`'s inline curl|bash heredocs, `scripts/build-mpvpaper.sh`'s
//! local rebuild. Both
//! build paths must apply the *same* two patches — `packaging/mpvpaper/`'s
//! `0001-egl-context-gl-compat-and-gles-fallback.patch` and
//! `0002-cosmic-session-lock-show-on-lock.patch` — in the *same* order.
//!
//! `src/daemon/mpvpaper.rs`'s own `#[cfg(test)]` module already covers the
//! first patch's `install.sh` copy
//! (`install_sh_mpvpaper_patch_matches_packaged_patch`); this file is a
//! standalone integration test (rather than adding to that unit test module)
//! so it can also read `scripts/build-mpvpaper.sh`,
//! which that unit test doesn't touch, without reaching into
//! `src/daemon/mpvpaper.rs` — another patch is already changing that file.
//!
//! [`extract_heredoc_body`] and [`extract_diff_body`] below are deliberate,
//! exact duplicates of the private helpers of the same name in
//! `src/daemon/mpvpaper.rs`'s test module: `#[cfg(test)]` items are not part
//! of the library an integration test binary links against, so they are not
//! reachable from here, and the two copies must stay byte-for-byte identical
//! in behaviour or this file's own parity test would stop meaning what it
//! says.

use std::path::{Path, PathBuf};

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Extracts the body of a `<<'MARKER'` ... `MARKER` heredoc from `script`
/// (exclusive of the marker lines themselves).
///
/// Mirrors `src/daemon/mpvpaper.rs`'s own `extract_heredoc_body` exactly.
fn extract_heredoc_body(script: &str, marker: &str) -> Option<String> {
    let start_needle = format!("<<'{marker}'");
    let start = script.find(&start_needle)?;
    let body_start = script[start..].find('\n')? + start + 1;
    let end_needle = format!("\n{marker}\n");
    let end = script[body_start..].find(&end_needle)? + body_start;
    Some(script[body_start..end].to_string())
}

/// Extracts a patch file's diff body, skipping any explanatory `#` header,
/// starting from the first `diff --git` or `---` line.
///
/// Mirrors `src/daemon/mpvpaper.rs`'s own `extract_diff_body` exactly.
fn extract_diff_body(patch: &str) -> Option<String> {
    let idx = patch
        .lines()
        .position(|l| l.starts_with("diff --git") || l.starts_with("---"))?;
    Some(patch.lines().skip(idx).collect::<Vec<_>>().join("\n"))
}

/// `install.sh` embeds the mpvpaper show-on-lock patch inline (inside a
/// `<<'MPVPAPER_SHOW_ON_LOCK_PATCH_EOF'` heredoc), for the same reason as the
/// EGL fallback patch covered by
/// `install_sh_mpvpaper_patch_matches_packaged_patch` in
/// `src/daemon/mpvpaper.rs`: the script runs standalone via `curl | bash`,
/// with no repo checkout to read
/// `packaging/mpvpaper/0002-cosmic-session-lock-show-on-lock.patch` from.
/// That inline copy and the packaged file (also applied by
/// `scripts/build-mpvpaper.sh`) must stay
/// byte-identical, or the build paths silently diverge.
#[test]
fn install_sh_mpvpaper_show_on_lock_patch_matches_packaged_patch() {
    let root = manifest_root();
    let install_sh = read(&root, "install.sh");
    let patch_file = read(
        &root,
        "packaging/mpvpaper/0002-cosmic-session-lock-show-on-lock.patch",
    );

    let heredoc_body = extract_heredoc_body(&install_sh, "MPVPAPER_SHOW_ON_LOCK_PATCH_EOF")
        .expect("install.sh must contain a MPVPAPER_SHOW_ON_LOCK_PATCH_EOF heredoc");
    let patch_diff_body = extract_diff_body(&patch_file)
        .expect("packaged patch must contain a diff body (--- or diff --git line)");

    assert_eq!(
        heredoc_body.trim_end(),
        patch_diff_body.trim_end(),
        "install.sh's inline MPVPAPER_SHOW_ON_LOCK_PATCH_EOF heredoc has drifted from \
         packaging/mpvpaper/0002-cosmic-session-lock-show-on-lock.patch. Update whichever \
         one is stale so both build paths apply the same show-on-lock patch."
    );
}

/// `scripts/build-mpvpaper.sh` must apply the EGL fallback patch (`0001-...`,
/// `$PATCH`) before the show-on-lock patch (`0002-...`, `$PATCH2`): the
/// second patch's hunks assume the first patch's `src/main.c` changes (the
/// `egl_is_gles` flag) are already applied — as the patch's
/// own header explains. Reordering the two
/// `git apply` calls would still leave every other parity check in this
/// crate green (they only compare file *contents*, not the order patches are
/// applied) while silently breaking the local-rebuild path, which is what
/// this test exists to catch.
#[test]
fn build_mpvpaper_sh_applies_both_patches_in_order() {
    let root = manifest_root();
    let script = read(&root, "scripts/build-mpvpaper.sh");

    assert!(
        script.contains("packaging/mpvpaper/0001-egl-context-gl-compat-and-gles-fallback.patch"),
        "scripts/build-mpvpaper.sh must reference the packaged EGL fallback patch"
    );
    assert!(
        script.contains("packaging/mpvpaper/0002-cosmic-session-lock-show-on-lock.patch"),
        "scripts/build-mpvpaper.sh must reference the packaged show-on-lock patch"
    );

    let apply_first = script
        .find("git apply --verbose \"$PATCH\"")
        .expect("scripts/build-mpvpaper.sh must apply the EGL fallback patch ($PATCH)");
    let apply_second = script
        .find("git apply --verbose \"$PATCH2\"")
        .expect("scripts/build-mpvpaper.sh must apply the show-on-lock patch ($PATCH2)");
    assert!(
        apply_first < apply_second,
        "scripts/build-mpvpaper.sh must apply the EGL fallback patch ($PATCH) before the \
         show-on-lock patch ($PATCH2) — the show-on-lock patch's hunks assume the EGL \
         patch's src/main.c changes are already applied"
    );
}
