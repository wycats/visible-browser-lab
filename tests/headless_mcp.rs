use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use proptest::{
    prelude::*,
    sample::select,
    strategy::{BoxedStrategy, Union},
    test_runner::Config,
};
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};
use serde_json::{Value, json};
use tempfile::TempDir;
use visible_browser_lab_test_support::{
    BROWSER_MODE_ENV, EXPECTED_TOOLS, FixtureServer, McpClient, OpenTab, RealBrowser,
    cleanup_open_tabs, close_target_via_cdp, data_url, field_str, run_live_smoke, stop_broker,
    tabs_include_id,
};

#[test]
fn deterministic_real_browser_facade() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-deterministic", false)?;
    let mut open_tabs = Vec::new();
    let cdp_endpoint = harness.cdp_endpoint().to_string();
    let summary = run_live_smoke(harness.client_mut(), &mut open_tabs, &cdp_endpoint);
    cleanup_open_tabs(harness.client_mut(), &mut open_tabs);
    let summary = summary?;
    assert!(summary.tool_count >= EXPECTED_TOOLS.len());
    assert!(summary.screenshot_bytes > 1000);
    assert!(summary.global_groups > 0);
    Ok(())
}

#[test]
fn explicit_focus_contract() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-focus-contract", true)?;
    let url = harness.fixture.url("/page");
    let session = harness.client_mut().call_tool(
        "start_session",
        json!({
            "label": "focus-contract",
            "start_url": url,
            "focus": false
        }),
        Duration::from_secs(45),
        false,
    )?;
    let session_id = field_str(&session, "agent_session_id")?;
    let tab = session.get("tab").context("start_session omitted tab")?;
    let open_tab = OpenTab::from_summary(&session_id, tab)?;
    let active = harness.client_mut().call_tool(
        "new_tab",
        json!({
            "agent_session_id": session_id,
            "url": "about:blank",
            "focus": true
        }),
        Duration::from_secs(20),
        false,
    )?;
    let active_tab = OpenTab::from_summary(
        &session_id,
        active.get("tab").context("new_tab omitted tab")?,
    )?;

    let navigation_url = harness.fixture.url("/page?background-navigation=1");
    harness.client_mut().call_tool(
        "navigate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "url": navigation_url,
            "wait_until": "load"
        }),
        Duration::from_secs(20),
        false,
    )?;

    let title = harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "expression": "document.title"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if title.get("value").and_then(Value::as_str) != Some("VBL Fixture") {
        bail!("background evaluation returned an unexpected title: {title}");
    }

    harness.client_mut().call_tool(
        "screenshot",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "full_page": false
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "expression": "document.querySelector('#entry').focus()"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "type_text",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "text": "background text"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "console_messages",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "network_events",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;

    let focus_state = harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "expression": "document.hasFocus() && document.visibilityState === 'visible'"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let browser_reports_focus = focus_state
        .get("value")
        .and_then(Value::as_bool)
        .context("trusted-input focus probe did not return a boolean")?;
    if env::var(BROWSER_MODE_ENV).as_deref() == Ok("visible") && browser_reports_focus {
        bail!("visible Chrome reported trusted-input focus for the background test tab");
    }

    for (tool, params) in [
        (
            "click",
            json!({
                "agent_session_id": session_id,
                "tab_id": open_tab.tab_id,
                "selector": "#clicker"
            }),
        ),
        (
            "press_key",
            json!({
                "agent_session_id": session_id,
                "tab_id": open_tab.tab_id,
                "key": "Enter"
            }),
        ),
    ] {
        let result = harness.client_mut().call_tool(
            tool,
            params,
            Duration::from_secs(20),
            !browser_reports_focus,
        )?;
        if !browser_reports_focus {
            assert_tool_error(&result, "focus_required")?;
        }
    }

    if !browser_reports_focus {
        harness.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": open_tab.tab_id
            }),
            Duration::from_secs(20),
            false,
        )?;
        harness.client_mut().call_tool(
            "click",
            json!({
                "agent_session_id": session_id,
                "tab_id": open_tab.tab_id,
                "selector": "#clicker"
            }),
            Duration::from_secs(20),
            false,
        )?;
        harness.client_mut().call_tool(
            "press_key",
            json!({
                "agent_session_id": session_id,
                "tab_id": open_tab.tab_id,
                "key": "Enter"
            }),
            Duration::from_secs(20),
            false,
        )?;
    }
    harness.client_mut().call_tool(
        "close_tab",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "close_tab",
        json!({
            "agent_session_id": session_id,
            "tab_id": active_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    Ok(())
}

proptest_state_machine::prop_state_machine! {
    #![proptest_config(property_config())]

    #[test]
    fn tab_ownership_state_machine(sequential 3..8 => OwnershipStateMachine);
}

fn property_config() -> Config {
    let cases = env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    Config {
        cases,
        ..Config::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Actor {
    First,
    Second,
}

impl Actor {
    const ALL: [Self; 2] = [Self::First, Self::Second];

    fn index(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::First => "property-first",
            Self::Second => "property-second",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelTabState {
    Active,
    Missing,
    Closed,
}

#[derive(Clone, Debug)]
struct ModelTab {
    owner: Option<Actor>,
    state: ModelTabState,
}

#[derive(Clone, Debug, Default)]
struct ModelState {
    sessions: [bool; 2],
    tabs: Vec<ModelTab>,
}

#[derive(Clone, Debug)]
enum Transition {
    StartSession(Actor),
    NewTab(Actor),
    ListOwned(Actor),
    ForeignFocus { caller: Actor, tab: usize },
    Release { owner: Actor, tab: usize },
    Claim { caller: Actor, tab: usize },
    Takeover { caller: Actor, tab: usize },
    Close { owner: Actor, tab: usize },
    ExternalCloseThenFocus { owner: Actor, tab: usize },
    Navigate { owner: Actor, tab: usize },
    Evaluate { owner: Actor, tab: usize },
}

struct OwnershipModel;

impl ReferenceStateMachine for OwnershipModel {
    type State = ModelState;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(ModelState::default()).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let mut choices: Vec<BoxedStrategy<Transition>> = Vec::new();

        for actor in Actor::ALL {
            if !state.sessions[actor.index()] {
                choices.push(Just(Transition::StartSession(actor)).boxed());
            } else {
                choices.push(Just(Transition::ListOwned(actor)).boxed());
                if state.tabs.len() < 6 {
                    choices.push(Just(Transition::NewTab(actor)).boxed());
                }
            }
        }

        for (tab, model_tab) in state.tabs.iter().enumerate() {
            match (model_tab.owner, model_tab.state) {
                (Some(owner), ModelTabState::Active) => {
                    choices.push(Just(Transition::Navigate { owner, tab }).boxed());
                    choices.push(Just(Transition::Evaluate { owner, tab }).boxed());
                    choices.push(Just(Transition::Release { owner, tab }).boxed());
                    choices.push(Just(Transition::Close { owner, tab }).boxed());
                    choices.push(Just(Transition::ExternalCloseThenFocus { owner, tab }).boxed());

                    for caller in Actor::ALL {
                        if state.sessions[caller.index()] && caller != owner {
                            choices.push(Just(Transition::ForeignFocus { caller, tab }).boxed());
                            choices.push(Just(Transition::Takeover { caller, tab }).boxed());
                        }
                    }
                }
                (None, ModelTabState::Active) => {
                    let callers = Actor::ALL
                        .into_iter()
                        .filter(|actor| state.sessions[actor.index()])
                        .collect::<Vec<_>>();
                    if !callers.is_empty() {
                        choices.push(
                            select(callers)
                                .prop_map(move |caller| Transition::Claim { caller, tab })
                                .boxed(),
                        );
                    }
                }
                _ => {}
            }
        }

        Union::new(choices).boxed()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match *transition {
            Transition::StartSession(actor) => {
                state.sessions[actor.index()] = true;
                state.tabs.push(ModelTab {
                    owner: Some(actor),
                    state: ModelTabState::Active,
                });
            }
            Transition::NewTab(actor) => {
                state.tabs.push(ModelTab {
                    owner: Some(actor),
                    state: ModelTabState::Active,
                });
            }
            Transition::ListOwned(_) | Transition::ForeignFocus { .. } => {}
            Transition::Release { tab, .. } => state.tabs[tab].owner = None,
            Transition::Claim { caller, tab } | Transition::Takeover { caller, tab } => {
                state.tabs[tab].owner = Some(caller);
            }
            Transition::Close { tab, .. } => state.tabs[tab].state = ModelTabState::Closed,
            Transition::ExternalCloseThenFocus { tab, .. } => {
                state.tabs[tab].state = ModelTabState::Missing;
            }
            Transition::Navigate { .. } | Transition::Evaluate { .. } => {}
        }
        state
    }

    fn preconditions(state: &Self::State, transition: &Self::Transition) -> bool {
        match *transition {
            Transition::StartSession(actor) => !state.sessions[actor.index()],
            Transition::NewTab(actor) | Transition::ListOwned(actor) => {
                state.sessions[actor.index()]
            }
            Transition::ForeignFocus { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active
                            && tab.owner.is_some()
                            && tab.owner != Some(caller)
                    })
            }
            Transition::Release { owner, tab }
            | Transition::Close { owner, tab }
            | Transition::ExternalCloseThenFocus { owner, tab }
            | Transition::Navigate { owner, tab }
            | Transition::Evaluate { owner, tab } => state
                .tabs
                .get(tab)
                .is_some_and(|tab| tab.state == ModelTabState::Active && tab.owner == Some(owner)),
            Transition::Claim { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active && tab.owner.is_none()
                    })
            }
            Transition::Takeover { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active
                            && tab.owner.is_some()
                            && tab.owner != Some(caller)
                    })
            }
        }
    }
}

struct OwnershipStateMachine;

impl StateMachineTest for OwnershipStateMachine {
    type SystemUnderTest = BrowserMcpHarness;
    type Reference = OwnershipModel;

    fn init_test(_ref_state: &ModelState) -> Self::SystemUnderTest {
        BrowserMcpHarness::start("visible-browser-lab-property", true)
            .unwrap_or_else(|error| panic!("{error:#}"))
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &ModelState,
        transition: Transition,
    ) -> Self::SystemUnderTest {
        state
            .apply_transition(transition)
            .unwrap_or_else(|error| panic!("{error:#}"));
        state
            .check_model(ref_state)
            .unwrap_or_else(|error| panic!("{error:#}"));
        state
    }

    fn teardown(mut state: Self::SystemUnderTest, _ref_state: ModelState) {
        state.shutdown();
    }
}

struct ConcreteTab {
    owner: Option<Actor>,
    state: ModelTabState,
    tab_id: String,
    target_id: String,
}

struct BrowserMcpHarness {
    browser: RealBrowser,
    state_dir: TempDir,
    client: Option<McpClient>,
    fixture: FixtureServer,
    sessions: [Option<String>; 2],
    tabs: Vec<ConcreteTab>,
}

impl BrowserMcpHarness {
    fn start(client_name: &str, initialize: bool) -> Result<Self> {
        let browser = RealBrowser::launch_from_env()?;
        let state_dir = tempfile::Builder::new()
            .prefix("visible-browser-lab-headless-mcp-")
            .tempdir()
            .context("failed to create broker state directory")?;
        let root = repo_root();
        let mut client = McpClient::spawn(
            &test_binary(),
            browser.cdp_endpoint(),
            state_dir.path(),
            &root,
        )?;
        if initialize {
            client.initialize(client_name)?;
        }

        Ok(Self {
            browser,
            state_dir,
            client: Some(client),
            fixture: FixtureServer::start()?,
            sessions: [None, None],
            tabs: Vec::new(),
        })
    }

    fn client_mut(&mut self) -> &mut McpClient {
        self.client.as_mut().expect("MCP client is still running")
    }

    fn cdp_endpoint(&self) -> &str {
        self.browser.cdp_endpoint()
    }

    fn session(&self, actor: Actor) -> Result<String> {
        self.sessions[actor.index()]
            .clone()
            .with_context(|| format!("session for {actor:?} has not started"))
    }

    fn apply_transition(&mut self, transition: Transition) -> Result<()> {
        match transition {
            Transition::StartSession(actor) => self.start_session(actor),
            Transition::NewTab(actor) => self.new_tab(actor),
            Transition::ListOwned(actor) => self.list_owned(actor).map(|_| ()),
            Transition::ForeignFocus { caller, tab } => self.foreign_focus(caller, tab),
            Transition::Release { owner, tab } => self.release(owner, tab),
            Transition::Claim { caller, tab } => self.claim(caller, tab),
            Transition::Takeover { caller, tab } => self.takeover(caller, tab),
            Transition::Close { owner, tab } => self.close(owner, tab),
            Transition::ExternalCloseThenFocus { owner, tab } => {
                self.external_close_then_focus(owner, tab)
            }
            Transition::Navigate { owner, tab } => self.navigate(owner, tab),
            Transition::Evaluate { owner, tab } => self.evaluate(owner, tab),
        }
    }

    fn start_session(&mut self, actor: Actor) -> Result<()> {
        let url = self.fixture.url("/page");
        let result = self.client_mut().call_tool(
            "start_session",
            json!({
                "label": actor.label(),
                "start_url": url,
                "focus": true
            }),
            Duration::from_secs(45),
            false,
        )?;
        let session_id = field_str(&result, "agent_session_id")?;
        let tab = result.get("tab").context("start_session omitted tab")?;
        let open_tab = OpenTab::from_summary(&session_id, tab)?;
        self.sessions[actor.index()] = Some(session_id);
        self.tabs.push(ConcreteTab {
            owner: Some(actor),
            state: ModelTabState::Active,
            tab_id: open_tab.tab_id,
            target_id: open_tab.target_id,
        });
        Ok(())
    }

    fn new_tab(&mut self, actor: Actor) -> Result<()> {
        let session_id = self.session(actor)?;
        let url = self.fixture.url("/page");
        let result = self.client_mut().call_tool(
            "new_tab",
            json!({
                "agent_session_id": session_id,
                "url": url,
                "focus": true
            }),
            Duration::from_secs(45),
            false,
        )?;
        let tab = result.get("tab").context("new_tab omitted tab")?;
        let open_tab = OpenTab::from_summary(&session_id, tab)?;
        self.tabs.push(ConcreteTab {
            owner: Some(actor),
            state: ModelTabState::Active,
            tab_id: open_tab.tab_id,
            target_id: open_tab.target_id,
        });
        Ok(())
    }

    fn list_owned(&mut self, actor: Actor) -> Result<Value> {
        let session_id = self.session(actor)?;
        self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": session_id }),
            Duration::from_secs(20),
            false,
        )
    }

    fn foreign_focus(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&error, "tab_not_owned")
    }

    fn release(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "release_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            false,
        )?;
        if result.get("released").and_then(Value::as_bool) != Some(true) {
            bail!("release_tab did not report released=true: {result}");
        }
        self.tabs[tab].owner = None;
        Ok(())
    }

    fn claim(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let target_id = self.tabs[tab].target_id.clone();
        let result = self.client_mut().call_tool(
            "claim_tab",
            json!({
                "agent_session_id": session_id,
                "target_id": target_id
            }),
            Duration::from_secs(30),
            false,
        )?;
        let tab_summary = result.get("tab").context("claim_tab omitted tab")?;
        self.tabs[tab].tab_id = field_str(tab_summary, "tab_id")?;
        self.tabs[tab].owner = Some(caller);
        Ok(())
    }

    fn takeover(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let old_owner = self.tabs[tab].owner.context("takeover tab has no owner")?;
        let old_session = self.session(old_owner)?;
        let old_tab_id = self.tabs[tab].tab_id.clone();
        let target_id = self.tabs[tab].target_id.clone();
        let result = self.client_mut().call_tool(
            "claim_tab",
            json!({
                "agent_session_id": session_id,
                "target_id": target_id,
                "takeover": true,
                "user_instruction": "transfer this tab for property validation"
            }),
            Duration::from_secs(30),
            false,
        )?;
        let tab_summary = result.get("tab").context("takeover omitted tab")?;
        let new_tab_id = field_str(tab_summary, "tab_id")?;
        if new_tab_id == old_tab_id {
            bail!("takeover reused the old tab_id");
        }
        let old_error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": old_session.clone(),
                "tab_id": old_tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&old_error, "unknown_tab")?;
        let old_owner_tabs = self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": old_session }),
            Duration::from_secs(20),
            false,
        )?;
        let old_owner_tabs = old_owner_tabs
            .get("tabs")
            .and_then(Value::as_array)
            .context("old owner list_tabs omitted tabs array")?;
        if tabs_include_id(old_owner_tabs, &old_tab_id) {
            bail!("old owner list_tabs exposed stale takeover tab_id `{old_tab_id}`");
        }
        self.tabs[tab].tab_id = new_tab_id;
        self.tabs[tab].owner = Some(caller);
        Ok(())
    }

    fn close(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "close_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(30),
            false,
        )?;
        if result.get("closed").and_then(Value::as_bool) != Some(true) {
            bail!("close_tab did not report closed=true: {result}");
        }
        self.tabs[tab].state = ModelTabState::Closed;
        Ok(())
    }

    fn external_close_then_focus(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let target_id = self.tabs[tab].target_id.clone();
        close_target_via_cdp(self.cdp_endpoint(), &target_id)?;
        let error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&error, "target_missing")?;
        self.tabs[tab].state = ModelTabState::Missing;
        Ok(())
    }

    fn navigate(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "url": data_url("VBL Property", "VBL Property"),
                "timeout_ms": 10000
            }),
            Duration::from_secs(30),
            false,
        )?;
        let returned_tab = result.get("tab").context("navigate omitted tab")?;
        if field_str(returned_tab, "tab_id")? != self.tabs[tab].tab_id {
            bail!("navigate returned a different tab_id");
        }
        Ok(())
    }

    fn evaluate(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "expression": "1 + 1"
            }),
            Duration::from_secs(20),
            false,
        )?;
        if result.get("value").and_then(Value::as_i64) != Some(2) {
            bail!("evaluate returned an unexpected value: {result}");
        }
        Ok(())
    }

    fn check_model(&mut self, ref_state: &ModelState) -> Result<()> {
        for actor in Actor::ALL {
            if !ref_state.sessions[actor.index()] {
                continue;
            }
            self.check_owned_listing(actor, ref_state)?;
            self.check_global_inventory(actor)?;
        }
        Ok(())
    }

    fn check_owned_listing(&mut self, actor: Actor, ref_state: &ModelState) -> Result<()> {
        let session_id = self.session(actor)?;
        let owned = self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": session_id }),
            Duration::from_secs(20),
            false,
        )?;
        let tabs = owned
            .get("tabs")
            .and_then(Value::as_array)
            .context("owned list_tabs omitted tabs array")?;
        for (index, model_tab) in ref_state.tabs.iter().enumerate() {
            let concrete = self
                .tabs
                .get(index)
                .with_context(|| format!("missing concrete tab {index}"))?;
            let listed = tabs_include_id(tabs, &concrete.tab_id);
            let should_be_listed =
                model_tab.owner == Some(actor) && model_tab.state != ModelTabState::Closed;
            if listed != should_be_listed {
                bail!(
                    "owned list mismatch for {actor:?} tab {index}: listed={listed}, expected={should_be_listed}, response={owned}"
                );
            }
        }
        Ok(())
    }

    fn check_global_inventory(&mut self, actor: Actor) -> Result<()> {
        let session_id = self.session(actor)?;
        let global = self.client_mut().call_tool(
            "list_tabs",
            json!({
                "agent_session_id": session_id,
                "scope": "global_readonly"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let groups = global
            .get("groups")
            .and_then(Value::as_array)
            .context("global list_tabs omitted groups array")?;
        for tab in groups
            .iter()
            .filter_map(|group| group.get("tabs").and_then(Value::as_array))
            .flat_map(|tabs| tabs.iter())
        {
            let owned_by_caller = tab
                .get("owned_by_caller")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !owned_by_caller
                && tab.get("owner_display_id").is_some()
                && (tab.get("caller_tab_id").is_some() || tab.get("tab_id").is_some())
            {
                bail!("global_readonly exposed a foreign action handle: {tab}");
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(client) = self.client.as_mut() {
            let mut open_tabs = self
                .tabs
                .iter()
                .filter_map(|tab| {
                    let owner = tab.owner?;
                    if tab.state != ModelTabState::Active {
                        return None;
                    }
                    Some(OpenTab {
                        session_id: self.sessions[owner.index()].clone()?,
                        tab_id: tab.tab_id.clone(),
                        target_id: tab.target_id.clone(),
                    })
                })
                .collect::<Vec<_>>();
            cleanup_open_tabs(client, &mut open_tabs);
        }

        for tab in &self.tabs {
            if tab.owner.is_none() && tab.state == ModelTabState::Active {
                let _ = close_target_via_cdp(self.cdp_endpoint(), &tab.target_id);
            }
        }

        if let Some(client) = self.client.as_mut() {
            client.shutdown();
        }
        stop_broker(self.state_dir.path());
        self.browser.shutdown();
        self.client = None;
    }
}

impl Drop for BrowserMcpHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn assert_tool_error(value: &Value, expected: &str) -> Result<()> {
    let code = field_str(value, "code")?;
    if code != expected {
        bail!("expected tool error `{expected}`, got `{code}` in {value}");
    }
    Ok(())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn test_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_visible-browser-lab-mcp"))
}
