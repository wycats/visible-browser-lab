export interface ConversationIdentityPayload {
  version: 1;
  issuer: "com.microsoft.vscode";
  id: string;
}

export interface SurfaceRequestContext {
  conversation_identity?: ConversationIdentityPayload;
  workspace_root?: string;
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
  const token = options.toolInvocationToken;
  if (!isRecord(token) || !isUriLike(token.sessionResource)) {
    return undefined;
  }
  if (token.workingDirectory !== undefined && !isUriLike(token.workingDirectory)) {
    return undefined;
  }

  let sessionResource: string;
  try {
    sessionResource = token.sessionResource.toString();
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
