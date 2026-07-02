import * as vscode from "vscode";

// Runs inside the extension host. Verifies that the packaged extension
// activates and registers every contributed language model tool, and that a
// non-browser tool invocation round-trips through the packaged binary.
export async function run(): Promise<void> {
  const extension = vscode.extensions.getExtension("wycats.visible-browser-lab");
  if (!extension) {
    throw new Error("extension wycats.visible-browser-lab is not installed in the test host");
  }

  await extension.activate();

  const packageJson = extension.packageJSON as {
    contributes?: { languageModelTools?: Array<{ name: string }> };
  };
  const contributed = packageJson.contributes?.languageModelTools ?? [];
  if (contributed.length === 0) {
    throw new Error("packaged extension contributes no language model tools");
  }

  const registered = new Set(vscode.lm.tools.map((tool) => tool.name));
  const missing = contributed.filter((tool) => !registered.has(tool.name));
  if (missing.length > 0) {
    throw new Error(
      `contributed tools are not registered in vscode.lm.tools: ${missing.map((tool) => tool.name).join(", ")}`,
    );
  }

  // help is the one production tool that answers without starting a browser.
  const result = await vscode.lm.invokeTool("visible_browser_lab_help", {
    input: { topic: "workflow" },
    toolInvocationToken: undefined,
  });
  const text = result.content
    .map((part) => (part instanceof vscode.LanguageModelTextPart ? part.value : ""))
    .join("");
  const payload = JSON.parse(text) as { preferred?: { tool?: string } };
  if (!payload.preferred?.tool) {
    throw new Error(`help returned an unexpected payload: ${text.slice(0, 200)}`);
  }

  console.log(
    `extension-host smoke: ${contributed.length} tools registered; help preferred ${payload.preferred.tool}`,
  );
}
