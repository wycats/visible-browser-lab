import { strict as assert } from "node:assert";
import { test } from "node:test";

import { confirmationFor } from "../src/confirmation";

test("durable release confirmation includes the authorizing instruction", () => {
  const confirmation = confirmationFor("release_tab", {
    tab_id: "tab-owned",
    leave_visible: true,
    user_instruction: "Leave this visible for comparison.",
  });

  assert.equal(confirmation?.title, "Leave browser tab visible?");
  assert.equal(
    confirmation?.message,
    'Release owned tab tab-owned and preserve it after this session expires. User instruction: "Leave this visible for comparison.".',
  );
});

test("durable release confirmation identifies a missing instruction", () => {
  const confirmation = confirmationFor("release_tab", {
    tab_id: "tab-owned",
    leave_visible: true,
    user_instruction: "   ",
  });

  assert.equal(
    confirmation?.message,
    "Release owned tab tab-owned and preserve it after this session expires. User instruction: (missing; this request will be rejected).",
  );
});
