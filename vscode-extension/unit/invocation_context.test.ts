import { strict as assert } from "node:assert";
import { test } from "node:test";

import { extractInvocationContext, withWorkspaceFallback } from "../src/invocation_context";

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
