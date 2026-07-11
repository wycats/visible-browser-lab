export function modelVisibleResult(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(modelVisibleResult);
  }
  if (!isRecord(value)) {
    return value;
  }

  return Object.fromEntries(
    Object.entries(value)
      .filter(([key]) => key !== "agent_session_id")
      .map(([key, entry]) => [key, modelVisibleResult(entry)]),
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
