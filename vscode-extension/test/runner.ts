import * as path from "node:path";
import { runTests } from "@vscode/test-electron";

// Launches a real VS Code extension host with the packaged extension loaded
// from VBL_EXTENSION_PATH (an extracted VSIX's extension/ directory) and runs
// the in-host suite. Invoked by `cargo xtask vsix-smoke --extension-host`.
async function main(): Promise<void> {
  const extensionDevelopmentPath = process.env.VBL_EXTENSION_PATH;
  if (!extensionDevelopmentPath) {
    throw new Error("VBL_EXTENSION_PATH must point at an extracted VSIX extension directory");
  }

  const extensionTestsPath = path.resolve(__dirname, "suite.js");

  await runTests({
    version: "stable",
    extensionDevelopmentPath,
    extensionTestsPath,
    launchArgs: ["--disable-workspace-trust", "--disable-telemetry", "--no-sandbox"],
  });
}

main().catch((error) => {
  console.error("vsix extension-host smoke failed:", error);
  process.exit(1);
});
