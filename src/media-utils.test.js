import assert from "node:assert/strict";
import test from "node:test";
import { formatDuration } from "./media-utils.js";

test("formats minute and hour durations", () => {
  assert.equal(formatDuration(95), "1:35");
  assert.equal(formatDuration(3661), "1:01:01");
});

test("handles non-finite input", () => {
  assert.equal(formatDuration(Number.NaN), "—");
});
