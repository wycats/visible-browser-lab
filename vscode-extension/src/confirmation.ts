import type * as vscode from "vscode";

export function confirmationFor(
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
      if (input.leave_visible === true) {
        const instruction = stringInput(input, "user_instruction")?.trim();
        const instructionSummary = instruction
          ? JSON.stringify(instruction)
          : "(missing; this request will be rejected)";
        return {
          title: "Leave browser tab visible?",
          message: `Release owned tab ${stringInput(input, "tab_id") ?? "(unknown tab)"} and preserve it after this session expires. User instruction: ${instructionSummary}.`,
        };
      }
      return {
        title: "Release browser tab?",
        message: `Release owned tab ${stringInput(input, "tab_id") ?? "(unknown tab)"}; a VBL-created target remains eligible for expiry cleanup.`,
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
