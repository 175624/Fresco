import assert from "node:assert/strict";
import { test } from "node:test";

import { parseErrorSignature, UNKNOWN_SIGNATURE } from "./error-signature.ts";

test("parseErrorSignature: prefers the sig= token when present", () => {
  assert.equal(
    parseErrorSignature(
      "LVDS-1: renderer failed 5x (mode=never_started, kind=Video, cause=exited_early:egl) exit=c1 sig=tok:egl_context"
    ),
    "tok:egl_context"
  );
  assert.equal(
    parseErrorSignature(
      "eDP-2: renderer failed 10x (mode=never_started, kind=Video, cause=exited_early:mpv) exit=c2 sig=tok:mpv_init"
    ),
    "tok:mpv_init"
  );
  assert.equal(
    parseErrorSignature("HDMI-A-1: crashed sig=h:1a2b3c4d"),
    "h:1a2b3c4d"
  );
});

test("parseErrorSignature: falls back to cause= when sig= is absent", () => {
  assert.equal(
    parseErrorSignature(
      "DP-1: renderer failed 3x (mode=never_started, kind=Video, cause=exited_early:egl) exit=c1"
    ),
    "exited_early:egl · mode=never_started"
  );
});

test("parseErrorSignature: appends mode= to cause= so dead vs frozen don't share a bucket", () => {
  assert.equal(
    parseErrorSignature(
      "DP-1: renderer failed 5x (mode=dead, kind=Video, cause=spawn_ok) exit=c1"
    ),
    "spawn_ok · mode=dead"
  );
  assert.equal(
    parseErrorSignature(
      "DP-1: renderer failed 5x (mode=frozen, kind=Video, cause=spawn_ok) exit=c1"
    ),
    "spawn_ok · mode=frozen"
  );
});

test("parseErrorSignature: falls back to detail with the output prefix stripped", () => {
  assert.equal(
    parseErrorSignature("LVDS-1: something unrecognized happened"),
    "something unrecognized happened"
  );
  assert.equal(
    parseErrorSignature("eDP-2: another shape"),
    "another shape"
  );
  assert.equal(
    parseErrorSignature("HDMI-A-1: yet another"),
    "yet another"
  );
  // No connector prefix at all — used verbatim.
  assert.equal(parseErrorSignature("no prefix here"), "no prefix here");
});

test("parseErrorSignature: null/undefined/empty detail is unknown", () => {
  assert.equal(parseErrorSignature(null), UNKNOWN_SIGNATURE);
  assert.equal(parseErrorSignature(undefined), UNKNOWN_SIGNATURE);
  assert.equal(parseErrorSignature(""), UNKNOWN_SIGNATURE);
  assert.equal(parseErrorSignature("   "), UNKNOWN_SIGNATURE);
});
