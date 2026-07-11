import { strict as assert } from "node:assert";
import { test } from "node:test";

import { modelVisibleResult } from "../src/model_result";

test("removes explicit session handles from model-visible results", () => {
  const result = modelVisibleResult({
    agent_session_id: "session-secret",
    mode: "ambient",
    tab: {
      tab_id: "tab-visible",
      nested: [{ agent_session_id: "nested-secret", value: 1 }],
    },
  });

  assert.deepEqual(result, {
    mode: "ambient",
    tab: {
      tab_id: "tab-visible",
      nested: [{ value: 1 }],
    },
  });
});

test("does not mutate the broker result", () => {
  const brokerResult = { agent_session_id: "session-secret", mode: "explicit" };
  modelVisibleResult(brokerResult);
  assert.equal(brokerResult.agent_session_id, "session-secret");
});
