export interface ConversationIdentityPayload {
  version: 1;
  issuer: "com.microsoft.vscode";
  id: string;
}

export interface SurfaceRequestContext {
  conversation_identity?: ConversationIdentityPayload;
  workspace_root?: string;
}

export type InvocationContextResolution =
  | { kind: "ambient"; context: SurfaceRequestContext }
  | { kind: "global" }
  | { kind: "unsupported" };

export function withWorkspaceFallback(
  context: SurfaceRequestContext | undefined,
  fallbackWorkspaceRoot: string | undefined,
): SurfaceRequestContext {
  if (context) {
    return { ...context };
  }
  return fallbackWorkspaceRoot ? { workspace_root: fallbackWorkspaceRoot } : {};
}

export function unsupportedInvocationTokenError(method: string): string {
  return `${method} failed with unsupported_host. VS Code did not expose a compatible chat session resource; Visible Browser Lab requires VS Code 1.120 or newer with the supported invocation-token shape. Recovery: update and reload VS Code, or use the explicit MCP/CLI surface`;
}

export function supportsUnsupportedTokenInvocation(method: string): boolean {
  return method === "help";
}

interface UriLike {
  scheme: string;
  path: string;
  fsPath: string;
  toString(skipEncoding?: boolean): string;
}

export function extractInvocationContext(options: unknown): SurfaceRequestContext | undefined {
  const resolution = resolveInvocationContext(options);
  return resolution.kind === "ambient" ? resolution.context : undefined;
}

export function resolveInvocationContext(options: unknown): InvocationContextResolution {
  if (!isRecord(options)) {
    return { kind: "global" };
  }
  // ChatParticipantToolToken is intentionally opaque in the public API, but
  // VS Code 1.120+ constructs its runtime value from sessionResource and
  // workingDirectory. The extension's engine floor matches that provenance.
  // A present but changed token is an unsupported chat host, while an absent
  // token remains the documented global-invocation path.
  const token = options.toolInvocationToken;
  if (token === undefined) {
    return { kind: "global" };
  }
  if (!isRecord(token) || !isUriLike(token.sessionResource)) {
    return { kind: "unsupported" };
  }
  if (token.workingDirectory !== undefined && !isUriLike(token.workingDirectory)) {
    return { kind: "unsupported" };
  }

  let sessionResource: string;
  try {
    sessionResource = token.sessionResource.toString(true);
  } catch {
    return { kind: "unsupported" };
  }
  if (sessionResource.length === 0) {
    return { kind: "unsupported" };
  }

  const context: SurfaceRequestContext = {
    conversation_identity: {
      version: 1,
      issuer: "com.microsoft.vscode",
      id: sessionResource,
    },
  };
  if (
    isUriLike(token.workingDirectory) &&
    token.workingDirectory.scheme === "file" &&
    token.workingDirectory.fsPath.length > 0
  ) {
    context.workspace_root = token.workingDirectory.fsPath;
  }
  return { kind: "ambient", context };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isUriLike(value: unknown): value is UriLike {
  return (
    isRecord(value) &&
    typeof value.scheme === "string" &&
    value.scheme.length > 0 &&
    typeof value.path === "string" &&
    typeof value.fsPath === "string" &&
    typeof value.toString === "function"
  );
}
