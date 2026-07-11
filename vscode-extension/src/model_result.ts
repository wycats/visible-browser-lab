export function modelVisibleResult(value: unknown): unknown {
  if (!isRecord(value)) {
    return value;
  }

  const { agent_session_id: _agentSessionId, ...modelVisible } = value;
  return modelVisible;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
