export interface ConversationIdentityPayload {
  version: 1;
  issuer: "com.microsoft.vscode";
  id: string;
}

export interface SurfaceRequestContext {
  conversation_identity?: ConversationIdentityPayload;
  workspace_root?: string;
}

export function withWorkspaceFallback(
  context: SurfaceRequestContext | undefined,
  fallbackWorkspaceRoot: string | undefined,
): SurfaceRequestContext {
  if (context) {
    return { ...context };
  }
  return fallbackWorkspaceRoot ? { workspace_root: fallbackWorkspaceRoot } : {};
}

interface UriLike {
  scheme: string;
  path: string;
  fsPath: string;
  toString(skipEncoding?: boolean): string;
}

export function extractInvocationContext(options: unknown): SurfaceRequestContext | undefined {
  if (!isRecord(options)) {
    return undefined;
  }
  // ChatParticipantToolToken is intentionally opaque in the public API, but
  // VS Code 1.128 constructs its runtime value from sessionResource and
  // workingDirectory. This guarded compatibility bridge fails closed when
  // that private shape is absent or changes; the stable direct API can replace
  // it once it reaches the extension's engine floor.
  const token = options.toolInvocationToken;
  if (!isRecord(token) || !isUriLike(token.sessionResource)) {
    return undefined;
  }
  if (token.workingDirectory !== undefined && !isUriLike(token.workingDirectory)) {
    return undefined;
  }

  let sessionResource: string;
  try {
    sessionResource = token.sessionResource.toString(true);
  } catch {
    return undefined;
  }
  if (sessionResource.length === 0) {
    return undefined;
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
  return context;
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
