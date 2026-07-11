use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[allow(deprecated)]
use chromiumoxide::{
    Browser,
    cdp::{
        browser_protocol::{
            accessibility::{
                AxNode, AxValue, EnableParams as AccessibilityEnableParams, GetFullAxTreeParams,
            },
            browser::CloseParams as BrowserCloseParams,
            dom::{
                BackendNodeId, DescribeNodeParams, GetBoxModelParams, GetContentQuadsParams,
                GetDocumentParams, PushNodesByBackendIdsToFrontendParams, QuerySelectorAllParams,
                ResolveNodeParams, SetFileInputFilesParams,
            },
            emulation::{
                ClearDeviceMetricsOverrideParams, MediaFeature, SetCpuThrottlingRateParams,
                SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
                SetFocusEmulationEnabledParams, SetGeolocationOverrideParams,
                SetTouchEmulationEnabledParams, SetUserAgentOverrideParams,
            },
            input::{
                DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
                DispatchMouseEventType, InsertTextParams, MouseButton,
            },
            log::{
                DisableParams as LogDisableParams, EnableParams as LogEnableParams, EventEntryAdded,
            },
            network::{
                DisableParams as NetworkDisableParams, EmulateNetworkConditionsParams,
                EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
                EventRequestWillBeSent, EventResponseReceived, GetRequestPostDataParams,
                GetResponseBodyParams, Headers, RequestId, SetExtraHttpHeadersParams,
            },
            page::{
                AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat,
                CaptureScreenshotParams, DialogType, EnableParams as PageEnableParams,
                EventDomContentEventFired, EventJavascriptDialogOpening, EventLoadEventFired,
                EventScreencastFrame, EventScreencastVisibilityChanged, Frame, FrameTree,
                GetFrameTreeParams, GetLayoutMetricsParams, GetNavigationHistoryParams,
                HandleJavaScriptDialogParams, NavigateParams as PageNavigateParams,
                NavigateToHistoryEntryParams, ReloadParams,
                RemoveScriptToEvaluateOnNewDocumentParams, ScreencastFrameAckParams,
                ScriptIdentifier, StartScreencastFormat, StartScreencastParams,
                StopScreencastParams, Viewport,
            },
            target::{
                ActivateTargetParams, CloseTargetParams, CreateTargetParams, GetTargetsParams,
                TargetId,
            },
            tracing::{
                EndParams as TraceEndParams, EventDataCollected, EventTracingComplete,
                StartParams as TraceStartParams, StartTransferMode, TraceConfig,
            },
        },
        js_protocol::heap_profiler::{
            EnableParams as HeapEnableParams, EventAddHeapSnapshotChunk, TakeHeapSnapshotParams,
        },
        js_protocol::runtime::{
            CallArgument, CallFunctionOnParams, DisableParams as RuntimeDisableParams,
            DiscardConsoleEntriesParams, EnableParams as RuntimeEnableParams,
            EvaluateParams as RuntimeEvaluateParams, EventConsoleApiCalled,
            ReleaseObjectGroupParams, RemoteObjectId,
        },
    },
    error::CdpError,
    handler::HandlerConfig,
    page::Page,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use url::Url;

use crate::leases::{BrowserToolError, BrowserToolErrorCode, TabSnapshot};
use crate::protocol::{EvaluateResult, NetworkEvent};
use crate::semantic::{RawAxFrame, RawAxNode, RawAxSnapshot};

const PAGE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_DISCOVERY_RETRY: Duration = Duration::from_millis(25);
const EXISTING_TARGET_REGISTRATION_DELAY: Duration = Duration::from_millis(250);
const NAVIGATION_RECOVERY_GRACE: Duration = Duration::from_secs(5);
const BEFOREUNLOAD_STASH_KEY: &str = "__io_github_wycats_visible_browser_lab_beforeunload_stash_v1";

/// Every remote handle we create is tagged with a per-operation object
/// group. Ungrouped handles pin their DOM nodes (including whole detached
/// trees) in the renderer heap until the page navigates, which is how long
/// agent sessions ballooned Chrome to gigabytes of RSS. The group is unique
/// per operation so releasing it cannot free a handle a concurrent request
/// on the same tab is still using.
static OBJECT_GROUP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_object_group() -> String {
    format!(
        "visible-browser-lab-{}",
        OBJECT_GROUP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Caps for Chrome's per-target network buffering while Network is enabled.
/// The per-resource cap has to fit the largest response body the network
/// tool can hand back as an artifact (bodies over the 1MB inline limit
/// become artifacts, so the cap bounds artifacts too).
const NETWORK_TOTAL_BUFFER_BYTES: i64 = 64 * 1024 * 1024;
const NETWORK_RESOURCE_BUFFER_BYTES: i64 = 32 * 1024 * 1024;
/// How many console events we let Chrome retain before asking it to drop
/// its copies (the arguments stay pinned as remote objects otherwise).
const CONSOLE_DISCARD_INTERVAL: usize = 50;

/// Free every handle in one operation's object group. Best-effort: the
/// handles get reclaimed on navigation anyway; this just keeps long-lived
/// pages lean.
async fn release_object_group(page: &Page, object_group: &str) {
    let _ = page
        .execute(ReleaseObjectGroupParams::new(object_group))
        .await;
}

/// Broker-created tabs each get their own Chrome window (RFC 00010). Tabs
/// sharing a window can background each other: activating one marks its
/// siblings hidden, which stops frame production and starves screencasts.
/// A tab that is alone in its window has no siblings, so the hazard cannot
/// occur. Window-vs-window overlap is harmless under the occlusion launch
/// flags in `managed_chrome.rs`.
static WINDOW_SLOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// New windows cascade from this origin so they do not stack identically.
const WINDOW_CASCADE_ORIGIN: (i64, i64) = (80, 80);
const WINDOW_CASCADE_STEP: i64 = 40;
const WINDOW_CASCADE_SLOTS: u64 = 10;
const WINDOW_DEFAULT_WIDTH: i64 = 1280;
const WINDOW_DEFAULT_HEIGHT: i64 = 900;

fn next_window_slot() -> u64 {
    WINDOW_SLOT_COUNTER.fetch_add(1, Ordering::Relaxed) % WINDOW_CASCADE_SLOTS
}

fn create_page_params(
    url: Option<&str>,
    focus: bool,
    window_slot: u64,
) -> Result<CreateTargetParams, BrowserToolError> {
    let offset = WINDOW_CASCADE_STEP * i64::try_from(window_slot).unwrap_or(0);
    CreateTargetParams::builder()
        .url(url.unwrap_or("about:blank"))
        .background(!focus)
        .new_window(true)
        .left(WINDOW_CASCADE_ORIGIN.0 + offset)
        .top(WINDOW_CASCADE_ORIGIN.1 + offset)
        .width(WINDOW_DEFAULT_WIDTH)
        .height(WINDOW_DEFAULT_HEIGHT)
        .build()
        .map_err(BrowserToolError::invalid_input)
}

#[derive(Clone, Copy)]
pub struct ElementEvaluation<'a> {
    pub source: &'a str,
    pub mode: &'a str,
    pub args: &'a [Value],
    pub await_promise: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEndpoint {
    origin: Url,
}

impl CdpEndpoint {
    pub fn parse(endpoint: &str) -> anyhow::Result<Self> {
        let origin = Url::parse(endpoint)?;
        if !matches!(origin.scheme(), "http" | "https" | "ws" | "wss") {
            anyhow::bail!("unsupported CDP endpoint scheme `{}`", origin.scheme());
        }
        Ok(Self { origin })
    }

    pub fn version_url(&self) -> Url {
        self.join("/json/version")
    }

    pub fn targets_url(&self) -> Url {
        self.join("/json/list")
    }

    pub fn origin(&self) -> &Url {
        &self.origin
    }

    fn join(&self, path: &str) -> Url {
        self.origin
            .join(path)
            .expect("static CDP paths must be valid URLs")
    }
}

#[derive(Debug, Clone)]
pub struct CdpClient {
    runtime: Arc<CdpRuntime>,
}

impl CdpClient {
    pub fn new(endpoint: &str) -> anyhow::Result<Self> {
        let endpoint = CdpEndpoint::parse(endpoint)?;
        Ok(Self {
            runtime: Arc::new(CdpRuntime::new(endpoint)),
        })
    }

    pub fn endpoint(&self) -> &Url {
        self.runtime.endpoint.origin()
    }

    #[cfg(test)]
    async fn disconnect_for_test(&self) {
        self.runtime.disconnect().await;
    }

    pub async fn page_targets(&self) -> Result<Vec<CdpTarget>, BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let response = self
            .runtime
            .browser_command(
                &connection,
                connection.browser.execute(GetTargetsParams::default()),
                "list Chrome targets",
            )
            .await?;

        Ok(response
            .result
            .target_infos
            .into_iter()
            .filter(|target| target.r#type == "page")
            .map(|target| CdpTarget {
                id: target.target_id.as_ref().to_string(),
                target_type: target.r#type,
                title: target.title,
                url: target.url,
            })
            .collect())
    }

    pub async fn create_page(
        &self,
        url: Option<&str>,
        focus: bool,
    ) -> Result<CdpTarget, BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let params = create_page_params(url, focus, next_window_slot())?;
        let response = self
            .runtime
            .browser_command(
                &connection,
                connection.browser.execute(params),
                "create Chrome target",
            )
            .await?;
        let target_id = response.result.target_id.as_ref().to_string();

        if focus {
            self.activate_target(&target_id).await?;
        }

        self.page_target(&target_id).await
    }

    pub async fn page_target(&self, target_id: &str) -> Result<CdpTarget, BrowserToolError> {
        self.page_targets()
            .await?
            .into_iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| BrowserToolError::target_missing_for_target(target_id))
    }

    pub async fn activate_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        self.runtime
            .browser_command(
                &connection,
                connection
                    .browser
                    .execute(ActivateTargetParams::new(TargetId::new(target_id))),
                "activate Chrome target",
            )
            .await?;
        // Real activation makes the page genuinely visible, so a screencast
        // no longer needs its focus-emulation override here. Clearing it
        // restores honest page-side focus answers; if the page goes hidden
        // again mid-cast, the cast's visibility listener re-engages it.
        let engaged = self
            .runtime
            .screencast_focus_overrides
            .lock()
            .unwrap()
            .remove(target_id);
        if engaged && let Ok((page, _)) = self.page(target_id).await {
            let _ = page
                .execute(SetFocusEmulationEnabledParams::new(false))
                .await;
        }
        Ok(())
    }

    pub async fn prepare_target_for_action(
        &self,
        target: &CdpTarget,
    ) -> Result<(), BrowserToolError> {
        let _ = self.page(&target.id).await?;
        Ok(())
    }

    pub async fn close_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        self.runtime
            .browser_command(
                &connection,
                connection
                    .browser
                    .execute(CloseTargetParams::new(TargetId::new(target_id))),
                "close Chrome target",
            )
            .await?;
        Ok(())
    }

    pub async fn close_browser(&self) -> Result<(), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        self.runtime
            .browser_command(
                &connection,
                connection.browser.execute(BrowserCloseParams::default()),
                "close Chrome",
            )
            .await?;
        Ok(())
    }

    pub async fn navigate(
        &self,
        target: &CdpTarget,
        url: &str,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<(), BrowserToolError> {
        let wait_until = validated_navigation_wait_state(wait_until)?;
        let (page, connection) = self.page(&target.id).await?;
        let original_href = if before_unload == Some("accept") {
            let value = page
                .evaluate_expression("location.href")
                .await
                .map_err(|error| map_cdp_error("read pre-navigation URL", &error))?;
            Some(value.into_value::<String>().map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "Chrome returned an invalid pre-navigation URL: {error}"
                ))
            })?)
        } else {
            None
        };
        if original_href.is_some() {
            self.clear_beforeunload_handlers(&page, &connection).await?;
        }
        let navigation = async {
            match wait_until {
                "none" => self
                    .start_page_navigation(&page, &connection, url)
                    .await
                    .map(|_| ()),
                "dom_content_loaded" => {
                    self.navigate_and_wait_for_event::<EventDomContentEventFired>(
                        &page,
                        &connection,
                        url,
                        timeout_ms,
                        "DOMContentLoaded",
                    )
                    .await
                }
                "load" | "network_idle" => {
                    self.navigate_and_wait_for_event::<EventLoadEventFired>(
                        &page,
                        &connection,
                        url,
                        timeout_ms,
                        "load",
                    )
                    .await
                }
                _ => unreachable!("navigation wait state was validated before navigation"),
            }
        };
        let result = self
            .with_navigation_timeout(
                &page,
                &connection,
                before_unload,
                timeout_ms,
                wait_until != "none",
                navigation,
            )
            .await;
        match result {
            Err(error)
                if error.code == crate::leases::BrowserToolErrorCode::OperationTimeout
                    && original_href.is_some() =>
            {
                match self
                    .recover_completed_navigation(
                        &target.id,
                        original_href.as_deref().unwrap_or_default(),
                    )
                    .await
                {
                    Ok(()) => {
                        self.restore_beforeunload_handlers(&target.id).await?;
                        Ok(())
                    }
                    Err(_) => {
                        if let Err(restore_error) =
                            self.restore_beforeunload_handlers(&target.id).await
                        {
                            tracing::warn!(
                                target_id = %target.id,
                                error = %restore_error.message,
                                "failed to restore beforeunload handlers after rejected navigation"
                            );
                        }
                        Err(error)
                    }
                }
            }
            Err(error) if original_href.is_some() => {
                if let Err(restore_error) = self.restore_beforeunload_handlers(&target.id).await {
                    tracing::warn!(
                        target_id = %target.id,
                        error = %restore_error.message,
                        "failed to restore beforeunload handlers after rejected navigation"
                    );
                }
                Err(error)
            }
            Ok(()) if original_href.is_some() => {
                self.restore_beforeunload_handlers_on_page(&page, &connection)
                    .await?;
                Ok(())
            }
            result => result,
        }
    }

    async fn start_page_navigation(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        url: &str,
    ) -> Result<Option<String>, BrowserToolError> {
        let response = self
            .runtime
            .page_command(
                connection,
                page.execute(PageNavigateParams::new(url)),
                "start page navigation",
            )
            .await?;
        if let Some(error) = response.result.error_text {
            return Err(BrowserToolError::chrome_unavailable(format!(
                "navigation to `{url}` failed: {error}"
            )));
        }
        Ok(response
            .result
            .loader_id
            .map(|loader_id| loader_id.as_ref().to_string()))
    }

    async fn main_frame_loader_id(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
    ) -> Result<String, BrowserToolError> {
        let tree = self
            .runtime
            .page_command(
                connection,
                page.execute(GetFrameTreeParams::default()),
                "read page frame tree",
            )
            .await?;
        Ok(tree.result.frame_tree.frame.loader_id.as_ref().to_string())
    }

    async fn navigate_and_wait_for_event<T>(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        url: &str,
        timeout_ms: u64,
        event_name: &str,
    ) -> Result<(), BrowserToolError>
    where
        T: chromiumoxide::cdp::IntoEventKind + Unpin,
    {
        let mut events = self.event_listener::<T>(page, connection).await?;
        let Some(expected_loader_id) = self.start_page_navigation(page, connection, url).await?
        else {
            return Ok(());
        };
        match self
            .wait_for_lifecycle_event(&mut events, timeout_ms, event_name)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if error.code == crate::leases::BrowserToolErrorCode::OperationTimeout => {
                let frame_tree = self
                    .runtime
                    .page_command(
                        connection,
                        page.execute(GetFrameTreeParams::default()),
                        "verify committed navigation",
                    )
                    .await?;
                let committed_loader_id = frame_tree
                    .result
                    .frame_tree
                    .frame
                    .loader_id
                    .as_ref()
                    .to_string();
                let ready_state = page
                    .evaluate_expression("document.readyState")
                    .await
                    .map_err(|cdp_error| map_cdp_error("read document readiness", &cdp_error))?
                    .into_value::<String>()
                    .map_err(|value_error| {
                        BrowserToolError::chrome_unavailable(format!(
                            "Chrome returned an invalid document state: {value_error}"
                        ))
                    })?;
                let ready = committed_navigation_reached_wait_state(
                    &expected_loader_id,
                    &committed_loader_id,
                    &ready_state,
                    event_name,
                );
                if ready { Ok(()) } else { Err(error) }
            }
            Err(error) => Err(error),
        }
    }

    async fn wait_for_lifecycle_event<T>(
        &self,
        events: &mut chromiumoxide::listeners::EventStream<T>,
        timeout_ms: u64,
        event_name: &str,
    ) -> Result<(), BrowserToolError>
    where
        T: chromiumoxide::cdp::IntoEventKind + Unpin,
    {
        match timeout(Duration::from_millis(timeout_ms), events.next()).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(BrowserToolError::chrome_unavailable(format!(
                "Chrome closed the {event_name} event stream during navigation"
            ))),
            Err(_) => Err(BrowserToolError::operation_timeout(format!(
                "timed out waiting for {event_name} during navigation"
            ))),
        }
    }

    async fn clear_beforeunload_handlers(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
    ) -> Result<(), BrowserToolError> {
        // An explicit accept authorizes VBL to remove page-owned guards before
        // navigating. This avoids Chromium leaving the target session wedged
        // after acknowledging a native beforeunload dialog.
        let expression = format!(
            r#"(() => {{
            const stashKey = {stash_key};
            const listeners = typeof getEventListeners === 'function'
                ? (getEventListeners(window).beforeunload ?? [])
                : [];
            window[stashKey] = {{
                listeners: listeners.map(entry => ({{
                    listener: entry.listener,
                    capture: Boolean(entry.useCapture),
                    passive: Boolean(entry.passive),
                    once: Boolean(entry.once),
                }})),
                onbeforeunload: window.onbeforeunload,
            }};
            for (const entry of listeners) {{
                window.removeEventListener('beforeunload', entry.listener, entry.useCapture);
            }}
            window.onbeforeunload = null;
        }})()"#,
            stash_key = serde_json::to_string(BEFOREUNLOAD_STASH_KEY).unwrap()
        );
        let response = self
            .runtime
            .page_command(
                connection,
                page.execute(
                    RuntimeEvaluateParams::builder()
                        .expression(expression)
                        .include_command_line_api(true)
                        .return_by_value(true)
                        .build()
                        .map_err(BrowserToolError::invalid_input)?,
                ),
                "accept beforeunload navigation",
            )
            .await?;
        if let Some(details) = response.result.exception_details {
            return Err(BrowserToolError::chrome_unavailable(format!(
                "accept beforeunload navigation failed: {}",
                details.text
            )));
        }
        Ok(())
    }

    async fn restore_beforeunload_handlers(
        &self,
        target_id: &str,
    ) -> Result<bool, BrowserToolError> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let attempt = async {
                let (page, connection) = self.page(target_id).await?;
                self.restore_beforeunload_handlers_on_page(&page, &connection)
                    .await
            }
            .await;
            match attempt {
                // A clean `false` means this is a committed new document and
                // there is no old-document stash to restore. Only transient
                // attachment/evaluation failures warrant the recovery grace.
                Ok(restored) => return Ok(restored),
                Err(error)
                    if error.code == BrowserToolErrorCode::ChromeUnavailable
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn restore_beforeunload_handlers_on_page(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
    ) -> Result<bool, BrowserToolError> {
        let expression = format!(
            r#"(() => {{
                const stashKey = {stash_key};
                const stash = window[stashKey];
                if (!stash) return false;
                for (const entry of stash.listeners) {{
                    window.addEventListener('beforeunload', entry.listener, {{
                        capture: entry.capture,
                        passive: entry.passive,
                        once: entry.once,
                    }});
                }}
                window.onbeforeunload = stash.onbeforeunload;
                delete window[stashKey];
                return true;
            }})()"#,
            stash_key = serde_json::to_string(BEFOREUNLOAD_STASH_KEY).unwrap(),
        );
        let restore = self
            .runtime
            .page_command(
                connection,
                page.execute(
                    RuntimeEvaluateParams::builder()
                        .expression(expression)
                        .return_by_value(true)
                        .build()
                        .expect("beforeunload restore expression is valid"),
                ),
                "restore beforeunload navigation guard",
            )
            .await?;
        if let Some(details) = restore.result.exception_details {
            return Err(BrowserToolError::chrome_unavailable(format!(
                "restore beforeunload navigation guard failed: {}",
                details.text
            )));
        }
        restore
            .result
            .result
            .value
            .and_then(|value| value.as_bool())
            .ok_or_else(|| {
                BrowserToolError::chrome_unavailable(
                    "Chrome returned an invalid beforeunload restoration result",
                )
            })
    }

    async fn recover_completed_navigation(
        &self,
        target_id: &str,
        original_href: &str,
    ) -> Result<(), BrowserToolError> {
        // Chromium can commit the navigation while its command response is
        // lost during dialog teardown. Only treat that timeout as success when
        // a fresh attachment proves both a changed URL and a usable document.
        let recovery = async {
            loop {
                let (page, _) = self.page(target_id).await?;
                let value = page
                    .evaluate_expression(
                        "({ href: location.href, readyState: document.readyState })",
                    )
                    .await
                    .map_err(|error| map_cdp_error("verify recovered navigation", &error))?
                    .into_value::<Value>()
                    .map_err(|error| {
                        BrowserToolError::chrome_unavailable(format!(
                            "Chrome returned invalid recovered navigation state: {error}"
                        ))
                    })?;
                let href = value
                    .get("href")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let ready_state = value
                    .get("readyState")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if href != original_href && matches!(ready_state, "interactive" | "complete") {
                    return Ok(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        };
        timeout(Duration::from_secs(2), recovery)
            .await
            .map_err(|_| {
                BrowserToolError::operation_timeout(
                    "navigation did not recover after the CDP command timed out",
                )
            })?
    }

    pub async fn add_init_script(
        &self,
        target: &CdpTarget,
        source: &str,
    ) -> Result<String, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let response = self
            .runtime
            .page_command(
                &connection,
                page.execute(AddScriptToEvaluateOnNewDocumentParams::new(source)),
                "install navigation init script",
            )
            .await?;
        Ok(response.result.identifier.into())
    }

    pub async fn remove_init_script(
        &self,
        target: &CdpTarget,
        identifier: String,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(RemoveScriptToEvaluateOnNewDocumentParams::new(
                    ScriptIdentifier::new(identifier),
                )),
                "remove navigation init script",
            )
            .await?;
        Ok(())
    }

    pub async fn navigate_history(
        &self,
        target: &CdpTarget,
        direction: i64,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<(), BrowserToolError> {
        let wait_until = validated_navigation_wait_state(wait_until)?;
        let (page, connection) = self.page(&target.id).await?;
        let history = self
            .runtime
            .page_command(
                &connection,
                page.execute(GetNavigationHistoryParams::default()),
                "read navigation history",
            )
            .await?;
        let index = history.result.current_index + direction;
        let entry = history
            .result
            .entries
            .get(index.max(0) as usize)
            .filter(|_| index >= 0)
            .ok_or_else(|| {
                BrowserToolError::invalid_input(if direction < 0 {
                    "this tab has no previous history entry"
                } else {
                    "this tab has no forward history entry"
                })
            })?;
        let entry_id = entry.id;
        if before_unload == Some("accept") {
            self.clear_beforeunload_handlers(&page, &connection).await?;
        }
        let navigation = async {
            match wait_until {
                "none" => self
                    .runtime
                    .page_command(
                        &connection,
                        page.execute(NavigateToHistoryEntryParams::new(entry_id)),
                        "navigate browser history",
                    )
                    .await
                    .map(|_| ()),
                "dom_content_loaded" | "load" | "network_idle" => {
                    self.runtime
                        .page_command(
                            &connection,
                            page.execute(NavigateToHistoryEntryParams::new(entry_id)),
                            "navigate browser history",
                        )
                        .await?;
                    self.wait_for_history_entry(&page, &connection, index, wait_until, timeout_ms)
                        .await
                }
                _ => unreachable!("history wait state was validated before navigation"),
            }
        };
        let result = self
            .with_navigation_timeout(
                &page,
                &connection,
                before_unload,
                timeout_ms,
                wait_until != "none",
                navigation,
            )
            .await;
        self.finish_beforeunload_navigation(&target.id, before_unload, result)
            .await
    }

    async fn wait_for_history_entry(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        expected_index: i64,
        wait_until: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserToolError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let history = self
                .runtime
                .page_command(
                    connection,
                    page.execute(GetNavigationHistoryParams::default()),
                    "observe browser history navigation",
                )
                .await?;
            if history.result.current_index == expected_index {
                let ready_state = match page.evaluate_expression("document.readyState").await {
                    Ok(value) => value.into_value::<String>().map_err(|error| {
                        BrowserToolError::chrome_unavailable(format!(
                            "Chrome returned an invalid document state: {error}"
                        ))
                    })?,
                    Err(_) if Instant::now() < deadline => {
                        sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    Err(error) => {
                        return Err(map_cdp_error("read history document state", &error));
                    }
                };
                let ready = match wait_until {
                    "dom_content_loaded" => {
                        matches!(ready_state.as_str(), "interactive" | "complete")
                    }
                    "load" | "network_idle" => ready_state == "complete",
                    _ => false,
                };
                if ready {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(BrowserToolError::operation_timeout(format!(
                    "timed out waiting for history entry {expected_index} to reach {wait_until}"
                )));
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn reload(
        &self,
        target: &CdpTarget,
        ignore_cache: bool,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<(), BrowserToolError> {
        let wait_until = validated_navigation_wait_state(wait_until)?;
        let (page, connection) = self.page(&target.id).await?;
        if before_unload == Some("accept") {
            self.clear_beforeunload_handlers(&page, &connection).await?;
        }
        let navigation = async {
            let command =
                || page.execute(ReloadParams::builder().ignore_cache(ignore_cache).build());
            match wait_until {
                "none" => self
                    .runtime
                    .page_command(&connection, command(), "reload page")
                    .await
                    .map(|_| ()),
                "dom_content_loaded" => {
                    let mut events = self
                        .event_listener::<EventDomContentEventFired>(&page, &connection)
                        .await?;
                    self.runtime
                        .page_command(&connection, command(), "reload page")
                        .await?;
                    self.wait_for_lifecycle_event(&mut events, timeout_ms, "DOMContentLoaded")
                        .await
                }
                "load" | "network_idle" => {
                    let mut events = self
                        .event_listener::<EventLoadEventFired>(&page, &connection)
                        .await?;
                    self.runtime
                        .page_command(&connection, command(), "reload page")
                        .await?;
                    self.wait_for_lifecycle_event(&mut events, timeout_ms, "load")
                        .await
                }
                _ => unreachable!("reload wait state was validated before navigation"),
            }
        };
        let result = self
            .with_navigation_timeout(
                &page,
                &connection,
                before_unload,
                timeout_ms,
                wait_until != "none",
                navigation,
            )
            .await;
        self.finish_beforeunload_navigation(&target.id, before_unload, result)
            .await
    }

    async fn finish_beforeunload_navigation(
        &self,
        target_id: &str,
        policy: Option<&str>,
        result: Result<(), BrowserToolError>,
    ) -> Result<(), BrowserToolError> {
        if policy != Some("accept") {
            return result;
        }
        match result {
            Ok(()) => {
                // A committed document has no stash. A same-document history
                // move or reload keeps the original document and must restore
                // the guard that explicit acceptance temporarily removed.
                self.restore_beforeunload_handlers(target_id).await?;
                Ok(())
            }
            Err(error) => {
                if let Err(restore_error) = self.restore_beforeunload_handlers(target_id).await {
                    tracing::warn!(
                        target_id,
                        error = %restore_error.message,
                        "failed to restore beforeunload handlers after rejected navigation"
                    );
                }
                Err(error)
            }
        }
    }

    pub async fn screenshot(
        &self,
        target: &CdpTarget,
        full_page: bool,
        format: &str,
        quality: Option<u8>,
        clip: Option<Viewport>,
    ) -> Result<String, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(PageEnableParams::default()),
                "enable page for screenshot",
            )
            .await?;
        let capture_format = match format {
            "png" => CaptureScreenshotFormat::Png,
            "jpeg" => CaptureScreenshotFormat::Jpeg,
            "webp" => CaptureScreenshotFormat::Webp,
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unsupported screenshot format `{other}`"
                )));
            }
        };
        let mut builder = CaptureScreenshotParams::builder()
            .format(capture_format)
            .capture_beyond_viewport(full_page);

        if format != "png" {
            builder = builder.quality(i64::from(quality.unwrap_or(80).clamp(1, 100)));
        }

        let has_clip = clip.is_some();
        if let Some(clip) = clip {
            builder = builder.clip(clip).capture_beyond_viewport(true);
        }

        if full_page && !has_clip {
            let metrics = self
                .runtime
                .page_command(
                    &connection,
                    page.execute(GetLayoutMetricsParams::default()),
                    "read page layout metrics",
                )
                .await?;
            let size = metrics.result.css_content_size;
            let clip = Viewport::builder()
                .x(size.x)
                .y(size.y)
                .width(size.width.max(1.0))
                .height(size.height.max(1.0))
                .scale(1.0)
                .build()
                .map_err(BrowserToolError::invalid_input)?;
            builder = builder.clip(clip);
        }

        let response = self
            .runtime
            .page_command(
                &connection,
                page.execute(builder.build()),
                "capture page screenshot",
            )
            .await?;
        Ok(response.result.data.into())
    }

    pub async fn screenshot_clip_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Viewport, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let quads = self
            .runtime
            .page_command(
                &connection,
                page.execute(
                    GetContentQuadsParams::builder()
                        .backend_node_id(BackendNodeId::new(backend_node_id))
                        .build(),
                ),
                "read screenshot element content quad",
            )
            .await?;
        viewport_from_quad(
            quads
                .result
                .quads
                .first()
                .ok_or_else(|| {
                    BrowserToolError::element_not_actionable("element has no content quad")
                })?
                .inner(),
        )
    }

    pub async fn screenshot_clip_css(
        &self,
        target: &CdpTarget,
        selector: &str,
    ) -> Result<Viewport, BrowserToolError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let result = self
            .evaluate(
                target,
                &format!(
                    r#"(() => {{ const matches=document.querySelectorAll({selector_json}); if(matches.length===0)return {{state:"not_found"}}; if(matches.length>1)return {{state:"ambiguous",count:matches.length}}; const e=matches[0]; e.scrollIntoView({{block:"center",inline:"center"}}); const r=e.getBoundingClientRect(),s=getComputedStyle(e); if(r.width<=0||r.height<=0||s.visibility==="hidden"||s.display==="none")return {{state:"hidden"}}; return {{state:"ready",x:r.left,y:r.top,width:r.width,height:r.height}}; }})()"#
                ),
            )
            .await?
            .value
            .ok_or_else(|| BrowserToolError::chrome_unavailable("CSS screenshot target omitted result"))?;
        match result.get("state").and_then(Value::as_str) {
            Some("not_found") => return Err(BrowserToolError::element_not_found(selector)),
            Some("ambiguous") => {
                return Err(BrowserToolError::element_ambiguous(
                    selector,
                    result.get("count").and_then(Value::as_u64).unwrap_or(2) as usize,
                ));
            }
            _ => actionable_state(result.clone())?,
        }
        Viewport::builder()
            .x(result["x"].as_f64().unwrap_or(0.0))
            .y(result["y"].as_f64().unwrap_or(0.0))
            .width(result["width"].as_f64().unwrap_or(1.0).max(1.0))
            .height(result["height"].as_f64().unwrap_or(1.0).max(1.0))
            .scale(1.0)
            .build()
            .map_err(BrowserToolError::invalid_input)
    }

    pub async fn screenshot_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        format: &str,
        quality: Option<u8>,
    ) -> Result<String, BrowserToolError> {
        let clip = self
            .screenshot_clip_backend_node(target, backend_node_id)
            .await?;
        self.screenshot(target, false, format, quality, Some(clip))
            .await
    }

    pub async fn screenshot_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        format: &str,
        quality: Option<u8>,
    ) -> Result<String, BrowserToolError> {
        let clip = self.screenshot_clip_css(target, selector).await?;
        self.screenshot(target, false, format, quality, Some(clip))
            .await
    }

    pub async fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let result = match page.evaluate_expression(expression).await {
            Ok(result) => result,
            Err(error) => {
                return Err(self
                    .runtime
                    .page_error(&connection, "evaluate", error)
                    .await);
            }
        };
        let remote = result.object();
        Ok(EvaluateResult {
            value: remote.value.clone(),
            preview: remote
                .description
                .clone()
                .or_else(|| Some(remote.r#type.as_ref().to_string())),
        })
    }

    pub async fn evaluate_on_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        evaluation: ElementEvaluation<'_>,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        self.evaluate_on_object(&page, &connection, object_id, &object_group, evaluation)
            .await
    }

    pub async fn resolve_frame_css_backend_node(
        &self,
        target: &CdpTarget,
        frame_backend_node_id: i64,
        selector: &str,
    ) -> Result<i64, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(GetDocumentParams::builder().depth(1).pierce(true).build()),
                "enable DOM tree for framed CSS",
            )
            .await?;
        let frame = self
            .runtime
            .page_command(
                &connection,
                page.execute(
                    DescribeNodeParams::builder()
                        .backend_node_id(BackendNodeId::new(frame_backend_node_id))
                        .depth(1)
                        .build(),
                ),
                "resolve CSS frame reference",
            )
            .await?;
        let document_backend_node_id = frame
            .result
            .node
            .content_document
            .as_ref()
            .map(|node| node.backend_node_id)
            .ok_or_else(|| {
                BrowserToolError::element_not_actionable(
                    "frame reference does not expose a content document",
                )
            })?;
        let document_node_id = self
            .runtime
            .page_command(
                &connection,
                page.execute(PushNodesByBackendIdsToFrontendParams::new(vec![
                    document_backend_node_id,
                ])),
                "register referenced frame content document",
            )
            .await?
            .result
            .node_ids
            .into_iter()
            .next()
            .ok_or_else(|| {
                BrowserToolError::element_not_actionable(
                    "frame content document could not be registered",
                )
            })?;
        let matches = self
            .runtime
            .page_command(
                &connection,
                page.execute(QuerySelectorAllParams::new(document_node_id, selector)),
                "query CSS inside referenced frame",
            )
            .await?
            .result
            .node_ids;
        let node_id = match matches.as_slice() {
            [] => return Err(BrowserToolError::element_not_found(selector)),
            [node_id] => *node_id,
            matches => return Err(BrowserToolError::element_ambiguous(selector, matches.len())),
        };
        let node = self
            .runtime
            .page_command(
                &connection,
                page.execute(DescribeNodeParams::builder().node_id(node_id).build()),
                "resolve framed CSS backend node",
            )
            .await?;
        Ok(node.result.node.backend_node_id.inner().to_owned())
    }

    pub async fn resolve_css_backend_node(
        &self,
        target: &CdpTarget,
        selector: &str,
    ) -> Result<i64, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let document = self
            .runtime
            .page_command(
                &connection,
                page.execute(GetDocumentParams::builder().depth(1).pierce(true).build()),
                "enable DOM tree for snapshot root",
            )
            .await?;
        let matches = self
            .runtime
            .page_command(
                &connection,
                page.execute(QuerySelectorAllParams::new(
                    document.result.root.node_id,
                    selector,
                )),
                "query snapshot root",
            )
            .await?
            .result
            .node_ids;
        let node_id = match matches.as_slice() {
            [] => return Err(BrowserToolError::element_not_found(selector)),
            [node_id] => *node_id,
            matches => return Err(BrowserToolError::element_ambiguous(selector, matches.len())),
        };
        let node = self
            .runtime
            .page_command(
                &connection,
                page.execute(DescribeNodeParams::builder().node_id(node_id).build()),
                "resolve snapshot root",
            )
            .await?;
        Ok(*node.result.node.backend_node_id.inner())
    }

    pub async fn evaluate_on_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        evaluation: ElementEvaluation<'_>,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let count = page
            .evaluate_expression(format!("document.querySelectorAll({selector_json}).length"))
            .await
            .map_err(|error| map_cdp_error("resolve evaluation CSS target", &error))?
            .value()
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match count {
            0 => return Err(BrowserToolError::element_not_found(selector)),
            1 => {}
            count => {
                return Err(BrowserToolError::element_ambiguous(
                    selector,
                    count as usize,
                ));
            }
        }
        let object_group = next_object_group();
        let result = self
            .runtime
            .page_command(
                &connection,
                page.execute(
                    RuntimeEvaluateParams::builder()
                        .expression(format!("document.querySelector({selector_json})"))
                        .return_by_value(false)
                        .object_group(object_group.clone())
                        .build()
                        .map_err(BrowserToolError::invalid_input)?,
                ),
                "resolve evaluation CSS target",
            )
            .await?;
        let object_id = result
            .result
            .result
            .object_id
            .ok_or_else(|| BrowserToolError::element_stale(selector))?;
        self.evaluate_on_object(&page, &connection, object_id, &object_group, evaluation)
            .await
    }

    pub async fn document_revision(&self, target: &CdpTarget) -> Result<String, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.main_frame_loader_id(&page, &connection).await
    }

    pub async fn accessibility_snapshot(
        &self,
        target: &CdpTarget,
        depth: Option<usize>,
        include_bounds: bool,
    ) -> Result<RawAxSnapshot, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(AccessibilityEnableParams::default()),
                "enable accessibility snapshot",
            )
            .await?;
        let frame_tree = self
            .runtime
            .page_command(
                &connection,
                page.execute(GetFrameTreeParams::default()),
                "read page frame tree",
            )
            .await?
            .result
            .frame_tree;
        let mut frames = Vec::new();
        flatten_frame_tree(&frame_tree, None, &mut frames);

        let mut snapshot_frames = Vec::with_capacity(frames.len());
        for (frame, parent_frame_id) in frames {
            let response = self
                .runtime
                .page_command(
                    &connection,
                    page.execute(GetFullAxTreeParams {
                        depth: depth.map(|value| value as i64),
                        frame_id: Some(frame.id.clone()),
                    }),
                    "read accessibility tree",
                )
                .await?;
            let frame_id = frame.id.as_ref().to_string();
            let mut nodes = response
                .result
                .nodes
                .into_iter()
                .map(|node| raw_ax_node(node, &frame_id))
                .collect::<Vec<_>>();
            if include_bounds {
                for node in &mut nodes {
                    let Some(backend_node_id) = node.backend_node_id else {
                        continue;
                    };
                    let Ok(response) = page
                        .execute(
                            GetBoxModelParams::builder()
                                .backend_node_id(BackendNodeId::new(backend_node_id))
                                .build(),
                        )
                        .await
                    else {
                        continue;
                    };
                    node.bounds = bounds_label(response.result.model.border.inner());
                }
            }
            snapshot_frames.push(RawAxFrame {
                frame_id: frame_id.clone(),
                parent_frame_id,
                loader_id: frame.loader_id.as_ref().to_string(),
                url: frame.url,
                nodes,
            });
        }

        let refreshed = self.page_target(&target.id).await?;
        Ok(RawAxSnapshot {
            title: refreshed.title,
            url: refreshed.url,
            frames: snapshot_frames,
        })
    }

    pub async fn has_focus(&self, target: &CdpTarget) -> Result<bool, BrowserToolError> {
        // While a screencast's focus-emulation override is engaged, the page
        // reports itself focused and visible by design; that lie keeps frames
        // flowing but must not leak into user-focus checks (raw input would
        // be accepted for a background tab). The override only exists because
        // the page went hidden and no real activation has happened since, so
        // the honest answer is "not focused". `focus_tab` activates the
        // target, which clears the override before this check is polled.
        if self
            .runtime
            .screencast_focus_overrides
            .lock()
            .unwrap()
            .contains(&target.id)
        {
            return Ok(false);
        }
        let (page, connection) = self.page(&target.id).await?;
        match page
            .evaluate_expression("document.hasFocus() && document.visibilityState === 'visible'")
            .await
        {
            Ok(result) => result.into_value::<bool>().map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "Chrome returned an invalid focus result: {error}"
                ))
            }),
            Err(error) => Err(self
                .runtime
                .page_error(&connection, "check page focus", error)
                .await),
        }
    }

    pub async fn click(
        &self,
        target: &CdpTarget,
        selector: &str,
        _timeout_ms: u64,
        button: &str,
        count: u8,
        modifiers: &[String],
    ) -> Result<Value, BrowserToolError> {
        let backend_node_id = self.resolve_css_backend_node(target, selector).await?;
        let mut evidence = self
            .click_backend_node(target, backend_node_id, button, count, modifiers)
            .await?;
        if let Some(object) = evidence.as_object_mut() {
            object.insert("selector".to_string(), json!(selector));
        }
        Ok(evidence)
    }

    pub async fn click_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        button: &str,
        count: u8,
        modifiers: &[String],
    ) -> Result<Value, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        let actionability = self
            .call_on_element(
                &page,
                &connection,
                object_id,
                &object_group,
                r#"async function() {
  if (!this.isConnected) return { state: "stale" };
  this.scrollIntoView({ block: "center", inline: "center" });
  const first = this.getBoundingClientRect();
  await new Promise(resolve => this.ownerDocument.defaultView.setTimeout(resolve, 50));
  const rect = this.getBoundingClientRect();
  if (Math.abs(first.x - rect.x) > 0.5 || Math.abs(first.y - rect.y) > 0.5 || Math.abs(first.width - rect.width) > 0.5 || Math.abs(first.height - rect.height) > 0.5) return { state: "unstable" };
  const style = this.ownerDocument.defaultView.getComputedStyle(this);
  if (rect.width <= 0 || rect.height <= 0 || style.visibility === "hidden" || style.display === "none" || Number(style.opacity || "1") === 0 || style.pointerEvents === "none") return { state: "hidden" };
  if (this.matches(":disabled") || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  if ((this instanceof HTMLInputElement || this instanceof HTMLTextAreaElement) && this.readOnly) return { state: "not_editable" };
  const x = rect.left + rect.width / 2;
  const y = rect.top + rect.height / 2;
  const hit = this.ownerDocument.elementFromPoint(x, y);
  if (hit !== this && !this.contains(hit)) return { state: "obscured" };
  const tagName = this.tagName ? this.tagName.toLowerCase() : "";
  const type = (this.getAttribute("type") || (tagName === "button" ? "submit" : "")).toLowerCase();
  const form = this.form || this.closest("form");
  const submitControl = (tagName === "button" && type !== "button" && type !== "reset") || (tagName === "input" && (type === "submit" || type === "image"));
  const submitCandidate = submitControl && !!form && typeof form.requestSubmit === "function";
  const text = (this.innerText || this.textContent || "").replace(/\s+/g, " ").trim().slice(0, 120);
  const name = this.getAttribute("aria-label") || this.getAttribute("title") || text || this.getAttribute("value") || "";
  const topmostText = hit ? ((hit.innerText || hit.textContent || "").replace(/\s+/g, " ").trim().slice(0, 120)) : "";
  return {
    state: "ready",
    found: true,
    visible: true,
    x,
    y,
    submit_candidate: submitCandidate,
    resolved_element: {
      tag_name: tagName,
      id: this.id || null,
      type: type || null,
      role: this.getAttribute("role"),
      name,
      text,
      disabled: this.matches(":disabled") || this.getAttribute("aria-disabled") === "true",
      pointer_events: style.pointerEvents,
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
    },
    center_hit_test: {
      x,
      y,
      topmost_tag_name: hit && hit.tagName ? hit.tagName.toLowerCase() : null,
      topmost_id: hit ? (hit.id || null) : null,
      topmost_role: hit ? hit.getAttribute("role") : null,
      topmost_text: topmostText,
      target_contains_topmost: hit === this || this.contains(hit)
    }
  };
}"#,
                Vec::new(),
            )
            .await?;
        actionable_state(actionability.clone())?;
        let quads = self
            .runtime
            .page_command(
                &connection,
                page.execute(
                    GetContentQuadsParams::builder()
                        .backend_node_id(BackendNodeId::new(backend_node_id))
                        .build(),
                ),
                "read referenced element content quad",
            )
            .await?;
        let point = quads
            .result
            .quads
            .first()
            .ok_or_else(|| {
                BrowserToolError::element_not_actionable(
                    "element has no content quad in the top-level viewport",
                )
            })
            .and_then(|quad| content_quad_point(quad.inner()))?;
        let dispatch = self
            .dispatch_click(
                &page,
                &connection,
                point,
                PointerClick {
                    button,
                    count,
                    modifiers,
                },
            )
            .await?;
        let mut evidence = actionability;
        if let Some(object) = evidence.as_object_mut() {
            if let Some(resolved_element) = object
                .get_mut("resolved_element")
                .and_then(Value::as_object_mut)
            {
                resolved_element.insert("backend_node_id".to_string(), json!(backend_node_id));
            }
            object.insert("dispatch".to_string(), dispatch);
        }
        Ok(evidence)
    }

    pub async fn semantic_activate_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Value, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        let mut result = self
            .call_on_element(
                &page,
                &connection,
                object_id,
                &object_group,
                r#"function() {
  if (!this.isConnected) return { state: "stale" };
  if (this.matches(":disabled") || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  const tagName = this.tagName ? this.tagName.toLowerCase() : "";
  const type = (this.getAttribute("type") || (tagName === "button" ? "submit" : "")).toLowerCase();
  const submitCandidate = (tagName === "button" && type !== "button" && type !== "reset") || (tagName === "input" && (type === "submit" || type === "image"));
  const form = this.form || this.closest("form");
  if (!submitCandidate || !form || typeof form.requestSubmit !== "function") return { state: "not_submit_control" };
  form.requestSubmit(this);
  return {
    state: "ready",
    semantic_activation: "form_request_submit",
    submit_candidate: true,
    resolved_element: {
      backend_node_id: null,
      tag_name: tagName,
      id: this.id || null,
      type: type || null,
      role: this.getAttribute("role"),
      name: this.getAttribute("aria-label") || this.getAttribute("title") || (this.innerText || this.textContent || "").replace(/\s+/g, " ").trim().slice(0, 120)
    }
  };
}"#,
                Vec::new(),
            )
            .await?;
        actionable_state(result.clone())?;
        if let Some(object) = result.as_object_mut()
            && let Some(resolved_element) = object
                .get_mut("resolved_element")
                .and_then(Value::as_object_mut)
        {
            resolved_element.insert("backend_node_id".to_string(), json!(backend_node_id));
        }
        Ok(result)
    }

    pub async fn fill_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        let result = self
            .call_on_element(
                &page,
                &connection,
                object_id,
                &object_group,
                r#"function(value) {
  if (!this.isConnected) return { state: "stale" };
  const rect = this.getBoundingClientRect();
  const style = this.ownerDocument.defaultView.getComputedStyle(this);
  if (rect.width <= 0 || rect.height <= 0 || style.visibility === "hidden" || style.display === "none") return { state: "hidden" };
  if (this.matches(":disabled") || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  if (!(this instanceof HTMLInputElement || this instanceof HTMLTextAreaElement || this.isContentEditable)) return { state: "not_editable" };
  if (this instanceof HTMLInputElement || this instanceof HTMLTextAreaElement) {
    const prototype = this instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(prototype, "value").set;
    setter.call(this, value);
  } else {
    this.textContent = value;
  }
  this.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
  this.dispatchEvent(new Event("change", { bubbles: true }));
  return { state: "ready" };
}"#,
                vec![CallArgument::builder().value(value).build()],
            )
            .await?;
        actionable_state(result)
    }

    pub async fn type_text_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        text: &str,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        let result = self
            .call_on_element(
                &page,
                &connection,
                object_id,
                &object_group,
                r#"function() {
  if (!this.isConnected) return { state: "stale" };
  if (!(this instanceof HTMLInputElement || this instanceof HTMLTextAreaElement || this.isContentEditable)) return { state: "not_editable" };
  if (this.matches(":disabled") || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  this.focus({ preventScroll: true });
  return { state: "ready" };
}"#,
                Vec::new(),
            )
            .await?;
        actionable_state(result)?;
        self.runtime
            .page_command(
                &connection,
                page.execute(InsertTextParams::new(text)),
                "insert text into referenced element",
            )
            .await?;
        Ok(())
    }

    pub async fn type_text_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        text: &str,
    ) -> Result<(), BrowserToolError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let expression = format!(
            r#"(() => {{
  const matches = document.querySelectorAll({selector_json});
  if (matches.length === 0) return {{ state: "not_found" }};
  if (matches.length > 1) return {{ state: "ambiguous", count: matches.length }};
  const element = matches[0];
  if (!(element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element.isContentEditable)) return {{ state: "not_editable" }};
  if (element.matches(":disabled") || element.getAttribute("aria-disabled") === "true") return {{ state: "disabled" }};
  element.focus({{ preventScroll: true }});
  return {{ state: "ready" }};
}})()"#
        );
        let result = self
            .evaluate(target, &expression)
            .await?
            .value
            .ok_or_else(|| {
                BrowserToolError::chrome_unavailable("focus CSS target omitted result")
            })?;
        match result.get("state").and_then(Value::as_str) {
            Some("not_found") => return Err(BrowserToolError::element_not_found(selector)),
            Some("ambiguous") => {
                return Err(BrowserToolError::element_ambiguous(
                    selector,
                    result.get("count").and_then(Value::as_u64).unwrap_or(2) as usize,
                ));
            }
            _ => actionable_state(result)?,
        }
        self.type_text(target, text).await
    }

    pub async fn set_checked_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        checked: bool,
    ) -> Result<(), BrowserToolError> {
        self.mutate_backend_node(
            target,
            backend_node_id,
            r#"function(checked) {
  if (!this.isConnected) return { state: "stale" };
  if (!(this instanceof HTMLInputElement) || !["checkbox", "radio"].includes(this.type)) return { state: "not_checkable" };
  if (this.disabled || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  if (this.checked !== checked) {
    this.checked = checked;
    this.dispatchEvent(new Event("input", { bubbles: true }));
    this.dispatchEvent(new Event("change", { bubbles: true }));
  }
  return { state: "ready" };
}"#,
            vec![CallArgument::builder().value(checked).build()],
        )
        .await
    }

    pub async fn select_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        values: &[String],
    ) -> Result<(), BrowserToolError> {
        self.mutate_backend_node(
            target,
            backend_node_id,
            r#"function(values) {
  if (!this.isConnected) return { state: "stale" };
  if (!(this instanceof HTMLSelectElement)) return { state: "not_select" };
  if (this.disabled || this.getAttribute("aria-disabled") === "true") return { state: "disabled" };
  const wanted = new Set(values);
  const matched = new Set();
  for (const option of this.options) {
    if (wanted.has(option.value)) matched.add(option.value);
    if (wanted.has(option.label)) matched.add(option.label);
  }
  if (matched.size !== wanted.size) return { state: "option_not_found" };
  for (const option of this.options) {
    option.selected = wanted.has(option.value) || wanted.has(option.label);
  }
  this.dispatchEvent(new Event("input", { bubbles: true }));
  this.dispatchEvent(new Event("change", { bubbles: true }));
  return { state: "ready" };
}"#,
            vec![
                CallArgument::builder()
                    .value(serde_json::to_value(values).unwrap_or_default())
                    .build(),
            ],
        )
        .await
    }

    pub async fn select_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        values: &[String],
    ) -> Result<(), BrowserToolError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let values_json = serde_json::to_string(values)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let expression = format!(
            r#"(() => {{
  const matches = document.querySelectorAll({selector_json});
  if (matches.length === 0) return {{state:"not_found"}};
  if (matches.length > 1) return {{state:"ambiguous",count:matches.length}};
  const element = matches[0];
  if (!(element instanceof HTMLSelectElement)) return {{state:"not_select"}};
  if (element.disabled || element.getAttribute("aria-disabled") === "true") return {{state:"disabled"}};
  const wanted = new Set({values_json});
  const matched = new Set();
  for (const option of element.options) {{
    if (wanted.has(option.value)) matched.add(option.value);
    if (wanted.has(option.label)) matched.add(option.label);
  }}
  if (matched.size !== wanted.size) return {{state:"option_not_found"}};
  for (const option of element.options) option.selected = wanted.has(option.value) || wanted.has(option.label);
  element.dispatchEvent(new Event("input", {{bubbles:true}}));
  element.dispatchEvent(new Event("change", {{bubbles:true}}));
  return {{state:"ready"}};
}})()"#
        );
        self.css_mutation_result(target, selector, &expression)
            .await
    }

    pub async fn set_checked_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        checked: bool,
    ) -> Result<(), BrowserToolError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let expression = format!(
            r#"(() => {{
  const matches = document.querySelectorAll({selector_json});
  if (matches.length === 0) return {{state:"not_found"}};
  if (matches.length > 1) return {{state:"ambiguous",count:matches.length}};
  const element = matches[0];
  if (!(element instanceof HTMLInputElement) || !["checkbox","radio"].includes(element.type)) return {{state:"not_checkable"}};
  if (element.disabled || element.getAttribute("aria-disabled") === "true") return {{state:"disabled"}};
  if (element.checked !== {checked}) {{
    element.checked = {checked};
    element.dispatchEvent(new Event("input", {{bubbles:true}}));
    element.dispatchEvent(new Event("change", {{bubbles:true}}));
  }}
  return {{state:"ready"}};
}})()"#
        );
        self.css_mutation_result(target, selector, &expression)
            .await
    }

    pub async fn element_state_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Value, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        self.call_on_element(
            &page,
            &connection,
            object_id,
            &object_group,
            r#"function() {
  if (!this.isConnected) return { attached: false, visible: false };
  const rect = this.getBoundingClientRect();
  const style = this.ownerDocument.defaultView.getComputedStyle(this);
  const visible = rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity || "1") > 0;
  const disabled = this.matches(":disabled") || this.getAttribute("aria-disabled") === "true";
  const editable = !disabled && (this instanceof HTMLInputElement || this instanceof HTMLTextAreaElement || this.isContentEditable);
  const checked = "checked" in this ? Boolean(this.checked) : this.getAttribute("aria-checked") === "true";
  return { attached: true, visible, enabled: !disabled, editable, checked };
}"#,
            Vec::new(),
        )
        .await
    }

    pub async fn hover_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let quads = self
            .runtime
            .page_command(
                &connection,
                page.execute(
                    GetContentQuadsParams::builder()
                        .backend_node_id(BackendNodeId::new(backend_node_id))
                        .build(),
                ),
                "read referenced element content quad",
            )
            .await?;
        let point = quads
            .result
            .quads
            .first()
            .ok_or_else(|| BrowserToolError::element_not_actionable("element has no content quad"))
            .and_then(|quad| content_quad_point(quad.inner()))?;
        self.runtime
            .page_command(
                &connection,
                page.execute(
                    DispatchMouseEventParams::builder()
                        .r#type(DispatchMouseEventType::MouseMoved)
                        .x(point.x)
                        .y(point.y)
                        .button(MouseButton::None)
                        .build()
                        .map_err(BrowserToolError::invalid_input)?,
                ),
                "hover referenced element",
            )
            .await?;
        Ok(())
    }

    pub async fn drag_backend_nodes(
        &self,
        target: &CdpTarget,
        source_backend_node_id: i64,
        destination_backend_node_id: i64,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let mut points = Vec::new();
        for backend_node_id in [source_backend_node_id, destination_backend_node_id] {
            let quads = self
                .runtime
                .page_command(
                    &connection,
                    page.execute(
                        GetContentQuadsParams::builder()
                            .backend_node_id(BackendNodeId::new(backend_node_id))
                            .build(),
                    ),
                    "read drag endpoint content quad",
                )
                .await?;
            points.push(
                quads
                    .result
                    .quads
                    .first()
                    .ok_or_else(|| {
                        BrowserToolError::element_not_actionable(
                            "drag endpoint has no content quad",
                        )
                    })
                    .and_then(|quad| content_quad_point(quad.inner()))?,
            );
        }
        let source = points[0];
        let destination = points[1];
        for event in [
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseMoved)
                .x(source.x)
                .y(source.y)
                .button(MouseButton::None)
                .build(),
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MousePressed)
                .x(source.x)
                .y(source.y)
                .button(MouseButton::Left)
                .click_count(1)
                .build(),
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseMoved)
                .x(destination.x)
                .y(destination.y)
                .button(MouseButton::Left)
                .build(),
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseReleased)
                .x(destination.x)
                .y(destination.y)
                .button(MouseButton::Left)
                .click_count(1)
                .build(),
        ] {
            self.runtime
                .page_command(
                    &connection,
                    page.execute(event.map_err(BrowserToolError::invalid_input)?),
                    "dispatch drag input",
                )
                .await?;
        }
        Ok(())
    }

    pub async fn click_at(
        &self,
        target: &CdpTarget,
        x: f64,
        y: f64,
        button: &str,
        count: i64,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.dispatch_click(
            &page,
            &connection,
            ClickPoint { x, y },
            PointerClick {
                button,
                count: count as u8,
                modifiers,
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn scroll_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<(), BrowserToolError> {
        self.mutate_backend_node(
            target,
            backend_node_id,
            r#"function(x, y) { if (!this.isConnected) return {state:"stale"}; this.scrollBy(x, y); return {state:"ready"}; }"#,
            vec![
                CallArgument::builder().value(delta_x).build(),
                CallArgument::builder().value(delta_y).build(),
            ],
        )
        .await
    }

    pub async fn upload_files_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        paths: &[String],
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(
                    SetFileInputFilesParams::builder()
                        .files(paths)
                        .backend_node_id(BackendNodeId::new(backend_node_id))
                        .build()
                        .map_err(BrowserToolError::invalid_input)?,
                ),
                "set file input paths",
            )
            .await?;
        Ok(())
    }

    pub async fn drop_data_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        files: &Value,
        data: &Value,
    ) -> Result<(), BrowserToolError> {
        self.mutate_backend_node(
            target,
            backend_node_id,
            r#"async function(files, data) {
  if (!this.isConnected) return {state:"stale"};
  const transfer = new DataTransfer();
  for (const file of files) {
    const binary = atob(file.base64);
    const bytes = Uint8Array.from(binary, c => c.charCodeAt(0));
    transfer.items.add(new File([bytes], file.name, {type:file.media_type}));
  }
  for (const [type, value] of Object.entries(data)) transfer.setData(type, value);
  for (const name of ["dragenter", "dragover", "drop"]) this.dispatchEvent(new DragEvent(name, {bubbles:true, cancelable:true, dataTransfer:transfer}));
  return {state:"ready"};
}"#,
            vec![
                CallArgument::builder().value(files.clone()).build(),
                CallArgument::builder().value(data.clone()).build(),
            ],
        )
        .await
    }

    pub async fn handle_dialog(
        &self,
        target: &CdpTarget,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let mut builder = HandleJavaScriptDialogParams::builder().accept(accept);
        if let Some(prompt_text) = prompt_text {
            builder = builder.prompt_text(prompt_text);
        }
        self.runtime
            .page_command(
                &connection,
                page.execute(builder.build().map_err(BrowserToolError::invalid_input)?),
                "handle JavaScript dialog",
            )
            .await?;
        Ok(())
    }

    async fn mutate_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        function: &str,
        arguments: Vec<CallArgument>,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let (object_id, object_group) = self
            .resolve_backend_node(&page, &connection, backend_node_id)
            .await?;
        let result = self
            .call_on_element(
                &page,
                &connection,
                object_id,
                &object_group,
                function,
                arguments,
            )
            .await?;
        actionable_state(result)
    }

    async fn css_mutation_result(
        &self,
        target: &CdpTarget,
        selector: &str,
        expression: &str,
    ) -> Result<(), BrowserToolError> {
        let result = self
            .evaluate(target, expression)
            .await?
            .value
            .ok_or_else(|| BrowserToolError::chrome_unavailable("CSS operation omitted result"))?;
        match result.get("state").and_then(Value::as_str) {
            Some("not_found") => Err(BrowserToolError::element_not_found(selector)),
            Some("ambiguous") => Err(BrowserToolError::element_ambiguous(
                selector,
                result.get("count").and_then(Value::as_u64).unwrap_or(2) as usize,
            )),
            _ => actionable_state(result),
        }
    }

    pub async fn fill_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let value_json = serde_json::to_string(value)
            .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
        let expression = format!(
            r#"(() => {{
  const selector = {selector_json};
  const value = {value_json};
  const matches = document.querySelectorAll(selector);
  if (matches.length === 0) return {{ state: "not_found" }};
  if (matches.length > 1) return {{ state: "ambiguous", count: matches.length }};
  const element = matches[0];
  if (!element.isConnected) return {{ state: "stale" }};
  const rect = element.getBoundingClientRect();
  const style = getComputedStyle(element);
  if (rect.width <= 0 || rect.height <= 0 || style.visibility === "hidden" || style.display === "none") return {{ state: "hidden" }};
  if (element.matches(":disabled") || element.getAttribute("aria-disabled") === "true") return {{ state: "disabled" }};
  if (!(element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element.isContentEditable)) return {{ state: "not_editable" }};
  if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) {{
    const prototype = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(prototype, "value").set.call(element, value);
  }} else {{
    element.textContent = value;
  }}
  element.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: value }}));
  element.dispatchEvent(new Event("change", {{ bubbles: true }}));
  return {{ state: "ready" }};
}})()"#
        );
        let (page, _connection) = self.page(&target.id).await?;
        let result = page
            .evaluate_expression(expression)
            .await
            .map_err(|error| map_cdp_error("fill CSS target", &error))?;
        let result = result.value().cloned().ok_or_else(|| {
            BrowserToolError::chrome_unavailable("fill CSS target omitted result")
        })?;
        match result.get("state").and_then(Value::as_str) {
            Some("not_found") => Err(BrowserToolError::element_not_found(selector)),
            Some("ambiguous") => Err(BrowserToolError::element_ambiguous(
                selector,
                result.get("count").and_then(Value::as_u64).unwrap_or(2) as usize,
            )),
            _ => actionable_state(result),
        }
    }

    pub async fn type_text(&self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(InsertTextParams::new(text)),
                "insert text",
            )
            .await?;
        Ok(())
    }

    pub async fn press_key(
        &self,
        target: &CdpTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        let key_event = key_event_for(key, modifiers)?;
        let (page, connection) = self.page(&target.id).await?;

        for event_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let params = key_event.params(event_type)?;
            self.runtime
                .page_command(&connection, page.execute(params), "dispatch keyboard input")
                .await?;
        }
        Ok(())
    }

    pub async fn diagnostics_monitor(
        &self,
        target: &CdpTarget,
        sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
    ) -> Result<CdpDiagnosticsMonitor, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;

        self.runtime
            .page_command(
                &connection,
                page.execute(RuntimeEnableParams::default()),
                "enable runtime diagnostics",
            )
            .await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(LogEnableParams::default()),
                "enable log diagnostics",
            )
            .await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(
                    NetworkEnableParams::builder()
                        .max_total_buffer_size(NETWORK_TOTAL_BUFFER_BYTES)
                        .max_resource_buffer_size(NETWORK_RESOURCE_BUFFER_BYTES)
                        .build(),
                ),
                "enable network diagnostics",
            )
            .await?;

        let mut console = self
            .event_listener::<EventConsoleApiCalled>(&page, &connection)
            .await?;
        let mut log = self
            .event_listener::<EventEntryAdded>(&page, &connection)
            .await?;
        let mut request = self
            .event_listener::<EventRequestWillBeSent>(&page, &connection)
            .await?;
        let mut response = self
            .event_listener::<EventResponseReceived>(&page, &connection)
            .await?;
        let mut failed = self
            .event_listener::<EventLoadingFailed>(&page, &connection)
            .await?;
        let mut finished = self
            .event_listener::<EventLoadingFinished>(&page, &connection)
            .await?;
        let (stop_tx, mut stop_rx) = oneshot::channel();

        let monitor_page = page.clone();
        let task = tokio::spawn(async move {
            let mut console_since_discard = 0usize;
            loop {
                let event = tokio::select! {
                    _ = &mut stop_rx => break,
                    event = console.next() => {
                        console_since_discard += 1;
                        typed_event("Runtime.consoleAPICalled", event)
                    }
                    event = log.next() => typed_event("Log.entryAdded", event),
                    event = request.next() => typed_event("Network.requestWillBeSent", event),
                    event = response.next() => typed_event("Network.responseReceived", event),
                    event = failed.next() => typed_event("Network.loadingFailed", event),
                    event = finished.next() => typed_event("Network.loadingFinished", event),
                };

                match event {
                    Some(event) => sink(event),
                    None => break,
                }

                // Chrome pins every console argument as a remote object while
                // Runtime is enabled. We already delivered the event, so drop
                // Chrome's copies before a chatty page piles them up.
                if console_since_discard >= CONSOLE_DISCARD_INTERVAL {
                    console_since_discard = 0;
                    let _ = monitor_page
                        .execute(DiscardConsoleEntriesParams::default())
                        .await;
                }
            }

            // Enabled diagnostics domains keep buffering (console arguments,
            // response bodies) for the life of the target. The monitor is the
            // only consumer, so shut them off when it stops.
            let _ = monitor_page
                .execute(DiscardConsoleEntriesParams::default())
                .await;
            let _ = monitor_page.execute(RuntimeDisableParams::default()).await;
            let _ = monitor_page.execute(LogDisableParams::default()).await;
            let _ = monitor_page.execute(NetworkDisableParams::default()).await;
        });

        Ok(CdpDiagnosticsMonitor {
            stop: Some(stop_tx),
            task,
        })
    }

    pub async fn network_response_body(
        &self,
        target: &CdpTarget,
        request_id: &str,
    ) -> Result<Vec<u8>, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let response = self
            .runtime
            .page_command(
                &connection,
                page.execute(GetResponseBodyParams::new(RequestId::new(request_id))),
                "read network response body",
            )
            .await?;
        if response.result.base64_encoded {
            BASE64.decode(response.result.body).map_err(|error| {
                BrowserToolError::artifact_error(format!(
                    "network response body contained invalid base64: {error}"
                ))
            })
        } else {
            Ok(response.result.body.into_bytes())
        }
    }

    pub async fn network_request_body(
        &self,
        target: &CdpTarget,
        request_id: &str,
    ) -> Result<Option<String>, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        match self
            .runtime
            .page_command(
                &connection,
                page.execute(GetRequestPostDataParams::new(RequestId::new(request_id))),
                "read network request body",
            )
            .await
        {
            Ok(response) => Ok(Some(response.result.post_data)),
            Err(_) => Ok(None),
        }
    }

    #[allow(deprecated)]
    pub async fn emulate(
        &self,
        target: &CdpTarget,
        operation: &str,
        arguments: &serde_json::Map<String, Value>,
    ) -> Result<Value, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        match operation {
            "set_viewport" => {
                let width = json_i64(arguments, "width")?;
                let height = json_i64(arguments, "height")?;
                let scale = arguments
                    .get("device_scale_factor")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0);
                let mobile = arguments
                    .get("mobile")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let touch = arguments
                    .get("touch")
                    .and_then(Value::as_bool)
                    .unwrap_or(mobile);
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetDeviceMetricsOverrideParams::new(
                            width, height, scale, mobile,
                        )),
                        "set viewport emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetTouchEmulationEnabledParams::new(touch)),
                        "set touch emulation",
                    )
                    .await?;
            }
            "set_network" => {
                let preset = arguments
                    .get("preset")
                    .and_then(Value::as_str)
                    .unwrap_or("none");
                let (offline, latency, down, up) = match preset {
                    "offline" => (true, 0.0, 0.0, 0.0),
                    "slow_3g" => (false, 400.0, 50_000.0, 25_000.0),
                    "fast_3g" => (false, 150.0, 200_000.0, 100_000.0),
                    "slow_4g" => (false, 100.0, 500_000.0, 250_000.0),
                    "none" => (false, 0.0, -1.0, -1.0),
                    other => {
                        return Err(BrowserToolError::invalid_input(format!(
                            "unknown network preset `{other}`"
                        )));
                    }
                };
                let params = EmulateNetworkConditionsParams::new(
                    arguments
                        .get("offline")
                        .and_then(Value::as_bool)
                        .unwrap_or(offline),
                    arguments
                        .get("latency_ms")
                        .and_then(Value::as_f64)
                        .unwrap_or(latency),
                    arguments
                        .get("download_bytes_per_second")
                        .and_then(Value::as_f64)
                        .unwrap_or(down),
                    arguments
                        .get("upload_bytes_per_second")
                        .and_then(Value::as_f64)
                        .unwrap_or(up),
                );
                self.runtime
                    .page_command(&connection, page.execute(params), "set network emulation")
                    .await?;
            }
            "set_cpu" => {
                let slowdown = arguments
                    .get("slowdown")
                    .and_then(Value::as_f64)
                    .filter(|value| *value >= 1.0)
                    .ok_or_else(|| {
                        BrowserToolError::invalid_input("slowdown must be at least 1")
                    })?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetCpuThrottlingRateParams::new(slowdown)),
                        "set CPU emulation",
                    )
                    .await?;
            }
            "set_geolocation" => {
                let latitude = json_f64(arguments, "latitude")?;
                let longitude = json_f64(arguments, "longitude")?;
                let accuracy = arguments
                    .get("accuracy_meters")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0);
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(
                            SetGeolocationOverrideParams::builder()
                                .latitude(latitude)
                                .longitude(longitude)
                                .accuracy(accuracy)
                                .build(),
                        ),
                        "set geolocation emulation",
                    )
                    .await?;
            }
            "set_media" => {
                let mut builder = SetEmulatedMediaParams::builder();
                if let Some(media) = arguments.get("media").and_then(Value::as_str) {
                    builder = builder.media(media);
                }
                if let Some(scheme) = arguments.get("color_scheme").and_then(Value::as_str) {
                    builder = builder.feature(MediaFeature::new(
                        "prefers-color-scheme",
                        scheme.replace('_', "-"),
                    ));
                }
                if let Some(motion) = arguments.get("reduced_motion").and_then(Value::as_str) {
                    builder = builder.feature(MediaFeature::new(
                        "prefers-reduced-motion",
                        motion.replace('_', "-"),
                    ));
                }
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(builder.build()),
                        "set media emulation",
                    )
                    .await?;
            }
            "set_user_agent" => {
                let mut builder = SetUserAgentOverrideParams::builder()
                    .user_agent(json_str(arguments, "user_agent")?);
                if let Some(platform) = arguments.get("platform").and_then(Value::as_str) {
                    builder = builder.platform(platform);
                }
                if let Some(language) = arguments.get("accept_language").and_then(Value::as_str) {
                    builder = builder.accept_language(language);
                }
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(builder.build().map_err(BrowserToolError::invalid_input)?),
                        "set user agent emulation",
                    )
                    .await?;
            }
            "set_headers" => {
                let headers = arguments
                    .get("headers")
                    .cloned()
                    .ok_or_else(|| BrowserToolError::invalid_input("missing `headers`"))?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetExtraHttpHeadersParams::new(Headers::new(headers))),
                        "set extra HTTP headers",
                    )
                    .await?;
            }
            "reset_viewport" => {
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(ClearDeviceMetricsOverrideParams::default()),
                        "clear viewport emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetTouchEmulationEnabledParams::new(false)),
                        "clear touch emulation",
                    )
                    .await?;
            }
            "reset" => {
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(ClearDeviceMetricsOverrideParams::default()),
                        "clear viewport emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetTouchEmulationEnabledParams::new(false)),
                        "clear touch emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(EmulateNetworkConditionsParams::new(false, 0.0, -1.0, -1.0)),
                        "clear network emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetCpuThrottlingRateParams::new(1.0)),
                        "clear CPU emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetGeolocationOverrideParams::default()),
                        "clear geolocation emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetEmulatedMediaParams::default()),
                        "clear media emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetUserAgentOverrideParams::new("")),
                        "clear user agent emulation",
                    )
                    .await?;
                self.runtime
                    .page_command(
                        &connection,
                        page.execute(SetExtraHttpHeadersParams::new(Headers::new(json!({})))),
                        "clear extra HTTP headers",
                    )
                    .await?;
            }
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unknown emulation operation `{other}`"
                )));
            }
        }
        Ok(Value::Object(arguments.clone()))
    }

    pub async fn start_trace(
        &self,
        categories: Vec<String>,
        screenshots: bool,
    ) -> Result<CdpTraceCapture, BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let mut data = connection
            .browser
            .event_listener::<EventDataCollected>()
            .await
            .map_err(|error| map_cdp_error("subscribe to trace data", &error))?;
        let mut complete = connection
            .browser
            .event_listener::<EventTracingComplete>()
            .await
            .map_err(|error| map_cdp_error("subscribe to trace completion", &error))?;
        let mut categories = if categories.is_empty() {
            vec![
                "devtools.timeline".to_string(),
                "v8.execute".to_string(),
                "blink.user_timing".to_string(),
                "loading".to_string(),
            ]
        } else {
            categories
        };
        if screenshots {
            categories.push("disabled-by-default-devtools.screenshot".to_string());
        }
        let config = TraceConfig::builder()
            .included_categories(categories)
            .build();
        self.runtime
            .browser_command(
                &connection,
                connection.browser.execute(
                    TraceStartParams::builder()
                        .transfer_mode(StartTransferMode::ReportEvents)
                        .trace_config(config)
                        .build(),
                ),
                "start performance trace",
            )
            .await?;
        let task = tokio::spawn(async move {
            let mut events = Vec::new();
            loop {
                tokio::select! {
                    chunk = data.next() => match chunk {
                        Some(chunk) => events.extend(chunk.value.clone()),
                        None => break,
                    },
                    completed = complete.next() => {
                        if completed.is_some() {
                            break;
                        }
                    }
                }
            }
            events
        });
        Ok(CdpTraceCapture {
            client: self.clone(),
            task,
        })
    }

    pub async fn heap_snapshot(&self, target: &CdpTarget) -> Result<Vec<u8>, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(HeapEnableParams::default()),
                "enable heap profiler",
            )
            .await?;
        let mut chunks = self
            .event_listener::<EventAddHeapSnapshotChunk>(&page, &connection)
            .await?;
        let (done_tx, mut done_rx) = oneshot::channel::<()>();
        let collector = tokio::spawn(async move {
            let mut output = Vec::new();
            loop {
                tokio::select! {
                    chunk = chunks.next() => match chunk {
                        Some(chunk) => output.extend_from_slice(chunk.chunk.as_bytes()),
                        None => break,
                    },
                    _ = &mut done_rx => {
                        while let Ok(Some(chunk)) = timeout(Duration::from_millis(100), chunks.next()).await {
                            output.extend_from_slice(chunk.chunk.as_bytes());
                        }
                        break;
                    }
                }
            }
            output
        });
        self.runtime
            .page_command(
                &connection,
                page.execute(
                    TakeHeapSnapshotParams::builder()
                        .capture_numeric_value(true)
                        .build(),
                ),
                "capture heap snapshot",
            )
            .await?;
        let _ = done_tx.send(());
        collector.await.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("heap snapshot collector failed: {error}"))
        })
    }

    pub async fn start_screencast(
        &self,
        target: &CdpTarget,
        fps: u32,
        quality: u8,
        max_duration: Duration,
    ) -> Result<CdpScreencastCapture, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let mut events = self
            .event_listener::<EventScreencastFrame>(&page, &connection)
            .await?;
        let mut visibility = self
            .event_listener::<EventScreencastVisibilityChanged>(&page, &connection)
            .await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(
                    StartScreencastParams::builder()
                        .format(StartScreencastFormat::Jpeg)
                        .quality(i64::from(quality))
                        .every_nth_frame(1)
                        .build(),
                ),
                "start page screencast",
            )
            .await?;
        let (done_tx, mut done_rx) = oneshot::channel();
        let event_page = page.clone();
        let overrides = self.runtime.screencast_focus_overrides.clone();
        let override_target = target.id.clone();
        let task = tokio::spawn(async move {
            let deadline = Instant::now() + max_duration;
            let sample_stride = (60 / fps.max(1)).max(1) as usize;
            let max_frames = ((fps as f64) * max_duration.as_secs_f64().ceil()) as usize;
            let mut frame_index = 0usize;
            let mut frames = Vec::new();
            // The cast owns its frame guarantee (RFC 00010): if the page
            // goes hidden mid-cast (a page-spawned sibling tab was
            // activated), Chrome stops producing frames and the recording
            // would silently collapse. Focus emulation forces the page
            // back to visible, which restarts frame production. The shared
            // registry is the source of truth for whether the override is
            // engaged: `has_focus` consults it so the emulated focus never
            // leaks into user-focus checks, and `activate_target` clears it
            // when the tab becomes genuinely visible. If the page goes
            // hidden again after a real activation, the cast re-engages.
            //
            // Enabling the emulation makes Chrome re-report the page as
            // visible, so the first visible event after an enable is our
            // own echo, not a genuine visibility change. Later visible
            // events are genuine (the user returned to the tab or the
            // covering sibling closed) and disengage the override so
            // honest focus answers come back without waiting for the cast
            // to end.
            let mut visibility_open = true;
            let mut emulation_echo_pending = false;
            loop {
                tokio::select! {
                    _ = &mut done_rx => break,
                    changed = visibility.next(), if visibility_open => {
                        let Some(changed) = changed else {
                            visibility_open = false;
                            continue;
                        };
                        if changed.visible {
                            if emulation_echo_pending {
                                emulation_echo_pending = false;
                            } else if overrides.lock().unwrap().remove(&override_target) {
                                let _ = event_page
                                    .execute(SetFocusEmulationEnabledParams::new(false))
                                    .await;
                            }
                        } else if overrides.lock().unwrap().insert(override_target.clone()) {
                            emulation_echo_pending = true;
                            if event_page
                                .execute(SetFocusEmulationEnabledParams::new(true))
                                .await
                                .is_err()
                            {
                                emulation_echo_pending = false;
                                overrides.lock().unwrap().remove(&override_target);
                            } else if !overrides.lock().unwrap().contains(&override_target) {
                                // `activate_target` cleared the override while
                                // the enable was in flight; honor the clear so
                                // the emulation cannot outlive the registry
                                // entry that makes `has_focus` distrust it.
                                emulation_echo_pending = false;
                                let _ = event_page
                                    .execute(SetFocusEmulationEnabledParams::new(false))
                                    .await;
                            }
                        }
                    }
                    frame = events.next() => {
                        let Some(frame) = frame else { break };
                        let _ = event_page.execute(ScreencastFrameAckParams::new(frame.session_id)).await;
                        if Instant::now() <= deadline
                            && frames.len() < max_frames
                            && frame_index.is_multiple_of(sample_stride)
                        {
                            let encoded: String = frame.data.clone().into();
                            if let Ok(bytes) = BASE64.decode(encoded) {
                                frames.push(CapturedScreencastFrame { bytes });
                            }
                        }
                        frame_index += 1;
                    }
                }
            }
            if overrides.lock().unwrap().remove(&override_target) {
                let _ = event_page
                    .execute(SetFocusEmulationEnabledParams::new(false))
                    .await;
            }
            frames
        });
        Ok(CdpScreencastCapture {
            client: self.clone(),
            target_id: target.id.clone(),
            done: Some(done_tx),
            task,
        })
    }

    pub async fn stop_screencast(
        mut capture: CdpScreencastCapture,
    ) -> Result<Vec<CapturedScreencastFrame>, BrowserToolError> {
        let (page, connection) = capture.client.page(&capture.target_id).await?;
        capture
            .client
            .runtime
            .page_command(
                &connection,
                page.execute(StopScreencastParams::default()),
                "stop page screencast",
            )
            .await?;
        if let Some(done) = capture.done.take() {
            let _ = done.send(());
        }
        capture.task.await.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("screencast collector failed: {error}"))
        })
    }

    pub async fn stop_trace(capture: CdpTraceCapture) -> Result<Vec<Value>, BrowserToolError> {
        let connection = capture.client.runtime.connection().await?;
        capture
            .client
            .runtime
            .browser_command(
                &connection,
                connection.browser.execute(TraceEndParams::default()),
                "stop performance trace",
            )
            .await?;
        timeout(Duration::from_secs(10), capture.task)
            .await
            .map_err(|_| BrowserToolError::operation_timeout("trace completion timed out"))?
            .map_err(|error| {
                BrowserToolError::chrome_unavailable(format!("trace collector failed: {error}"))
            })
    }

    async fn with_navigation_dialog_policy<F>(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        policy: Option<&str>,
        operation: F,
    ) -> Result<(), BrowserToolError>
    where
        F: Future<Output = Result<(), BrowserToolError>>,
    {
        let Some(policy) = policy else {
            return operation.await;
        };
        let accept = match policy {
            "accept" => true,
            "dismiss" => false,
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unknown before_unload policy `{other}`"
                )));
            }
        };
        let mut dialogs = self
            .event_listener::<EventJavascriptDialogOpening>(page, connection)
            .await?;
        tokio::pin!(operation);
        loop {
            tokio::select! {
                result = &mut operation => return result,
                dialog = dialogs.next() => {
                    let Some(dialog) = dialog else {
                        return operation.await;
                    };
                    let dismissed_beforeunload =
                        !accept && Self::dismissed_dialog_cancels_navigation(&dialog.r#type);
                    self.runtime
                        .page_command(
                            connection,
                            page.execute(
                                HandleJavaScriptDialogParams::builder()
                                    .accept(accept)
                                    .build()
                                    .map_err(BrowserToolError::invalid_input)?,
                            ),
                            "handle navigation dialog",
                        )
                        .await?;
                    if dismissed_beforeunload {
                        // Dismissing beforeunload cancels the navigation, so
                        // there is no navigation completion left to await.
                        self.runtime.invalidate(connection.generation).await;
                        return Ok(());
                    }
                }
            }
        }
    }

    fn dismissed_dialog_cancels_navigation(dialog_type: &DialogType) -> bool {
        matches!(dialog_type, DialogType::Beforeunload)
    }

    async fn with_navigation_timeout<F>(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        policy: Option<&str>,
        timeout_ms: u64,
        waits_for_lifecycle: bool,
        operation: F,
    ) -> Result<(), BrowserToolError>
    where
        F: Future<Output = Result<(), BrowserToolError>>,
    {
        match timeout(
            navigation_timeout_budget(timeout_ms, waits_for_lifecycle),
            self.with_navigation_dialog_policy(page, connection, policy, operation),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.runtime.invalidate(connection.generation).await;
                Err(BrowserToolError::operation_timeout(
                    "navigation did not complete before the requested timeout",
                ))
            }
        }
    }

    async fn page(&self, target_id: &str) -> Result<(Page, RuntimeConnection), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let deadline = Instant::now() + PAGE_DISCOVERY_TIMEOUT;
        loop {
            match connection.browser.get_page(TargetId::new(target_id)).await {
                Ok(page) => return Ok((page, connection)),
                Err(CdpError::NotFound) if Instant::now() < deadline => {
                    sleep(PAGE_DISCOVERY_RETRY).await;
                }
                Err(CdpError::NotFound) => {
                    return Err(BrowserToolError::target_missing_for_target(target_id));
                }
                Err(error) => {
                    return Err(self
                        .runtime
                        .page_error(&connection, "open Chrome target session", error)
                        .await);
                }
            }
        }
    }

    async fn event_listener<T>(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
    ) -> Result<chromiumoxide::listeners::EventStream<T>, BrowserToolError>
    where
        T: chromiumoxide::cdp::IntoEventKind + Unpin,
    {
        match page.event_listener::<T>().await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let operation = format!("subscribe to {}", std::any::type_name::<T>());
                if invalidates_connection(&error) {
                    self.runtime.invalidate(connection.generation).await;
                }
                Err(map_cdp_error(&operation, &error))
            }
        }
    }

    async fn resolve_backend_node(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        backend_node_id: i64,
    ) -> Result<(RemoteObjectId, String), BrowserToolError> {
        let object_group = next_object_group();
        let resolved = self
            .runtime
            .page_command(
                connection,
                page.execute(
                    ResolveNodeParams::builder()
                        .backend_node_id(BackendNodeId::new(backend_node_id))
                        .object_group(object_group.clone())
                        .build(),
                ),
                "resolve element reference",
            )
            .await
            .map_err(|error| match error.code {
                crate::leases::BrowserToolErrorCode::ChromeUnavailable => {
                    BrowserToolError::element_stale("resolved node")
                }
                _ => error,
            })?;
        let object_id = resolved
            .result
            .object
            .object_id
            .ok_or_else(|| BrowserToolError::element_stale("resolved node"))?;
        Ok((object_id, object_group))
    }

    async fn call_on_element(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        object_id: RemoteObjectId,
        object_group: &str,
        function: &str,
        arguments: Vec<CallArgument>,
    ) -> Result<Value, BrowserToolError> {
        let response = async {
            self.runtime
                .page_command(
                    connection,
                    page.execute(
                        CallFunctionOnParams::builder()
                            .function_declaration(function)
                            .object_id(object_id)
                            .arguments(arguments)
                            .return_by_value(true)
                            .await_promise(true)
                            .object_group(object_group)
                            .build()
                            .map_err(BrowserToolError::invalid_input)?,
                    ),
                    "inspect referenced element",
                )
                .await
        }
        .await;
        // Release on every exit path, including transport failures, so a
        // timed-out call cannot leave the element pinned.
        release_object_group(page, object_group).await;
        let response = response?;
        if let Some(exception) = response.result.exception_details {
            return Err(BrowserToolError::element_not_actionable(format!(
                "element operation failed: {}",
                exception.text
            )));
        }
        response.result.result.value.ok_or_else(|| {
            BrowserToolError::chrome_unavailable("element operation omitted its result")
        })
    }

    async fn evaluate_on_object(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        object_id: RemoteObjectId,
        object_group: &str,
        evaluation: ElementEvaluation<'_>,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let response = async {
            let function = match evaluation.mode {
                "expression" if evaluation.args.is_empty() => {
                    format!("function() {{ return ({}); }}", evaluation.source)
                }
                "expression" => {
                    return Err(BrowserToolError::invalid_input(
                        "evaluate arguments require mode `function`",
                    ));
                }
                "function" => evaluation.source.to_string(),
                other => {
                    return Err(BrowserToolError::invalid_input(format!(
                        "unknown evaluation mode `{other}`"
                    )));
                }
            };
            let arguments = evaluation
                .args
                .iter()
                .cloned()
                .map(|value| CallArgument::builder().value(value).build())
                .collect::<Vec<_>>();
            self.runtime
                .page_command(
                    connection,
                    page.execute(
                        CallFunctionOnParams::builder()
                            .function_declaration(function)
                            .object_id(object_id)
                            .arguments(arguments)
                            .return_by_value(true)
                            .await_promise(evaluation.await_promise)
                            .object_group(object_group)
                            .build()
                            .map_err(BrowserToolError::invalid_input)?,
                    ),
                    "evaluate on element target",
                )
                .await
        }
        .await;
        // Release on every exit path — rejected inputs and transport
        // failures must not leave the resolved element pinned.
        release_object_group(page, object_group).await;
        let response = response?;
        if let Some(exception) = response.result.exception_details {
            return Err(BrowserToolError::invalid_input(format!(
                "evaluation failed: {}",
                exception.text
            )));
        }
        let remote = response.result.result;
        Ok(EvaluateResult {
            value: remote.value,
            preview: remote
                .description
                .or_else(|| Some(remote.r#type.as_ref().to_string())),
        })
    }

    async fn dispatch_click(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        point: ClickPoint,
        click: PointerClick<'_>,
    ) -> Result<Value, BrowserToolError> {
        let button = match click.button {
            "left" => MouseButton::Left,
            "middle" => MouseButton::Middle,
            "right" => MouseButton::Right,
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unknown mouse button `{other}`"
                )));
            }
        };
        if !(1..=2).contains(&click.count) {
            return Err(BrowserToolError::invalid_input(
                "click count must be 1 or 2",
            ));
        }
        let modifiers = i64::from(modifier_bits(click.modifiers)?);
        let mut dialogs = self
            .event_listener::<EventJavascriptDialogOpening>(page, connection)
            .await?;
        self.runtime
            .page_command(
                connection,
                page.execute(
                    DispatchMouseEventParams::builder()
                        .r#type(DispatchMouseEventType::MouseMoved)
                        .x(point.x)
                        .y(point.y)
                        .button(MouseButton::None)
                        .modifiers(modifiers)
                        .build()
                        .map_err(BrowserToolError::invalid_input)?,
                ),
                "move mouse to click target",
            )
            .await?;
        for click_count in 1..=click.count {
            for event_type in [
                DispatchMouseEventType::MousePressed,
                DispatchMouseEventType::MouseReleased,
            ] {
                let event = DispatchMouseEventParams::builder()
                    .r#type(event_type.clone())
                    .x(point.x)
                    .y(point.y)
                    .button(button.clone())
                    .click_count(i64::from(click_count))
                    .modifiers(modifiers)
                    .build()
                    .map_err(BrowserToolError::invalid_input)?;
                let command = self.runtime.page_command(
                    connection,
                    page.execute(event),
                    "dispatch mouse input",
                );
                if event_type == DispatchMouseEventType::MouseReleased {
                    let release_delivery;
                    tokio::pin!(command);
                    tokio::select! {
                        biased;
                        result = &mut command => {
                            result?;
                            release_delivery = "chrome_ack";
                        }
                        dialog = dialogs.next() => {
                            if dialog.is_some() {
                                release_delivery = "dialog_event";
                            } else {
                                command.await?;
                                release_delivery = "chrome_ack";
                            }
                        }
                        _ = sleep(Duration::from_millis(250)) => {
                            release_delivery = "delivery_uncertain";
                        }
                    }
                    if click_count == click.count {
                        return Ok(json!({
                            "release_delivery": release_delivery,
                            "delivery_uncertain": release_delivery == "delivery_uncertain"
                        }));
                    }
                } else {
                    command.await?;
                }
            }
        }
        Ok(json!({
            "release_delivery": "chrome_ack",
            "delivery_uncertain": false
        }))
    }
}

fn navigation_timeout_budget(timeout_ms: u64, waits_for_lifecycle: bool) -> Duration {
    // Lifecycle waits start after listener setup and Page.navigate returns.
    // Keep the outer watchdog as a stuck-command/dialog backstop while giving
    // the requested lifecycle wait and its committed-document recovery time
    // to finish instead of cancelling them at the same deadline.
    let requested = Duration::from_millis(timeout_ms);
    if waits_for_lifecycle {
        requested.saturating_add(NAVIGATION_RECOVERY_GRACE)
    } else {
        requested
    }
}

#[derive(Debug)]
struct CdpRuntime {
    endpoint: CdpEndpoint,
    state: Mutex<RuntimeState>,
    /// Targets whose screencast has engaged focus emulation to keep frames
    /// flowing while the page is hidden (RFC 00010). The emulation makes the
    /// page report itself focused and visible, so user-focus checks must
    /// consult this registry instead of trusting the page while a cast's
    /// override is engaged.
    screencast_focus_overrides: Arc<StdMutex<HashSet<String>>>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    connection: Option<ConnectedBrowser>,
    generation: u64,
}

#[derive(Debug)]
struct ConnectedBrowser {
    generation: u64,
    browser: Arc<Browser>,
    handler: JoinHandle<Result<(), String>>,
}

#[derive(Debug, Clone)]
struct RuntimeConnection {
    generation: u64,
    browser: Arc<Browser>,
}

impl CdpRuntime {
    fn new(endpoint: CdpEndpoint) -> Self {
        Self {
            endpoint,
            state: Mutex::new(RuntimeState::default()),
            screencast_focus_overrides: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    async fn connection(&self) -> Result<RuntimeConnection, BrowserToolError> {
        let mut state = self.state.lock().await;
        if let Some(connection) = &state.connection
            && !connection.handler.is_finished()
        {
            return Ok(RuntimeConnection {
                generation: connection.generation,
                browser: connection.browser.clone(),
            });
        }

        if let Some(connection) = state.connection.take() {
            connection.handler.abort();
        }

        let endpoint = self.endpoint.origin().as_str().to_string();
        let (mut browser, mut handler) = Browser::connect_with_config(
            endpoint.clone(),
            HandlerConfig {
                viewport: None,
                ..HandlerConfig::default()
            },
        )
        .await
        .map_err(|error| {
            BrowserToolError::chrome_unavailable(format!(
                "failed to connect Chromiumoxide to `{endpoint}`: {error}"
            ))
        })?;
        let handler_task = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                result.map_err(|error| error.to_string())?;
            }
            Err("Chromiumoxide handler ended".to_string())
        });
        let existing_targets = match browser.fetch_targets().await {
            Ok(targets) => targets,
            Err(error) => {
                handler_task.abort();
                return Err(map_cdp_error("register existing Chrome targets", &error));
            }
        };
        if existing_targets
            .iter()
            .any(|target| target.r#type == "page")
        {
            // Chromiumoxide attaches fetched targets asynchronously. Waiting before the first
            // page lookup prevents it from caching the transient attachment session.
            sleep(EXISTING_TARGET_REGISTRATION_DELAY).await;
        }
        let browser = Arc::new(browser);
        state.generation += 1;
        let generation = state.generation;
        state.connection = Some(ConnectedBrowser {
            generation,
            browser: browser.clone(),
            handler: handler_task,
        });

        Ok(RuntimeConnection {
            generation,
            browser,
        })
    }

    async fn browser_command<T, F>(
        &self,
        connection: &RuntimeConnection,
        future: F,
        operation: &str,
    ) -> Result<T, BrowserToolError>
    where
        F: Future<Output = Result<T, CdpError>>,
    {
        match future.await {
            Ok(value) => Ok(value),
            Err(error) => {
                if invalidates_connection(&error) {
                    self.invalidate(connection.generation).await;
                }
                Err(map_cdp_error(operation, &error))
            }
        }
    }

    async fn page_command<T, F>(
        &self,
        connection: &RuntimeConnection,
        future: F,
        operation: &str,
    ) -> Result<T, BrowserToolError>
    where
        F: Future<Output = Result<T, CdpError>>,
    {
        self.browser_command(connection, future, operation).await
    }

    async fn page_error(
        &self,
        connection: &RuntimeConnection,
        operation: &str,
        error: CdpError,
    ) -> BrowserToolError {
        if invalidates_connection(&error) {
            self.invalidate(connection.generation).await;
        }
        map_cdp_error(operation, &error)
    }

    async fn invalidate(&self, generation: u64) {
        let mut state = self.state.lock().await;
        if state
            .connection
            .as_ref()
            .is_some_and(|connection| connection.generation == generation)
            && let Some(connection) = state.connection.take()
        {
            connection.handler.abort();
        }
    }

    #[cfg(test)]
    async fn disconnect(&self) {
        let mut state = self.state.lock().await;
        if let Some(connection) = state.connection.take() {
            connection.handler.abort();
        }
    }
}

impl Drop for CdpRuntime {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock()
            && let Some(connection) = state.connection.take()
        {
            connection.handler.abort();
        }
    }
}

fn invalidates_connection(error: &CdpError) -> bool {
    matches!(
        error,
        CdpError::Ws(_)
            | CdpError::Io(_)
            | CdpError::NoResponse
            | CdpError::ChannelSendError(_)
            | CdpError::UnexpectedWsMessage(_)
    )
}

fn map_cdp_error(operation: &str, error: &CdpError) -> BrowserToolError {
    match error {
        CdpError::JavascriptException(details) => {
            BrowserToolError::invalid_input(format!("{operation} failed: {}", details.text))
        }
        CdpError::Timeout => BrowserToolError::operation_timeout(format!("{operation} timed out")),
        CdpError::NotFound => BrowserToolError::chrome_unavailable(format!(
            "{operation} failed because Chrome no longer exposes the target"
        )),
        _ => BrowserToolError::chrome_unavailable(format!("{operation} failed: {error}")),
    }
}

fn validated_navigation_wait_state(wait_until: Option<&str>) -> Result<&str, BrowserToolError> {
    let wait_until = wait_until.unwrap_or("load");
    if matches!(
        wait_until,
        "none" | "dom_content_loaded" | "load" | "network_idle"
    ) {
        Ok(wait_until)
    } else {
        Err(BrowserToolError::invalid_input(format!(
            "unknown navigation wait state `{wait_until}`"
        )))
    }
}

fn committed_navigation_reached_wait_state(
    expected_loader_id: &str,
    committed_loader_id: &str,
    ready_state: &str,
    event_name: &str,
) -> bool {
    if committed_loader_id != expected_loader_id {
        return false;
    }
    match event_name {
        "DOMContentLoaded" => matches!(ready_state, "interactive" | "complete"),
        "load" => ready_state == "complete",
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CdpDiagnosticEvent {
    Console {
        level: String,
        text: String,
        timestamp_ms: Option<u64>,
    },
    Network(NetworkEvent),
}

pub struct CdpDiagnosticsMonitor {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

pub struct CdpTraceCapture {
    client: CdpClient,
    task: JoinHandle<Vec<Value>>,
}

pub struct CapturedScreencastFrame {
    pub bytes: Vec<u8>,
}

pub struct CdpScreencastCapture {
    client: CdpClient,
    target_id: String,
    done: Option<oneshot::Sender<()>>,
    task: JoinHandle<Vec<CapturedScreencastFrame>>,
}

impl CdpDiagnosticsMonitor {
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Stop the monitor and wait for its cleanup (domain disables) to
    /// finish. Callers that start a replacement monitor on the same target
    /// must use this instead of dropping, or the old task's `Runtime.disable`
    /// can land after the new monitor enabled the domain and silence it.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Bound the wait so a wedged Chrome cannot hang the caller; on
        // timeout the abort in Drop kills the cleanup task, which also
        // prevents any late disables.
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.task).await;
    }
}

impl Drop for CdpDiagnosticsMonitor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            // The stop signal breaks the event loop, and the task then
            // disables the diagnostics domains before exiting. Aborting
            // here would kill that cleanup, so let the task finish on its
            // own (its commands time out if Chrome is gone).
            let _ = stop.send(());
        } else {
            self.task.abort();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,

    #[serde(rename = "type")]
    pub target_type: String,

    pub title: String,
    pub url: String,
}

impl From<&CdpTarget> for TabSnapshot {
    fn from(target: &CdpTarget) -> Self {
        TabSnapshot::new(&target.id, &target.title, &target.url, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ClickPoint {
    x: f64,
    y: f64,
}

struct PointerClick<'a> {
    button: &'a str,
    count: u8,
    modifiers: &'a [String],
}

fn content_quad_point(quad: &[f64]) -> Result<ClickPoint, BrowserToolError> {
    if quad.len() != 8 {
        return Err(BrowserToolError::chrome_unavailable(format!(
            "element content quad contained {} coordinates instead of 8",
            quad.len()
        )));
    }
    Ok(ClickPoint {
        x: (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0,
        y: (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0,
    })
}

fn viewport_from_quad(quad: &[f64]) -> Result<Viewport, BrowserToolError> {
    if quad.len() != 8 {
        return Err(BrowserToolError::element_not_actionable(format!(
            "element content quad contained {} coordinates instead of 8",
            quad.len()
        )));
    }
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let min_x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.into_iter().fold(f64::INFINITY, f64::min);
    let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max);
    Viewport::builder()
        .x(min_x)
        .y(min_y)
        .width((max_x - min_x).max(1.0))
        .height((max_y - min_y).max(1.0))
        .scale(1.0)
        .build()
        .map_err(BrowserToolError::invalid_input)
}

fn actionable_state(value: Value) -> Result<(), BrowserToolError> {
    match value.get("state").and_then(Value::as_str) {
        Some("ready") => Ok(()),
        Some("stale") => Err(BrowserToolError::element_stale("referenced element")),
        Some("hidden") => Err(BrowserToolError::element_not_actionable(
            "element is not visible",
        )),
        Some("disabled") => Err(BrowserToolError::element_not_actionable(
            "element is disabled",
        )),
        Some("obscured") => Err(BrowserToolError::element_not_actionable(
            "element is obscured at its action point",
        )),
        Some("unstable") => Err(BrowserToolError::element_not_actionable(
            "element geometry did not stabilize before input",
        )),
        Some("not_editable") => Err(BrowserToolError::element_not_actionable(
            "element is not editable",
        )),
        Some("not_checkable") => Err(BrowserToolError::element_not_actionable(
            "element is not a checkbox or radio control",
        )),
        Some("not_select") => Err(BrowserToolError::element_not_actionable(
            "element is not a select control",
        )),
        Some("option_not_found") => Err(BrowserToolError::element_not_actionable(
            "one or more requested select options were not found",
        )),
        Some("not_submit_control") => Err(BrowserToolError::element_not_actionable(
            "element is not a form submit control",
        )),
        Some(state) => Err(BrowserToolError::chrome_unavailable(format!(
            "element operation returned unknown state `{state}`"
        ))),
        None => Err(BrowserToolError::chrome_unavailable(
            "element operation omitted its state",
        )),
    }
}

fn flatten_frame_tree(
    tree: &FrameTree,
    parent_frame_id: Option<String>,
    output: &mut Vec<(Frame, Option<String>)>,
) {
    let frame_id = tree.frame.id.as_ref().to_string();
    output.push((tree.frame.clone(), parent_frame_id));
    if let Some(children) = &tree.child_frames {
        for child in children {
            flatten_frame_tree(child, Some(frame_id.clone()), output);
        }
    }
}

fn raw_ax_node(node: AxNode, containing_frame_id: &str) -> RawAxNode {
    RawAxNode {
        node_id: node.node_id.as_ref().to_string(),
        parent_id: node.parent_id.map(|id| id.as_ref().to_string()),
        child_ids: node
            .child_ids
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.as_ref().to_string())
            .collect(),
        backend_node_id: node.backend_dom_node_id.map(|id| *id.inner()),
        frame_id: node
            .frame_id
            .map(|id| id.as_ref().to_string())
            .unwrap_or_else(|| containing_frame_id.to_string()),
        role: node
            .role
            .as_ref()
            .and_then(ax_value_text)
            .unwrap_or_default(),
        name: node
            .name
            .as_ref()
            .and_then(ax_value_text)
            .unwrap_or_default(),
        value: node.value.as_ref().and_then(ax_value_text),
        properties: node
            .properties
            .unwrap_or_default()
            .into_iter()
            .filter_map(|property| {
                ax_value_text(&property.value)
                    .map(|value| (property.name.as_ref().to_string(), value))
            })
            .collect(),
        ignored: node.ignored,
        bounds: None,
    }
}

fn bounds_label(quad: &[f64]) -> Option<String> {
    let points = quad.chunks_exact(2).collect::<Vec<_>>();
    if points.len() != 4 || points.iter().any(|point| point.len() != 2) {
        return None;
    }
    let min_x = points
        .iter()
        .map(|point| point[0])
        .fold(f64::INFINITY, f64::min);
    let max_x = points
        .iter()
        .map(|point| point[0])
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = points
        .iter()
        .map(|point| point[1])
        .fold(f64::INFINITY, f64::min);
    let max_y = points
        .iter()
        .map(|point| point[1])
        .fold(f64::NEG_INFINITY, f64::max);
    Some(format!(
        "{min_x:.1},{min_y:.1},{:.1},{:.1}",
        max_x - min_x,
        max_y - min_y
    ))
}

fn ax_value_text(value: &AxValue) -> Option<String> {
    match value.value.as_ref()? {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        other => serde_json::to_string(other).ok(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyEvent {
    key: String,
    code: String,
    text: Option<String>,
    modifiers: u8,
    windows_virtual_key_code: Option<u16>,
}

impl KeyEvent {
    fn params(
        &self,
        event_type: DispatchKeyEventType,
    ) -> Result<DispatchKeyEventParams, BrowserToolError> {
        let is_key_down = event_type == DispatchKeyEventType::KeyDown;
        let mut builder = DispatchKeyEventParams::builder()
            .r#type(event_type)
            .key(&self.key)
            .code(&self.code)
            .modifiers(i64::from(self.modifiers));
        if is_key_down && let Some(text) = &self.text {
            builder = builder.text(text).unmodified_text(text);
        }
        if let Some(code) = self.windows_virtual_key_code {
            builder = builder
                .windows_virtual_key_code(i64::from(code))
                .native_virtual_key_code(i64::from(code));
        }
        builder.build().map_err(BrowserToolError::invalid_input)
    }
}

fn key_event_for(key: &str, modifiers: &[String]) -> Result<KeyEvent, BrowserToolError> {
    let modifiers = modifier_bits(modifiers)?;
    let named = match key {
        "Enter" => Some(("Enter", "Enter", 13)),
        "Tab" => Some(("Tab", "Tab", 9)),
        "Escape" | "Esc" => Some(("Escape", "Escape", 27)),
        "Backspace" => Some(("Backspace", "Backspace", 8)),
        "Delete" => Some(("Delete", "Delete", 46)),
        "ArrowLeft" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "ArrowRight" => Some(("ArrowRight", "ArrowRight", 39)),
        "ArrowUp" => Some(("ArrowUp", "ArrowUp", 38)),
        "ArrowDown" => Some(("ArrowDown", "ArrowDown", 40)),
        "Home" => Some(("Home", "Home", 36)),
        "End" => Some(("End", "End", 35)),
        "PageUp" => Some(("PageUp", "PageUp", 33)),
        "PageDown" => Some(("PageDown", "PageDown", 34)),
        "Space" => Some((" ", "Space", 32)),
        _ => None,
    };

    if let Some((mapped_key, code, virtual_key)) = named {
        return Ok(KeyEvent {
            key: mapped_key.to_string(),
            code: code.to_string(),
            text: (mapped_key == " ").then(|| " ".to_string()),
            modifiers,
            windows_virtual_key_code: Some(virtual_key),
        });
    }

    let mut chars = key.chars();
    let Some(ch) = chars.next() else {
        return Err(BrowserToolError::invalid_input("key must not be empty"));
    };
    if chars.next().is_some() {
        return Err(BrowserToolError::invalid_input(format!(
            "unsupported key `{key}`; use a single printable character or common named key"
        )));
    }
    if ch.is_control() {
        return Err(BrowserToolError::invalid_input(format!(
            "unsupported control key `{key}`"
        )));
    }

    Ok(KeyEvent {
        key: key.to_string(),
        code: printable_code(ch),
        text: Some(key.to_string()),
        modifiers,
        windows_virtual_key_code: Some(ch.to_ascii_uppercase() as u16),
    })
}

fn modifier_bits(modifiers: &[String]) -> Result<u8, BrowserToolError> {
    let mut bits = 0;
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "alt" => bits |= 1,
            "control" | "ctrl" => bits |= 2,
            "meta" | "command" => bits |= 4,
            "shift" => bits |= 8,
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unsupported key modifier `{other}`"
                )));
            }
        }
    }
    Ok(bits)
}

fn printable_code(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        return format!("Key{}", ch.to_ascii_uppercase());
    }
    if ch.is_ascii_digit() {
        return format!("Digit{ch}");
    }
    match ch {
        ' ' => "Space".to_string(),
        '-' => "Minus".to_string(),
        '=' => "Equal".to_string(),
        '[' => "BracketLeft".to_string(),
        ']' => "BracketRight".to_string(),
        '\\' => "Backslash".to_string(),
        ';' => "Semicolon".to_string(),
        '\'' => "Quote".to_string(),
        ',' => "Comma".to_string(),
        '.' => "Period".to_string(),
        '/' => "Slash".to_string(),
        '`' => "Backquote".to_string(),
        _ => String::new(),
    }
}

fn typed_event<T: Serialize>(method: &str, event: Option<Arc<T>>) -> Option<CdpDiagnosticEvent> {
    let event = event?;
    let params = serde_json::to_value(&*event).ok()?;
    diagnostic_event(&json!({ "method": method, "params": params }))
}

fn diagnostic_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    match value.get("method").and_then(Value::as_str)? {
        "Runtime.consoleAPICalled" => runtime_console_event(value),
        "Log.entryAdded" => log_entry_event(value),
        "Network.requestWillBeSent" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "request".to_string(),
            request_id: value
                .pointer("/params/requestId")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: value
                .pointer("/params/request/url")
                .and_then(Value::as_str)
                .map(str::to_string),
            method: value
                .pointer("/params/request/method")
                .and_then(Value::as_str)
                .map(str::to_string),
            resource_type: value
                .pointer("/params/type")
                .and_then(Value::as_str)
                .map(str::to_string),
            mime_type: None,
            headers: diagnostic_headers(value.pointer("/params/request/headers")),
            status: None,
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.responseReceived" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "response".to_string(),
            request_id: value
                .pointer("/params/requestId")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: value
                .pointer("/params/response/url")
                .and_then(Value::as_str)
                .map(str::to_string),
            method: None,
            resource_type: value
                .pointer("/params/type")
                .and_then(Value::as_str)
                .map(str::to_string),
            mime_type: value
                .pointer("/params/response/mimeType")
                .and_then(Value::as_str)
                .map(str::to_string),
            headers: diagnostic_headers(value.pointer("/params/response/headers")),
            status: value
                .pointer("/params/response/status")
                .and_then(Value::as_f64)
                .and_then(|status| u16::try_from(status as u64).ok()),
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.loadingFailed" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "failed".to_string(),
            request_id: value
                .pointer("/params/requestId")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: None,
            method: None,
            resource_type: value
                .pointer("/params/type")
                .and_then(Value::as_str)
                .map(str::to_string),
            mime_type: None,
            headers: BTreeMap::new(),
            status: None,
            error_text: value
                .pointer("/params/errorText")
                .and_then(Value::as_str)
                .map(str::to_string),
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.loadingFinished" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "finished".to_string(),
            request_id: value
                .pointer("/params/requestId")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: None,
            method: None,
            resource_type: None,
            mime_type: None,
            headers: BTreeMap::new(),
            status: None,
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        _ => None,
    }
}

fn diagnostic_headers(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| value.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_str<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a str, BrowserToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{name}`")))
}

fn json_f64(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<f64, BrowserToolError> {
    arguments
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{name}`")))
}

fn json_i64(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<i64, BrowserToolError> {
    arguments
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{name}`")))
}

fn runtime_console_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    let params = value.get("params")?;
    let level = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("log")
        .to_string();
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(remote_object_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(CdpDiagnosticEvent::Console {
        level,
        text,
        timestamp_ms: wall_timestamp_ms(params.get("timestamp")),
    })
}

fn log_entry_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    let entry = value.pointer("/params/entry")?;
    Some(CdpDiagnosticEvent::Console {
        level: entry
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("log")
            .to_string(),
        text: entry
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp_ms: wall_timestamp_ms(entry.get("timestamp")),
    })
}

fn remote_object_text(value: &Value) -> String {
    value
        .get("value")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .or_else(|| {
            value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn monotonic_timestamp_ms(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).map(|seconds| {
        if seconds > 10_000_000.0 {
            seconds as u64
        } else {
            (seconds * 1000.0) as u64
        }
    })
}

fn wall_timestamp_ms(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).map(|timestamp| {
        if timestamp > 1_000_000_000_000.0 {
            timestamp as u64
        } else {
            (timestamp * 1000.0) as u64
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use visible_browser_lab_test_support::{BrowserMode, RealBrowser};

    #[test]
    fn builds_cdp_discovery_urls() {
        let endpoint = CdpEndpoint::parse("http://127.0.0.1:9222").unwrap();
        assert_eq!(
            endpoint.version_url().as_str(),
            "http://127.0.0.1:9222/json/version"
        );
        assert_eq!(
            endpoint.targets_url().as_str(),
            "http://127.0.0.1:9222/json/list"
        );
    }

    #[test]
    fn broker_created_tabs_own_their_windows() {
        let params = create_page_params(Some("https://example.com"), true, 0).unwrap();
        assert_eq!(params.new_window, Some(true));
        assert_eq!(params.background, Some(false));
        assert_eq!(params.left, Some(WINDOW_CASCADE_ORIGIN.0));
        assert_eq!(params.top, Some(WINDOW_CASCADE_ORIGIN.1));
        assert_eq!(params.width, Some(WINDOW_DEFAULT_WIDTH));
        assert_eq!(params.height, Some(WINDOW_DEFAULT_HEIGHT));
    }

    #[test]
    fn only_dismissed_beforeunload_cancels_navigation_completion() {
        assert!(CdpClient::dismissed_dialog_cancels_navigation(
            &DialogType::Beforeunload
        ));
        for dialog_type in [DialogType::Alert, DialogType::Confirm, DialogType::Prompt] {
            assert!(
                !CdpClient::dismissed_dialog_cancels_navigation(&dialog_type),
                "ordinary dialogs must continue awaiting navigation completion"
            );
        }
    }

    #[test]
    fn timed_out_navigation_requires_the_expected_document_loader() {
        assert!(committed_navigation_reached_wait_state(
            "new-loader",
            "new-loader",
            "complete",
            "load"
        ));
        assert!(!committed_navigation_reached_wait_state(
            "new-loader",
            "old-loader",
            "complete",
            "load"
        ));
        assert!(!committed_navigation_reached_wait_state(
            "new-loader",
            "new-loader",
            "loading",
            "DOMContentLoaded"
        ));
    }

    #[test]
    fn navigation_watchdog_leaves_time_for_lifecycle_recovery() {
        assert_eq!(
            navigation_timeout_budget(10_000, true),
            Duration::from_secs(10) + NAVIGATION_RECOVERY_GRACE
        );
        assert!(navigation_timeout_budget(10_000, true) > Duration::from_secs(10));
        assert_eq!(
            navigation_timeout_budget(10_000, false),
            Duration::from_secs(10),
            "no-wait navigation has no lifecycle wait that needs recovery grace"
        );
    }

    #[test]
    fn windows_cascade_by_slot_and_wrap() {
        let third = create_page_params(None, false, 3).unwrap();
        assert_eq!(
            third.left,
            Some(WINDOW_CASCADE_ORIGIN.0 + 3 * WINDOW_CASCADE_STEP)
        );
        assert_eq!(third.background, Some(true));
        assert_eq!(third.url, "about:blank");

        // Slots advance by one per call and cycle: a full lap of the
        // cascade returns to the same slot. Relative assertions because
        // other tests share the process-wide counter.
        let first = next_window_slot();
        let second = next_window_slot();
        assert_eq!(second, (first + 1) % WINDOW_CASCADE_SLOTS);
        for _ in 0..(WINDOW_CASCADE_SLOTS - 1) {
            next_window_slot();
        }
        assert_eq!(next_window_slot(), second);
    }

    #[test]
    fn cdp_target_converts_to_tab_snapshot() {
        let target = CdpTarget {
            id: "target-1".to_string(),
            target_type: "page".to_string(),
            title: "Example".to_string(),
            url: "https://example.com".to_string(),
        };
        let snapshot = TabSnapshot::from(&target);
        assert_eq!(snapshot.target_id, "target-1");
        assert_eq!(snapshot.title, "Example");
        assert_eq!(snapshot.url, "https://example.com");
        assert!(!snapshot.focused);
    }

    #[test]
    fn maps_printable_and_named_keys() {
        let printable = key_event_for("a", &["Shift".to_string()]).unwrap();
        assert_eq!(printable.key, "a");
        assert_eq!(printable.code, "KeyA");
        assert_eq!(printable.text, Some("a".to_string()));
        assert_eq!(printable.modifiers, 8);

        let enter = key_event_for("Enter", &["Control".to_string()]).unwrap();
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.code, "Enter");
        assert_eq!(enter.text, None);
        assert_eq!(enter.modifiers, 2);
        assert_eq!(enter.windows_virtual_key_code, Some(13));
    }

    #[test]
    fn computes_content_quad_center_in_top_level_viewport() {
        let point = content_quad_point(&[10.0, 20.0, 30.0, 20.0, 30.0, 40.0, 10.0, 40.0]).unwrap();

        assert_eq!(point, ClickPoint { x: 20.0, y: 30.0 });
    }

    #[test]
    fn parses_runtime_console_event() {
        let event = diagnostic_event(&json!({
            "method": "Runtime.consoleAPICalled",
            "params": {
                "type": "log",
                "timestamp": 1234.0,
                "args": [
                    { "type": "string", "value": "hello" },
                    { "type": "number", "value": 42 }
                ]
            }
        }))
        .unwrap();
        assert_eq!(
            event,
            CdpDiagnosticEvent::Console {
                level: "log".to_string(),
                text: "hello 42".to_string(),
                timestamp_ms: Some(1_234_000)
            }
        );
    }

    #[test]
    fn parses_log_entry_timestamp_as_milliseconds() {
        let event = diagnostic_event(&json!({
            "method": "Log.entryAdded",
            "params": { "entry": { "level": "warning", "text": "careful", "timestamp": 1234.5 } }
        }))
        .unwrap();
        assert_eq!(
            event,
            CdpDiagnosticEvent::Console {
                level: "warning".to_string(),
                text: "careful".to_string(),
                timestamp_ms: Some(1_234_500)
            }
        );
    }

    #[test]
    fn parses_network_response_event() {
        let event = diagnostic_event(&json!({
            "method": "Network.responseReceived",
            "params": {
                "timestamp": 12.5,
                "response": { "url": "https://example.com/data.json", "status": 201 }
            }
        }))
        .unwrap();
        assert_eq!(
            event,
            CdpDiagnosticEvent::Network(NetworkEvent {
                sequence: 0,
                kind: "response".to_string(),
                request_id: None,
                url: Some("https://example.com/data.json".to_string()),
                method: None,
                resource_type: None,
                mime_type: None,
                headers: BTreeMap::new(),
                status: Some(201),
                error_text: None,
                timestamp_ms: Some(12_500)
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnects_after_handler_shutdown_and_reports_browser_disappearance() {
        let mut chrome = tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
            .await
            .unwrap()
            .unwrap();
        let client = CdpClient::new(chrome.cdp_endpoint()).unwrap();

        client.page_targets().await.unwrap();
        client.disconnect_for_test().await;
        client.page_targets().await.unwrap();

        chrome.shutdown();
        let error = timeout(Duration::from_secs(5), client.page_targets())
            .await
            .expect("Chrome disappearance should be observed without a request timeout")
            .unwrap_err();
        assert_eq!(
            error.code,
            crate::leases::BrowserToolErrorCode::ChromeUnavailable
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registers_targets_that_predate_the_runtime_connection() {
        let mut chrome = tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
            .await
            .unwrap()
            .unwrap();
        let target = reqwest::Client::new()
            .put(format!("{}/json/new?about:blank", chrome.cdp_endpoint()))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        let target_id = target["id"].as_str().unwrap();
        let client = CdpClient::new(chrome.cdp_endpoint()).unwrap();

        let target = client
            .page_targets()
            .await
            .unwrap()
            .into_iter()
            .find(|target| target.id == target_id)
            .expect("Chromiumoxide should register the pre-existing target");
        let result = client
            .evaluate(&target, "document.location.href")
            .await
            .unwrap();

        assert!(result.value.is_some());
        chrome.shutdown();
    }
}
