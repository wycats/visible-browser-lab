use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct AgentSessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TabId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct OwnerDisplayId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub agent_session_id: AgentSessionId,
    pub owner_display_id: OwnerDisplayId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<PathBuf>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

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
pub struct TabSnapshot {
    pub target_id: String,
    pub title: String,
    pub url: String,
    pub focused: bool,
}

impl TabSnapshot {
    pub fn new(
        target_id: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
        focused: bool,
    ) -> Self {
        Self {
            target_id: target_id.into(),
            title: title.into(),
            url: url.into(),
            focused,
        }
    }
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
    FocusTab,
    StartChrome,
    Snapshot,
    WaitFor,
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
    SessionExpired,
    UnknownTab,
    TabNotOwned,
    TabNotActive,
    TargetMissing,
    TargetOwned,
    InvalidInput,
    OperationTimeout,
    FocusRequired,
    ElementNotFound,
    ElementAmbiguous,
    ElementStale,
    ElementNotActionable,
    ArtifactNotFound,
    ArtifactError,
    WorkspaceUnavailable,
    PathOutsideWorkspace,
}

impl BrowserToolError {
    pub fn unknown_session(session_id: &AgentSessionId) -> Self {
        Self {
            code: BrowserToolErrorCode::UnknownSession,
            message: format!("unknown agent session `{}`", session_id.0),
            recovery: Some(RecoveryAction::StartSession),
        }
    }

    pub fn session_expired(session_id: &AgentSessionId, idle: Duration) -> Self {
        Self {
            code: BrowserToolErrorCode::SessionExpired,
            message: format!(
                "agent session `{}` expired after {}s of inactivity; its tabs were released and are claimable",
                session_id.0,
                idle.as_secs()
            ),
            recovery: Some(RecoveryAction::StartSession),
        }
    }

    pub fn unknown_tab(tab_id: &TabId) -> Self {
        Self {
            code: BrowserToolErrorCode::UnknownTab,
            message: format!("unknown tab `{}`", tab_id.0),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn tab_not_owned(tab_id: &TabId) -> Self {
        Self {
            code: BrowserToolErrorCode::TabNotOwned,
            message: format!("tab `{}` is not owned by this session", tab_id.0),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn tab_not_active(tab_id: &TabId, state: &LeaseState) -> Self {
        Self {
            code: BrowserToolErrorCode::TabNotActive,
            message: format!("tab `{}` is `{state:?}`, not `active`", tab_id.0),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn target_missing(tab_id: &TabId) -> Self {
        Self {
            code: BrowserToolErrorCode::TargetMissing,
            message: format!(
                "tab `{}` no longer has a visible Chrome target; release it and choose another tab",
                tab_id.0
            ),
            recovery: Some(RecoveryAction::ReleaseTab),
        }
    }

    pub fn target_missing_for_target(target_id: &str) -> Self {
        Self {
            code: BrowserToolErrorCode::TargetMissing,
            message: format!("Chrome target `{target_id}` is not a visible page target"),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn target_owned(target_id: &str) -> Self {
        Self {
            code: BrowserToolErrorCode::TargetOwned,
            message: format!("Chrome target `{target_id}` is already leased"),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn element_not_found(target: &str) -> Self {
        Self {
            code: BrowserToolErrorCode::ElementNotFound,
            message: format!("no element matched `{target}`"),
            recovery: Some(RecoveryAction::Snapshot),
        }
    }

    pub fn element_ambiguous(target: &str, count: usize) -> Self {
        Self {
            code: BrowserToolErrorCode::ElementAmbiguous,
            message: format!("element target `{target}` matched {count} nodes"),
            recovery: Some(RecoveryAction::Snapshot),
        }
    }

    pub fn element_stale(reference: &str) -> Self {
        Self {
            code: BrowserToolErrorCode::ElementStale,
            message: format!(
                "element reference `{reference}` is not valid for the active document"
            ),
            recovery: Some(RecoveryAction::Snapshot),
        }
    }

    pub fn element_not_actionable(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::ElementNotActionable,
            message: message.into(),
            recovery: Some(RecoveryAction::WaitFor),
        }
    }

    pub fn operation_timeout(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::OperationTimeout,
            message: message.into(),
            recovery: Some(RecoveryAction::ListTabs),
        }
    }

    pub fn focus_required(tab_id: &TabId) -> Self {
        Self {
            code: BrowserToolErrorCode::FocusRequired,
            message: format!(
                "tab `{}` must have browser focus before dispatching native input",
                tab_id.0
            ),
            recovery: Some(RecoveryAction::FocusTab),
        }
    }

    pub fn artifact_not_found(artifact_id: &str) -> Self {
        Self {
            code: BrowserToolErrorCode::ArtifactNotFound,
            message: format!("artifact `{artifact_id}` is not owned by this session"),
            recovery: None,
        }
    }

    pub fn artifact_error(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::ArtifactError,
            message: message.into(),
            recovery: None,
        }
    }

    pub fn workspace_unavailable(message: impl Into<String>) -> Self {
        Self {
            code: BrowserToolErrorCode::WorkspaceUnavailable,
            message: message.into(),
            recovery: None,
        }
    }

    pub fn path_outside_workspace(path: &Path) -> Self {
        Self {
            code: BrowserToolErrorCode::PathOutsideWorkspace,
            message: format!(
                "artifact export path `{}` is outside the session workspace",
                path.display()
            ),
            recovery: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct LeaseRegistry {
    sessions: HashMap<AgentSessionId, BrowserSession>,
    leases: HashMap<TabId, TabLease>,
    active_target_owners: HashMap<String, TabId>,
    /// Sessions removed by the expiry sweep, with how long each sat idle.
    /// Lets a call on an expired session fail with `session_expired` ("you
    /// had this and waited too long") instead of `unknown_session` ("you
    /// never had this"). Bounded by sessions expired within one broker
    /// lifetime.
    expired_sessions: HashMap<AgentSessionId, u64>,
}

/// What the expiry sweep did to one session, so the broker can reclaim the
/// session-private state that lives outside the registry.
pub struct ExpiredSession {
    pub session_id: AgentSessionId,
    pub idle: Duration,
    pub released_tab_ids: Vec<TabId>,
}

impl LeaseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_session(&mut self, label: Option<String>) -> BrowserSession {
        self.start_session_with_workspace(label, None)
    }

    pub fn start_session_with_workspace(
        &mut self,
        label: Option<String>,
        workspace_root: Option<PathBuf>,
    ) -> BrowserSession {
        let now = now_ms();
        let session = BrowserSession {
            agent_session_id: AgentSessionId(prefixed_uuid("session")),
            owner_display_id: OwnerDisplayId(prefixed_uuid("owner")),
            label,
            workspace_root,
            created_at_ms: now,
            updated_at_ms: now,
        };

        self.sessions
            .insert(session.agent_session_id.clone(), session.clone());

        session
    }

    pub fn session(&self, session_id: &AgentSessionId) -> Option<&BrowserSession> {
        self.sessions.get(session_id)
    }

    /// Test support: rewind a session's last-activity timestamp so tests can
    /// exercise staleness without waiting out real clock time.
    #[cfg(test)]
    pub(crate) fn backdate_session(&mut self, session_id: &AgentSessionId, by_ms: u64) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.updated_at_ms = session.updated_at_ms.saturating_sub(by_ms);
        }
    }

    /// Whether any live session exists. Every live session defers the
    /// broker's idle exit; the expiry sweep is what turns an abandoned
    /// session from live to gone.
    pub fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Expire every session whose last touch is older than `ttl`. Tab leases
    /// are released, never closed: the Chrome tab is the user's visible
    /// state, and a bookkeeping deadline is not grounds for destroying it.
    /// The session record itself is removed and tombstoned so later calls
    /// naming it get `session_expired` rather than `unknown_session`.
    pub fn expire_sessions(&mut self, ttl: Duration, now_ms: u64) -> Vec<ExpiredSession> {
        let ttl_ms = ttl.as_millis() as u64;
        let expired_ids: Vec<AgentSessionId> = self
            .sessions
            .values()
            .filter(|session| now_ms.saturating_sub(session.updated_at_ms) > ttl_ms)
            .map(|session| session.agent_session_id.clone())
            .collect();

        expired_ids
            .into_iter()
            .map(|session_id| {
                let session = self
                    .sessions
                    .remove(&session_id)
                    .expect("expired session id was just collected from the table");
                let idle_ms = now_ms.saturating_sub(session.updated_at_ms);

                let mut released = Vec::new();
                for lease in self.leases.values_mut() {
                    if lease.owner_session_id == session_id
                        && matches!(lease.state, LeaseState::Active | LeaseState::Missing)
                    {
                        lease.state = LeaseState::Released;
                        lease.updated_at_ms = now_ms;
                        released.push((lease.tab_id.clone(), lease.target_id.clone()));
                    }
                }
                for (tab_id, target_id) in &released {
                    self.remove_active_target_if_matches(target_id, tab_id);
                }

                self.expired_sessions.insert(session_id.clone(), idle_ms);

                ExpiredSession {
                    session_id,
                    idle: Duration::from_millis(idle_ms),
                    released_tab_ids: released.into_iter().map(|(tab_id, _)| tab_id).collect(),
                }
            })
            .collect()
    }

    pub fn ensure_session(&self, session_id: &AgentSessionId) -> Result<(), BrowserToolError> {
        self.require_session(session_id)?;
        Ok(())
    }

    /// Refresh a session's last-touch time. Called at the broker's dispatch
    /// choke point, so any request that names a session counts as using it.
    /// Unknown session ids are ignored; the per-operation handler is the one
    /// that reports them.
    pub fn touch(&mut self, session_id: &AgentSessionId) {
        self.touch_session(session_id, now_ms());
    }

    pub fn lease_tab(
        &mut self,
        session_id: &AgentSessionId,
        target: TabSnapshot,
    ) -> Result<OwnedTabSummary, BrowserToolError> {
        self.require_session(session_id)?;
        let focused = target.focused;

        if self.active_lease_for_target(&target.target_id).is_some() {
            return Err(BrowserToolError::target_owned(&target.target_id));
        }

        let lease = self.insert_active_lease(session_id, target);
        Ok(owned_summary(&lease, focused))
    }

    pub fn claim_tab(
        &mut self,
        session_id: &AgentSessionId,
        target: TabSnapshot,
        takeover: bool,
        user_instruction: Option<&str>,
    ) -> Result<OwnedTabSummary, BrowserToolError> {
        self.require_session(session_id)?;
        let focused = target.focused;

        if let Some(existing) = self.active_lease_for_target(&target.target_id).cloned() {
            if !takeover {
                return Err(BrowserToolError::target_owned(&target.target_id));
            }

            let instruction = user_instruction.unwrap_or("").trim();
            if instruction.is_empty() {
                return Err(BrowserToolError::invalid_input(
                    "takeover requires a non-empty user instruction",
                ));
            }

            self.leases.remove(&existing.tab_id);
            self.remove_active_target_if_matches(&existing.target_id, &existing.tab_id);
        }

        let lease = self.insert_active_lease(session_id, target);
        Ok(owned_summary(&lease, focused))
    }

    pub fn list_owned_tabs(
        &self,
        session_id: &AgentSessionId,
        focused_target_id: Option<&str>,
    ) -> Result<Vec<OwnedTabSummary>, BrowserToolError> {
        self.require_session(session_id)?;

        let mut tabs = self
            .leases
            .values()
            .filter(|lease| {
                lease.owner_session_id == *session_id
                    && matches!(lease.state, LeaseState::Active | LeaseState::Missing)
            })
            .map(|lease| owned_summary(lease, focused_target_id == Some(lease.target_id.as_str())))
            .collect::<Vec<_>>();

        tabs.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.tab_id.0.cmp(&right.tab_id.0))
        });

        Ok(tabs)
    }

    pub fn require_active_owned(
        &mut self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
        target_exists: bool,
    ) -> Result<TabLease, BrowserToolError> {
        self.require_owned_tab(session_id, tab_id)?;

        let lease = self
            .leases
            .get(tab_id)
            .expect("owned tab was checked before active-state validation");

        match lease.state {
            LeaseState::Active => {}
            LeaseState::Missing => return Err(BrowserToolError::target_missing(tab_id)),
            LeaseState::Released | LeaseState::Closed => {
                return Err(BrowserToolError::tab_not_active(tab_id, &lease.state));
            }
        }

        if !target_exists {
            self.mark_missing(tab_id)?;
            return Err(BrowserToolError::target_missing(tab_id));
        }

        Ok(self
            .leases
            .get(tab_id)
            .expect("active owned tab should still exist")
            .clone())
    }

    pub fn owned_lease(
        &self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
    ) -> Result<TabLease, BrowserToolError> {
        self.require_owned_tab(session_id, tab_id)?;
        Ok(self
            .leases
            .get(tab_id)
            .expect("owned tab should exist")
            .clone())
    }

    pub fn update_tab_snapshot(
        &mut self,
        tab_id: &TabId,
        target: TabSnapshot,
    ) -> Result<TabLease, BrowserToolError> {
        let now = now_ms();

        {
            let lease = self
                .leases
                .get_mut(tab_id)
                .ok_or_else(|| BrowserToolError::unknown_tab(tab_id))?;
            lease.target_id = target.target_id;
            lease.title = Some(target.title);
            lease.url = Some(target.url);
            lease.updated_at_ms = now;
        }

        Ok(self
            .leases
            .get(tab_id)
            .expect("updated tab should still be tracked")
            .clone())
    }

    pub fn release_tab(
        &mut self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
    ) -> Result<TabLease, BrowserToolError> {
        self.transition_owned_tab(session_id, tab_id, LeaseState::Released)
    }

    pub fn close_tab_mark(
        &mut self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
    ) -> Result<TabLease, BrowserToolError> {
        self.transition_owned_tab(session_id, tab_id, LeaseState::Closed)
    }

    pub fn mark_missing(&mut self, tab_id: &TabId) -> Result<TabLease, BrowserToolError> {
        let now = now_ms();
        let target_id;

        {
            let lease = self
                .leases
                .get_mut(tab_id)
                .ok_or_else(|| BrowserToolError::unknown_tab(tab_id))?;
            target_id = lease.target_id.clone();

            if lease.state == LeaseState::Active {
                lease.state = LeaseState::Missing;
                lease.updated_at_ms = now;
            }
        }

        self.remove_active_target_if_matches(&target_id, tab_id);

        Ok(self
            .leases
            .get(tab_id)
            .expect("missing tab should still be tracked")
            .clone())
    }

    pub fn mark_missing_by_target(&mut self, target_id: &str) -> Option<TabLease> {
        let tab_id = self.active_target_owners.get(target_id)?.clone();
        self.mark_missing(&tab_id).ok()
    }

    pub fn mark_missing_targets_not_in<I>(&mut self, visible_target_ids: I) -> Vec<TabLease>
    where
        I: IntoIterator<Item = String>,
    {
        let visible_target_ids = visible_target_ids.into_iter().collect::<HashSet<_>>();
        let missing_tab_ids = self
            .active_target_owners
            .iter()
            .filter(|(target_id, _)| !visible_target_ids.contains(*target_id))
            .map(|(_, tab_id)| tab_id.clone())
            .collect::<Vec<_>>();

        missing_tab_ids
            .into_iter()
            .filter_map(|tab_id| self.mark_missing(&tab_id).ok())
            .collect()
    }

    pub fn global_inventory<I>(
        &self,
        requested_by: &AgentSessionId,
        visible_tabs: I,
    ) -> Result<GlobalTabInventory, BrowserToolError>
    where
        I: IntoIterator<Item = TabSnapshot>,
    {
        self.require_session(requested_by)?;

        let mut groups: Vec<GlobalTabGroup> = Vec::new();

        for visible_tab in visible_tabs {
            let active_lease = self.active_lease_for_target(&visible_tab.target_id);
            let owner = active_lease.and_then(|lease| self.sessions.get(&lease.owner_session_id));
            let owned_by_caller =
                active_lease.is_some_and(|lease| lease.owner_session_id == *requested_by);
            let caller_tab_id = active_lease
                .filter(|_| owned_by_caller)
                .map(|lease| lease.tab_id.clone());
            let owner_display_id = owner.map(|session| session.owner_display_id.clone());
            let owner_label = owner.and_then(|session| session.label.clone());
            let claimable = active_lease.is_none();

            let summary = GlobalTabSummary {
                target_id: visible_tab.target_id,
                title: visible_tab.title,
                url: visible_tab.url,
                owner_display_id: owner_display_id.clone(),
                owner_label: owner_label.clone(),
                owned_by_caller,
                caller_tab_id,
                claimable,
                focused: visible_tab.focused,
            };

            push_global_summary(&mut groups, owner_display_id, owner_label, summary);
        }

        Ok(GlobalTabInventory {
            requested_by: requested_by.clone(),
            groups,
        })
    }

    fn insert_active_lease(
        &mut self,
        session_id: &AgentSessionId,
        target: TabSnapshot,
    ) -> TabLease {
        let now = now_ms();
        let lease = TabLease {
            tab_id: TabId(prefixed_uuid("tab")),
            target_id: target.target_id,
            owner_session_id: session_id.clone(),
            title: Some(target.title),
            url: Some(target.url),
            state: LeaseState::Active,
            created_at_ms: now,
            updated_at_ms: now,
        };

        self.active_target_owners
            .insert(lease.target_id.clone(), lease.tab_id.clone());
        self.leases.insert(lease.tab_id.clone(), lease.clone());

        lease
    }

    fn transition_owned_tab(
        &mut self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
        next_state: LeaseState,
    ) -> Result<TabLease, BrowserToolError> {
        self.require_owned_tab(session_id, tab_id)?;

        let now = now_ms();
        let target_id;

        {
            let lease = self
                .leases
                .get_mut(tab_id)
                .expect("owned tab should exist before transition");
            match lease.state {
                LeaseState::Active | LeaseState::Missing => {}
                LeaseState::Released | LeaseState::Closed => {
                    return Err(BrowserToolError::tab_not_active(tab_id, &lease.state));
                }
            }

            target_id = lease.target_id.clone();
            lease.state = next_state;
            lease.updated_at_ms = now;
        }

        self.remove_active_target_if_matches(&target_id, tab_id);

        Ok(self
            .leases
            .get(tab_id)
            .expect("transitioned tab should still be tracked")
            .clone())
    }

    fn require_owned_tab(
        &self,
        session_id: &AgentSessionId,
        tab_id: &TabId,
    ) -> Result<(), BrowserToolError> {
        self.require_session(session_id)?;

        let lease = self
            .leases
            .get(tab_id)
            .ok_or_else(|| BrowserToolError::unknown_tab(tab_id))?;

        if lease.owner_session_id != *session_id {
            return Err(BrowserToolError::tab_not_owned(tab_id));
        }

        Ok(())
    }

    fn require_session(
        &self,
        session_id: &AgentSessionId,
    ) -> Result<&BrowserSession, BrowserToolError> {
        if let Some(session) = self.sessions.get(session_id) {
            return Ok(session);
        }
        if let Some(idle_ms) = self.expired_sessions.get(session_id) {
            return Err(BrowserToolError::session_expired(
                session_id,
                Duration::from_millis(*idle_ms),
            ));
        }
        Err(BrowserToolError::unknown_session(session_id))
    }

    fn active_lease_for_target(&self, target_id: &str) -> Option<&TabLease> {
        self.active_target_owners
            .get(target_id)
            .and_then(|tab_id| self.leases.get(tab_id))
            .filter(|lease| lease.state == LeaseState::Active)
    }

    fn remove_active_target_if_matches(&mut self, target_id: &str, tab_id: &TabId) {
        if self.active_target_owners.get(target_id) == Some(tab_id) {
            self.active_target_owners.remove(target_id);
        }
    }

    fn touch_session(&mut self, session_id: &AgentSessionId, updated_at_ms: u64) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.updated_at_ms = updated_at_ms;
        }
    }
}

fn owned_summary(lease: &TabLease, focused: bool) -> OwnedTabSummary {
    OwnedTabSummary {
        tab_id: lease.tab_id.clone(),
        target_id: lease.target_id.clone(),
        title: lease.title.clone().unwrap_or_default(),
        url: lease.url.clone().unwrap_or_default(),
        state: lease.state.clone(),
        focused,
        created_at_ms: lease.created_at_ms,
        updated_at_ms: lease.updated_at_ms,
    }
}

fn push_global_summary(
    groups: &mut Vec<GlobalTabGroup>,
    owner_display_id: Option<OwnerDisplayId>,
    owner_label: Option<String>,
    summary: GlobalTabSummary,
) {
    if let Some(group) = groups.iter_mut().find(|group| {
        group.owner_display_id == owner_display_id && group.owner_label == owner_label
    }) {
        group.tabs.push(summary);
        return;
    }

    groups.push(GlobalTabGroup {
        owner_display_id,
        owner_label,
        tabs: vec![summary],
    });
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn prefixed_uuid(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(target_id: &str) -> TabSnapshot {
        TabSnapshot::new(
            target_id,
            format!("Title for {target_id}"),
            format!("https://example.com/{target_id}"),
            false,
        )
    }

    fn focused_snapshot(target_id: &str) -> TabSnapshot {
        TabSnapshot::new(
            target_id,
            format!("Title for {target_id}"),
            format!("https://example.com/{target_id}"),
            true,
        )
    }

    #[test]
    fn empty_registry_has_no_sessions() {
        let registry = LeaseRegistry::new();
        assert!(!registry.has_sessions());
    }

    #[test]
    fn fresh_session_survives_the_expiry_sweep() {
        let mut registry = LeaseRegistry::new();
        registry.start_session(Some("agent".to_string()));

        let expired = registry.expire_sessions(Duration::from_secs(3_600), now_ms());

        assert!(expired.is_empty());
        assert!(registry.has_sessions());
    }

    #[test]
    fn stale_session_expires_and_its_leases_are_released_not_closed() {
        let mut registry = LeaseRegistry::new();
        let session = registry.start_session(Some("agent".to_string()));
        let summary = registry
            .lease_tab(&session.agent_session_id, focused_snapshot("target-a"))
            .unwrap();

        registry.backdate_session(&session.agent_session_id, 2 * 3_600_000);
        let expired = registry.expire_sessions(Duration::from_secs(3_600), now_ms());

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].session_id, session.agent_session_id);
        assert_eq!(expired[0].released_tab_ids, vec![summary.tab_id.clone()]);
        assert!(!registry.has_sessions());
        // Released, not gone: the lease record survives as a claimable tab.
        let lease = registry.leases.get(&summary.tab_id).unwrap();
        assert!(matches!(lease.state, LeaseState::Released));
    }

    #[test]
    fn expired_session_reports_session_expired_not_unknown() {
        let mut registry = LeaseRegistry::new();
        let session = registry.start_session(Some("agent".to_string()));
        registry.backdate_session(&session.agent_session_id, 2 * 3_600_000);
        registry.expire_sessions(Duration::from_secs(3_600), now_ms());

        let error = registry
            .ensure_session(&session.agent_session_id)
            .unwrap_err();
        assert!(matches!(error.code, BrowserToolErrorCode::SessionExpired));
        assert_eq!(error.recovery, Some(RecoveryAction::StartSession));

        let never_seen = registry
            .ensure_session(&AgentSessionId("session-nonexistent".to_string()))
            .unwrap_err();
        assert!(matches!(
            never_seen.code,
            BrowserToolErrorCode::UnknownSession
        ));
    }

    #[test]
    fn restart_after_expiry_mints_a_fresh_session_and_keeps_the_tombstone() {
        let mut registry = LeaseRegistry::new();
        let session = registry.start_session(Some("agent".to_string()));
        registry.backdate_session(&session.agent_session_id, 2 * 3_600_000);
        registry.expire_sessions(Duration::from_secs(3_600), now_ms());

        let restarted = registry.start_session(Some("agent".to_string()));

        assert_ne!(restarted.agent_session_id, session.agent_session_id);
        assert!(registry.ensure_session(&restarted.agent_session_id).is_ok());
        // The old id still answers session_expired, not unknown_session.
        let error = registry
            .ensure_session(&session.agent_session_id)
            .unwrap_err();
        assert!(matches!(error.code, BrowserToolErrorCode::SessionExpired));
    }

    #[test]
    fn touch_refreshes_a_sessions_last_activity() {
        let mut registry = LeaseRegistry::new();
        let session = registry.start_session(Some("agent".to_string()));

        // Backdate the session an hour, then touch it.
        registry.backdate_session(&session.agent_session_id, 3_600_000);
        registry.touch(&session.agent_session_id);

        let expired = registry.expire_sessions(Duration::from_secs(60), now_ms());
        assert!(expired.is_empty());
    }

    #[test]
    fn touch_ignores_unknown_sessions() {
        let mut registry = LeaseRegistry::new();
        registry.touch(&AgentSessionId("session-nonexistent".to_string()));

        assert!(!registry.has_sessions());
    }

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
    fn focus_required_names_the_explicit_recovery_action() {
        let error = BrowserToolError::focus_required(&TabId("tab_test".to_string()));
        let value = serde_json::to_value(error).unwrap();

        assert_eq!(value["code"], "focus_required");
        assert_eq!(value["recovery"], "focus_tab");
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

    #[test]
    fn session_creation_uses_distinct_prefixed_ids() {
        let mut registry = LeaseRegistry::new();

        let first = registry.start_session(Some("agent one".to_string()));
        let second = registry.start_session(Some("agent two".to_string()));

        assert_ne!(first.agent_session_id, second.agent_session_id);
        assert_ne!(first.owner_display_id, second.owner_display_id);
        assert!(first.agent_session_id.0.starts_with("session_"));
        assert!(first.owner_display_id.0.starts_with("owner_"));
        assert_eq!(first.label.as_deref(), Some("agent one"));
        assert!(registry.session(&first.agent_session_id).is_some());
    }

    #[test]
    fn unknown_sessions_are_rejected() {
        let mut registry = LeaseRegistry::new();
        let missing_session = AgentSessionId("session_missing".to_string());

        let error = registry
            .lease_tab(&missing_session, snapshot("target-1"))
            .unwrap_err();

        assert_eq!(error.code, BrowserToolErrorCode::UnknownSession);
        assert_eq!(error.recovery, Some(RecoveryAction::StartSession));
    }

    #[test]
    fn active_leases_are_accessible_only_to_the_owner() {
        let mut registry = LeaseRegistry::new();
        let owner = registry.start_session(Some("owner".to_string()));
        let foreign = registry.start_session(Some("foreign".to_string()));
        let leased = registry
            .lease_tab(&owner.agent_session_id, focused_snapshot("target-1"))
            .unwrap();

        let owned = registry
            .require_active_owned(&owner.agent_session_id, &leased.tab_id, true)
            .unwrap();
        let error = registry
            .require_active_owned(&foreign.agent_session_id, &leased.tab_id, true)
            .unwrap_err();

        assert_eq!(owned.tab_id, leased.tab_id);
        assert!(leased.focused);
        assert_eq!(error.code, BrowserToolErrorCode::TabNotOwned);
        assert_eq!(error.recovery, Some(RecoveryAction::ListTabs));
    }

    #[test]
    fn released_closed_and_missing_leases_have_distinct_listing_behavior() {
        let mut registry = LeaseRegistry::new();
        let session = registry.start_session(Some("agent".to_string()));

        let released = registry
            .lease_tab(&session.agent_session_id, snapshot("released"))
            .unwrap();
        registry
            .release_tab(&session.agent_session_id, &released.tab_id)
            .unwrap();
        let released_error = registry
            .require_active_owned(&session.agent_session_id, &released.tab_id, true)
            .unwrap_err();

        let closed = registry
            .lease_tab(&session.agent_session_id, snapshot("closed"))
            .unwrap();
        registry
            .close_tab_mark(&session.agent_session_id, &closed.tab_id)
            .unwrap();
        let closed_error = registry
            .require_active_owned(&session.agent_session_id, &closed.tab_id, true)
            .unwrap_err();

        let missing = registry
            .lease_tab(&session.agent_session_id, snapshot("missing"))
            .unwrap();
        registry.mark_missing(&missing.tab_id).unwrap();
        let missing_error = registry
            .require_active_owned(&session.agent_session_id, &missing.tab_id, true)
            .unwrap_err();
        let owned = registry
            .list_owned_tabs(&session.agent_session_id, Some("missing"))
            .unwrap();

        assert_eq!(released_error.code, BrowserToolErrorCode::TabNotActive);
        assert_eq!(closed_error.code, BrowserToolErrorCode::TabNotActive);
        assert_eq!(missing_error.code, BrowserToolErrorCode::TargetMissing);
        assert_eq!(missing_error.recovery, Some(RecoveryAction::ReleaseTab));
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].tab_id, missing.tab_id);
        assert_eq!(owned[0].state, LeaseState::Missing);
        assert!(owned[0].focused);
    }

    #[test]
    fn target_claims_refuse_existing_owners_and_explicit_takeover_rekeys_the_lease() {
        let mut registry = LeaseRegistry::new();
        let first = registry.start_session(Some("first".to_string()));
        let second = registry.start_session(Some("second".to_string()));
        let first_tab = registry
            .claim_tab(&first.agent_session_id, snapshot("target-1"), false, None)
            .unwrap();

        let owned_error = registry
            .claim_tab(&second.agent_session_id, snapshot("target-1"), false, None)
            .unwrap_err();
        let empty_takeover_error = registry
            .claim_tab(
                &second.agent_session_id,
                snapshot("target-1"),
                true,
                Some("  "),
            )
            .unwrap_err();
        let second_tab = registry
            .claim_tab(
                &second.agent_session_id,
                snapshot("target-1"),
                true,
                Some("user asked this agent to take over"),
            )
            .unwrap();
        let old_tab_error = registry
            .require_active_owned(&first.agent_session_id, &first_tab.tab_id, true)
            .unwrap_err();

        assert_eq!(owned_error.code, BrowserToolErrorCode::TargetOwned);
        assert_eq!(
            empty_takeover_error.code,
            BrowserToolErrorCode::InvalidInput
        );
        assert_ne!(first_tab.tab_id, second_tab.tab_id);
        assert_eq!(old_tab_error.code, BrowserToolErrorCode::UnknownTab);
    }

    #[test]
    fn global_inventory_exposes_only_caller_owned_tab_handles() {
        let mut registry = LeaseRegistry::new();
        let caller = registry.start_session(Some("caller".to_string()));
        let foreign = registry.start_session(Some("foreign".to_string()));
        let caller_tab = registry
            .lease_tab(&caller.agent_session_id, snapshot("target-a"))
            .unwrap();
        registry
            .lease_tab(&foreign.agent_session_id, snapshot("target-b"))
            .unwrap();

        let inventory = registry
            .global_inventory(
                &caller.agent_session_id,
                [
                    focused_snapshot("target-a"),
                    snapshot("target-b"),
                    snapshot("target-c"),
                ],
            )
            .unwrap();
        let tabs = inventory
            .groups
            .iter()
            .flat_map(|group| group.tabs.iter())
            .collect::<Vec<_>>();
        let caller_summary = tabs
            .iter()
            .find(|summary| summary.target_id == "target-a")
            .unwrap();
        let foreign_summary = tabs
            .iter()
            .find(|summary| summary.target_id == "target-b")
            .unwrap();
        let unowned_summary = tabs
            .iter()
            .find(|summary| summary.target_id == "target-c")
            .unwrap();
        let value = serde_json::to_value(&inventory).unwrap();

        assert_eq!(caller_summary.caller_tab_id, Some(caller_tab.tab_id));
        assert_eq!(
            caller_summary.owner_display_id,
            Some(caller.owner_display_id)
        );
        assert!(caller_summary.owned_by_caller);
        assert!(caller_summary.focused);

        assert_eq!(
            foreign_summary.owner_display_id,
            Some(foreign.owner_display_id)
        );
        assert_eq!(foreign_summary.owner_label.as_deref(), Some("foreign"));
        assert!(!foreign_summary.owned_by_caller);
        assert_eq!(foreign_summary.caller_tab_id, None);
        assert!(!foreign_summary.claimable);

        assert_eq!(unowned_summary.owner_display_id, None);
        assert_eq!(unowned_summary.caller_tab_id, None);
        assert!(unowned_summary.claimable);
        assert!(value.to_string().contains("owner_display_id"));
        assert!(!value.to_string().contains("owner_session_id"));
    }
}
