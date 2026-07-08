import * as vscode from "vscode";
import { spawn } from "node:child_process";
import * as path from "node:path";

import { extractInvocationContext, type SurfaceRequestContext } from "./invocation_context";

interface ToolContribution {
  name: string;
  displayName?: string;
  userDescription?: string;
}

interface BrowserToolResult {
  ok: boolean;
  result?: unknown;
  error?: {
    code?: string;
    message?: string;
    recovery?: string;
  };
}

const TOOL_PREFIX = "visible_browser_lab_";

export function activate(context: vscode.ExtensionContext): void {
  const tools = contributedTools(context);
  for (const tool of tools) {
    context.subscriptions.push(
      vscode.lm.registerTool(tool.name, new BrowserLanguageModelTool(context, tool)),
    );
  }
}

export function deactivate(): void {}

class BrowserLanguageModelTool implements vscode.LanguageModelTool<Record<string, unknown>> {
  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly contribution: ToolContribution,
  ) {}

  prepareInvocation(
    options: vscode.LanguageModelToolInvocationPrepareOptions<Record<string, unknown>>,
  ): vscode.ProviderResult<vscode.PreparedToolInvocation> {
    const method = browserMethod(this.contribution.name);
    const displayName = this.contribution.displayName ?? method;
    const invocationMessage = invocationMessageFor(method, displayName);
    const confirmationMessages = confirmationFor(method, options.input);
    return confirmationMessages
      ? { invocationMessage, confirmationMessages }
      : { invocationMessage };
  }

  async invoke(
    options: vscode.LanguageModelToolInvocationOptions<Record<string, unknown>>,
    token: vscode.CancellationToken,
  ): Promise<vscode.LanguageModelToolResult> {
    const method = browserMethod(this.contribution.name);
    const output = await invokeSurfaceCall(
      this.context,
      method,
      options.input ?? {},
      extractInvocationContext(options),
      token,
    );
    if (!output.ok) {
      throw new Error(formatToolError(method, output.error));
    }

    // LanguageModelTextPart is stable at the declared engine floor (1.105).
    return new vscode.LanguageModelToolResult([
      new vscode.LanguageModelTextPart(JSON.stringify(output.result ?? null)),
    ]);
  }
}

function contributedTools(context: vscode.ExtensionContext): ToolContribution[] {
  const packageJson = extensionPackageJson(context.extension.packageJSON);
  return packageJson.contributes?.languageModelTools ?? [];
}

function extensionPackageJson(value: unknown): {
  contributes?: { languageModelTools?: ToolContribution[] };
} {
  if (!isRecord(value)) {
    return {};
  }
  const contributes = value.contributes;
  if (!isRecord(contributes)) {
    return {};
  }
  const languageModelTools = contributes.languageModelTools;
  if (!Array.isArray(languageModelTools)) {
    return { contributes: {} };
  }
  return {
    contributes: {
      languageModelTools: languageModelTools.filter(isToolContribution),
    },
  };
}

function isToolContribution(value: unknown): value is ToolContribution {
  return isRecord(value) && typeof value.name === "string";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function browserMethod(toolName: string): string {
  return toolName.startsWith(TOOL_PREFIX) ? toolName.slice(TOOL_PREFIX.length) : toolName;
}

function invocationMessageFor(method: string, displayName: string): string {
  switch (method) {
    case "start_session":
      return "Starting a visible browser session";
    case "snapshot":
      return "Capturing a browser snapshot";
    case "screenshot":
      return "Capturing a browser screenshot";
    case "navigate":
      return "Navigating the owned browser tab";
    case "click":
      return "Clicking a browser element";
    case "fill":
    case "fill_form":
      return "Filling browser form controls";
    case "wait_for":
      return "Waiting for browser state";
    default:
      return `Running ${displayName}`;
  }
}

function confirmationFor(
  method: string,
  input: Record<string, unknown>,
): vscode.LanguageModelToolConfirmationMessages | undefined {
  switch (method) {
    case "claim_tab":
      return {
        title: "Claim browser tab?",
        message: `Claim target ${stringInput(input, "target_id") ?? "(unknown target)"} for this agent session.`,
      };
    case "close_tab":
      return {
        title: "Close browser tab?",
        message: `Close owned tab ${stringInput(input, "tab_id") ?? "(unknown tab)"}.`,
      };
    case "release_tab":
      return {
        title: "Release browser tab?",
        message: `Release owned tab ${stringInput(input, "tab_id") ?? "(unknown tab)"} without closing it.`,
      };
    case "focus_tab":
      return {
        title: "Bring Chrome forward?",
        message: `Focus owned tab ${stringInput(input, "tab_id") ?? "(unknown tab)"} for manual inspection or handoff.`,
      };
    default:
      return undefined;
  }
}

function stringInput(input: Record<string, unknown>, key: string): string | undefined {
  const value = input[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

async function invokeSurfaceCall(
  context: vscode.ExtensionContext,
  method: string,
  input: Record<string, unknown>,
  invocationContext: SurfaceRequestContext | undefined,
  token: vscode.CancellationToken,
): Promise<BrowserToolResult> {
  const binary = resolveBinary(context);
  const workspaceRoot = invocationContext?.workspace_root ?? activeWorkspaceRoot();
  const args = ["surface", "call", method, "--request-envelope-version", "1"];
  const requestContext: SurfaceRequestContext = invocationContext ? { ...invocationContext } : {};
  if (workspaceRoot) {
    requestContext.workspace_root = workspaceRoot;
  }
  const envelope = { arguments: input, context: requestContext };

  const env = runtimeEnvironment();
  const stdout = await runProcess(binary, args, JSON.stringify(envelope), env, token);
  return browserToolResult(JSON.parse(stdout));
}

function activeWorkspaceRoot(): string | undefined {
  // Prefer the folder that owns the active editor so multi-root windows scope
  // uploads and artifact exports to the project the user is working in.
  const activeDocument = vscode.window.activeTextEditor?.document.uri;
  if (activeDocument) {
    const folder = vscode.workspace.getWorkspaceFolder(activeDocument);
    if (folder) {
      return folder.uri.fsPath;
    }
  }
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

function browserToolResult(value: unknown): BrowserToolResult {
  if (!isRecord(value) || typeof value.ok !== "boolean") {
    throw new Error("visible-browser-lab returned an invalid tool result");
  }
  if (value.ok) {
    return { ok: true, result: value.result };
  }
  return { ok: false, error: browserToolError(value.error) };
}

function browserToolError(value: unknown): BrowserToolResult["error"] {
  if (!isRecord(value)) {
    return undefined;
  }
  return {
    code: typeof value.code === "string" ? value.code : undefined,
    message: typeof value.message === "string" ? value.message : undefined,
    recovery: typeof value.recovery === "string" ? value.recovery : undefined,
  };
}

function resolveBinary(context: vscode.ExtensionContext): string {
  const configured = vscode.workspace
    .getConfiguration("visibleBrowserLab")
    .get<string>("binaryPath")
    ?.trim();
  if (configured) {
    return configured;
  }

  const packaged = path.join(context.extensionPath, "bin", binaryName());
  return packaged;
}

function binaryName(): string {
  return process.platform === "win32" ? "visible-browser-lab-mcp.exe" : "visible-browser-lab-mcp";
}

function runtimeEnvironment(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  copySetting(env, "stateDir", "VISIBLE_BROWSER_LAB_STATE_DIR");
  copySetting(env, "cdpEndpoint", "VISIBLE_BROWSER_CDP_ENDPOINT");
  copySetting(env, "cdpPort", "VISIBLE_BROWSER_CDP_PORT");
  copySetting(env, "chromePath", "VISIBLE_BROWSER_LAB_CHROME_PATH");
  return env;
}

function copySetting(env: NodeJS.ProcessEnv, setting: string, variable: string): void {
  const value = vscode.workspace.getConfiguration("visibleBrowserLab").get<string>(setting)?.trim();
  if (value) {
    env[variable] = value;
  }
}

function runProcess(
  binary: string,
  args: string[],
  stdin: string,
  env: NodeJS.ProcessEnv,
  token: vscode.CancellationToken,
): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, { env, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";

    // Cancellation ends this wrapper process and reports cancellation to VS
    // Code. A browser action the broker has already dispatched runs to
    // completion; lease ownership keeps its effects scoped to this session's
    // tabs, and a broker-level cancel channel is tracked as RFC 00006
    // follow-up work.
    const cancellation = token.onCancellationRequested(() => {
      child.kill();
      reject(new vscode.CancellationError());
    });

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", (error) => {
      cancellation.dispose();
      reject(error);
    });
    child.on("close", (code: number | null) => {
      cancellation.dispose();
      if (code === 0) {
        resolve(stdout);
      } else {
        reject(new Error(`visible-browser-lab exited with code ${code}: ${stderr.trim()}`));
      }
    });
    child.stdin.end(stdin);
  });
}

function formatToolError(method: string, error: BrowserToolResult["error"]): string {
  if (!error) {
    return `${method} failed without an error payload`;
  }

  const parts = [
    error.code ? `${method} failed with ${error.code}` : `${method} failed`,
    error.message,
    error.recovery ? `Recovery: ${error.recovery}` : undefined,
  ].filter(Boolean);
  return parts.join(". ");
}
