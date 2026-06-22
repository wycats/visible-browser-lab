use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerDisplayId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Missing,
    Released,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabLease {
    pub tab_id: TabId,
    pub target_id: String,
    pub owner_session_id: AgentSessionId,
    pub title: Option<String>,
    pub url: Option<String>,
    pub state: LeaseState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedTabSummary {
    pub tab_id: TabId,
    pub target_id: String,
    pub title: String,
    pub url: String,
    pub state: LeaseState,
    pub focused: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalTabSummary {
    pub target_id: String,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_display_id: Option<OwnerDisplayId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_label: Option<String>,
    pub owned_by_caller: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_tab_id: Option<TabId>,
    pub claimable: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalTabGroup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_display_id: Option<OwnerDisplayId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_label: Option<String>,
    pub tabs: Vec<GlobalTabSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalTabInventory {
    pub requested_by: AgentSessionId,
    pub groups: Vec<GlobalTabGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    StartSession,
    ListTabs,
    NewTab,
    ClaimExistingTab,
    ReleaseTab,
    StartChrome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserToolError {
    pub code: BrowserToolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryAction>,
}

impl BrowserToolError {
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::InvalidInput,
            message: message.into(),
            recovery: None,
        }
    }

    pub fn chrome_unavailable(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::ChromeUnavailable,
            message: message.into(),
            recovery: Some(RecoveryAction::StartChrome),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserToolErrorCode {
    ChromeUnavailable,
    UnknownSession,
    UnknownTab,
    TabNotOwned,
    TabNotActive,
    TargetMissing,
    TargetOwned,
    InvalidInput,
    OperationTimeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_tool_error_uses_stable_snake_case_code() {
        let error = BrowserToolError {
            code: BrowserToolErrorCode::TabNotOwned,
            message: "tab is owned by another session".to_string(),
            recovery: Some(RecoveryAction::ListTabs),
        };

        let value = serde_json::to_value(error).unwrap();

        assert_eq!(value["code"], "tab_not_owned");
        assert_eq!(value["recovery"], "list_tabs");
    }

    #[test]
    fn global_summary_omits_action_handle_for_foreign_tabs() {
        let summary = GlobalTabSummary {
            target_id: "target-1".to_string(),
            title: "Example".to_string(),
            url: "https://example.com".to_string(),
            owner_display_id: Some(OwnerDisplayId("owner-1".to_string())),
            owner_label: Some("agent one".to_string()),
            owned_by_caller: false,
            caller_tab_id: None,
            claimable: false,
            focused: false,
        };

        let value = serde_json::to_value(summary).unwrap();

        assert!(value.get("caller_tab_id").is_none());
        assert!(value.get("owner_display_id").is_some());
        assert!(value.get("owner_session_id").is_none());
    }
}
