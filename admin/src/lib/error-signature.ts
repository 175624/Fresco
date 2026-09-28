/**
 * Pure parsing of a `sig:` signature out of a telemetry `errors.detail`
 * string, so a "12 rows" group in the reliability table can be broken down
 * by root cause instead of shown as one undifferentiated count with a single
 * sample detail (see reliability/page.tsx).
 *
 * Deliberately free of Supabase, `server-only` and React — a plain string in,
 * a plain string out — so it is unit-testable with `node:test`
 * (see error-signature.test.ts) without a database or a Next.js runtime.
 *
 * `detail` strings look like:
 *   "LVDS-1: renderer failed 5x (mode=never_started, kind=Video, cause=exited_early:egl) exit=c1 sig=tok:egl_context"
 *
 * Preference order for the signature:
 *   1. the `sig=` token, when present (`tok:egl_context`, `h:1a2b3c4d`) —
 *      most specific, and what the daemon emits when it can name the cause.
 *   2. the `cause=` value, when present (`exited_early:egl`) — the daemon
 *      knows *what* failed even without a `sig`.
 *   3. the whole detail with a leading output-name prefix stripped
 *      (`LVDS-1:`, `eDP-2:`, `HDMI-A-1:`, `DP-1:`) — the only case left is a
 *      detail shape this parser doesn't recognize, and grouping by verbatim
 *      text (ignoring which monitor it happened on) is still better than one
 *      bucket per detail string.
 */

const SIG_RE = /sig=([^\s,)]+)/;
const CAUSE_RE = /cause=([^\s,)]+)/;
const MODE_RE = /mode=([^\s,)]+)/;
// Connector names: LVDS-1, eDP-2, HDMI-A-1, DP-1, ... — letters/digits
// segments joined by hyphens, followed by a colon and whitespace.
const CONNECTOR_PREFIX_RE = /^[A-Za-z][A-Za-z0-9]*(?:-[A-Za-z0-9]+)*:\s*/;

/** Falls back to when `detail` is null, empty, or otherwise unparseable. */
export const UNKNOWN_SIGNATURE = "unknown";

/**
 * Extract a short, stable cause signature from one error's `detail` string.
 * Two rows with the same signature are the same failure; two rows with
 * different signatures under the same kind+version are different failures
 * that happened to land in the same count.
 */
export function parseErrorSignature(detail: string | null | undefined): string {
  if (!detail) return UNKNOWN_SIGNATURE;
  const trimmed = detail.trim();
  if (!trimmed) return UNKNOWN_SIGNATURE;

  const sigMatch = SIG_RE.exec(trimmed);
  if (sigMatch) return sigMatch[1];

  const causeMatch = CAUSE_RE.exec(trimmed);
  if (causeMatch) {
    // `cause=spawn_ok` alone doesn't say whether the renderer crashed
    // (mode=dead) or hung (mode=frozen) — two different failures that must
    // not collapse into one bucket, so the mode rides along when present.
    const modeMatch = MODE_RE.exec(trimmed);
    return modeMatch ? `${causeMatch[1]} · mode=${modeMatch[1]}` : causeMatch[1];
  }

  return trimmed.replace(CONNECTOR_PREFIX_RE, "");
}
