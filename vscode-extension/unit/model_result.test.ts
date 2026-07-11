import { strict as assert } from "node:assert";
import { test } from "node:test";

import { modelVisibleResult } from "../src/model_result";

test("removes explicit session handles from model-visible results", () => {
  const result = modelVisibleResult({
    agent_session_id: "session-secret",
    mode: "ambient",
    tab: {
      tab_id: "tab-visible",
      nested: [{ agent_session_id: "page-controlled-value", value: 1 }],
    },
  });

  assert.deepEqual(result, {
    mode: "ambient",
    tab: {
      tab_id: "tab-visible",
      nested: [{ agent_session_id: "page-controlled-value", value: 1 }],
    },
  });
});

test("preserves page-controlled fields in nested evaluate results", () => {
  const value = {
    value: {
      user: { agent_session_id: "application-data" },
    },
  };
  assert.deepEqual(modelVisibleResult(value), value);
});

test("preserves top-level array results", () => {
  const value = [{ agent_session_id: "page-controlled-value", value: 1 }];
  assert.deepEqual(modelVisibleResult(value), value);
});

test("does not mutate the broker result", () => {
  const brokerResult = { agent_session_id: "session-secret", mode: "explicit" };
  modelVisibleResult(brokerResult);
  assert.equal(brokerResult.agent_session_id, "session-secret");
});
