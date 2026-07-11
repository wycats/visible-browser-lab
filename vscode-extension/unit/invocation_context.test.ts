import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  extractInvocationContext,
  globalStartSessionError,
  resolveInvocationContext,
  supportsUnsupportedTokenInvocation,
  unsupportedInvocationTokenError,
  withWorkspaceFallback,
} from "../src/invocation_context";

function uri(value: string, fsPath: string, scheme = "file") {
  return {
    scheme,
    path: fsPath,
    fsPath,
    toString: (skipEncoding?: boolean) => {
      assert.equal(skipEncoding, true);
      return value;
    },
  };
}

test("extracts and preserves the session resource URI", () => {
  const context = extractInvocationContext({
    toolInvocationToken: {
      sessionResource: uri("vscode-chat-session://authority/path?query=1#fragment", ""),
      workingDirectory: uri("file:///workspace/project", "/workspace/project"),
    },
  });

  assert.deepEqual(context, {
    conversation_identity: {
      version: 1,
      issuer: "com.microsoft.vscode",
      id: "vscode-chat-session://authority/path?query=1#fragment",
    },
    workspace_root: "/workspace/project",
  });
});

test("keeps identity when a valid token has no working directory", () => {
  const context = extractInvocationContext({
    toolInvocationToken: {
      sessionResource: uri("vscode-chat-session://authority/path", ""),
    },
  });
  assert.equal(context?.conversation_identity?.id, "vscode-chat-session://authority/path");
  assert.equal(context?.workspace_root, undefined);
});

test("rejects missing or changed token shapes", () => {
  assert.equal(extractInvocationContext({ toolInvocationToken: undefined }), undefined);
  assert.equal(
    extractInvocationContext({
      toolInvocationToken: { sessionResource: "vscode-chat-session://authority/path" },
    }),
    undefined,
  );
  assert.equal(
    extractInvocationContext({
      toolInvocationToken: {
        sessionResource: uri("vscode-chat-session://authority/path", ""),
        workingDirectory: "/workspace/project",
      },
    }),
    undefined,
  );
});

test("distinguishes global invocations from unsupported chat tokens", () => {
  assert.deepEqual(resolveInvocationContext({ toolInvocationToken: undefined }), {
    kind: "global",
  });
  assert.deepEqual(
    resolveInvocationContext({ toolInvocationToken: { sessionId: "legacy-session" } }),
    { kind: "unsupported" },
  );
  assert.equal(
    resolveInvocationContext({
      toolInvocationToken: {
        sessionResource: uri("vscode-chat-session://authority/path", ""),
      },
    }).kind,
    "ambient",
  );
});

test("unsupported chat tokens produce actionable redacted guidance", () => {
  assert.equal(
    unsupportedInvocationTokenError("list_tabs"),
    "list_tabs failed with unsupported_host. VS Code did not expose a compatible chat session resource; Visible Browser Lab requires VS Code 1.120 or newer with the supported invocation-token shape. Recovery: update and reload VS Code, or use the explicit MCP/CLI surface",
  );
  assert.equal(supportsUnsupportedTokenInvocation("help"), true);
  assert.equal(supportsUnsupportedTokenInvocation("start_session"), false);
  assert.equal(supportsUnsupportedTokenInvocation("list_tabs"), false);
});

test("global start_session cannot create an inaccessible explicit session", () => {
  assert.equal(
    globalStartSessionError(),
    "start_session failed with session_required. Global VS Code tool invocations have no conversation identity and do not expose explicit session handles. Recovery: invoke Visible Browser Lab from a supported VS Code chat, or use the explicit MCP/CLI surface",
  );
});

test("uses the active workspace fallback only without token-derived context", () => {
  const identityOnly = {
    conversation_identity: {
      version: 1 as const,
      issuer: "com.microsoft.vscode" as const,
      id: "vscode-chat-session://authority/path",
    },
  };
  assert.deepEqual(withWorkspaceFallback(identityOnly, "/active/workspace"), identityOnly);
  assert.deepEqual(withWorkspaceFallback(undefined, "/active/workspace"), {
    workspace_root: "/active/workspace",
  });
  assert.deepEqual(withWorkspaceFallback(undefined, undefined), {});
});
