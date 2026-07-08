<!-- exo:11 ulid:01kx14nwe5wncfqqak1z8ck90z -->

# RFC 11: Conversation-Scoped Session Identity

# Summary

RFC 00010 established that session handles must not live in the agent's context window and directed this design toward the shims that observe conversation lifetimes. This RFC specifies that design. Session identity becomes an ambient fact injected by infrastructure, in three tiers matching what each deployment can actually observe. The codex tier binds identity to the facade process, whose lifetime is the conversation. The VS Code tier derives identity in vscode-ai-gateway, a companion extension that serves chat models to VS Code and therefore sees every conversation that uses them, and stamps it into tool-call arguments at the one seam the gateway already controls. Bare MCP clients keep the explicit `start_session` protocol with the RFC 00009 TTL as their safety net. In the first two tiers the model never sees, stores, or forgets a session handle, and a conversation's sessions end when the conversation ends.

# Motivation

## The handle problem, one level deeper

RFC 00010 named the rule: handles live in the lowest layer with an observable lifetime, and the agent's context window is not such a layer. What it left open was which layer is. The obvious candidate, the tool-hosting extension, turns out to see almost nothing: VS Code hands a tool invocation an opaque `toolInvocationToken` with no stability contract, no stable API exposes a chat session identifier to tools, and no event announces a conversation's end. An extension-host-scoped session was considered and rejected during this design: two concurrent chats in one window would share one session, and a session that outlives its conversation by hours is not a lifetime, it is a leak with better manners.

The party that does see conversations is the language model provider, and understanding why requires knowing how VS Code structures that role. Chat model providers in VS Code are stateless: on every turn, VS Code hands the provider the complete message history and asks for the next response. There is no incremental protocol and no session object; the conversation exists, in full, in every request.

vscode-ai-gateway is such a provider: a companion extension (same author, separate codebase) that serves chat models to VS Code by routing requests to OpenResponses backends. Everything this RFC relies on, the gateway already does for its own purposes, not for this design. Because its backends benefit from server-side state reuse, it must recognize "this request continues that conversation" across stateless calls, and it does so with a mechanism worth describing because this RFC inherits its properties: the gateway embeds a stateful marker, an opaque data part VS Code persists in chat history, into every completed assistant turn, and on the next request scans the history backward for the latest marker to recover the conversation's identity. The marker is stripped before anything reaches the model and is never rendered to the user. Because VS Code's tool-calling loop runs through the provider, the gateway also constructs the tool-call parts VS Code dispatches to tool extensions, and it rebuilds the model-bound request from VS Code's chat history on every turn. Correlation, construction, and re-ingestion are existing, load-bearing machinery; this design adds one field at each point.

Identity recovered this way has a property nothing stored in the context window can have: it does not live in the model's context at all. The marker rides in VS Code's persisted chat history, so compacting or truncating what the model sees cannot touch it, and every assistant turn re-plants it, so identity survives as long as any marker-bearing message survives. The failure mode is correspondingly precise: a history rewrite that removes every marker-bearing assistant message (editing the first message, a host-side summarization that replaces all assistant turns) silently mints a fresh conversation identity. For sessions this is a fork, not a leak: the old session ages out through teardown or TTL while the new conversation gets a clean one.

The marker is also precedent for this design's central move. The gateway already smuggles infrastructure metadata through a channel the platform does not officially offer, stripping it before the model sees it, because the platform provides no first-class alternative. The argument stamp is the same maneuver aimed at the outbound hop, and the gateway's own design documents already frame the marker as a workaround awaiting a first-class identity interface, the same polyfill posture this RFC adopts below.

The corresponding limit is stated in Drawbacks: the gateway only sees conversations whose model it serves.

## Why every tool should not solve this

Without this RFC, each tool extension that wants conversation-scoped resources must reinvent conversation correlation, which means each one either builds a ConversationManager or peeks at undocumented token internals. That work is exactly the kind of boring, correctness-critical bookkeeping that RFC 00010 says belongs in infrastructure. The gateway derives identity once; every tool downstream consumes it ambiently.

# Guide-level explanation

An agent using browser tools in VS Code or codex simply never calls `start_session`. Tabs, leases, artifacts, and emulation overrides belong to a session the infrastructure created for the conversation, and when the conversation ends, the broker finds out and unwinds, deterministically. Two chats in one window are two conversations, two sessions, two disjoint lease sets. A compaction that erases the model's memory of everything erases nothing the broker depends on.

If the conversation moves to a model the infrastructure cannot see, the tools say so: the next result explains that ambient identity ended, offers the session's handle, and the agent continues explicitly. The infrastructure never silently strands work.

For bare MCP clients, nothing changes: explicit `start_session`, explicit handle, TTL sweep as the honest fallback for a client the infrastructure cannot observe.

# Reference-level explanation

## The identity channel: `_meta` where the wire is MCP, arguments where it is not

MCP already reserves a channel for exactly this kind of metadata: every request, including `tools/call`, carries an optional `params._meta` alongside `params.arguments`, designated for implementations to attach namespaced metadata outside the tool's input schema. The codex facade already reads `sandboxCwd` from `_meta` today. Wherever MCP is the wire, session identity travels in `_meta` under a reserved namespace: schema-invisible, absent from tool-call rendering, and spec-blessed.

The one hop where this is impossible is the hop the gateway controls. The gateway-to-tool path runs through VS Code's language model tools API, and `LanguageModelToolCallPart` has three fields: call id, name, arguments. There is no metadata channel, and even when the target tool is an MCP server registered with VS Code, VS Code's own MCP client constructs the `tools/call` request and offers chat providers no way to attach `_meta` to it. On that hop, and only that hop, identity rides inside the arguments as a stamp, and the consuming extension converts it back: strip the reserved key, forward identity out-of-band. The stamp is a VS Code-specific adapter for the `_meta` convention, not the convention itself.

Both halves of this arrangement are candidates for standardization, and the design should be read with that trajectory in mind. If the convention proves out, the reserved `_meta` namespace is the shape of a proposal to the MCP spec itself: conversation-scoped identity as first-class request metadata, the way trace context graduated from per-vendor headers to a standard. And the gateway mechanism, derivation plus stamp-and-strip, is a polyfill in the precise web-platform sense: a userland implementation of a feature the platform could provide natively, built so that when the native version arrives, the polyfill deletes and nothing layered on top notices.

| Hop | Wire | Identity channel |
|---|---|---|
| codex facade → broker | MCP/IPC | `_meta` (existing pattern) |
| gateway → VS Code → tool extension | LM tools API | argument stamp (no metadata channel exists) |
| tool extension → broker | `surface call` | injected parameter; stamp stripped |
| bare MCP client → server | MCP | `_meta` or explicit params |

## Tier 1: codex, process lifetime

The codex plugin runs one MCP facade process per conversation over stdio. The facade mints a session identity at startup, injects it into every broker call, and holds a persistent identity connection to the broker. Stdio close or process exit is the conversation-end signal, a real drop. RFC 00009 rejected connection lifetime because "the facade reconnects per request"; that described the implementation of the day, not a constraint. The facade dials per request today as a choice, and it can hold one long-lived identity connection precisely to give the broker something to observe.

## Tier 2: VS Code, gateway-derived identity

**Derivation.** The gateway's marker-recovered conversation identity already serves its own persistence and backend-state reuse. The session identity for a tool call is a function of that conversation identity plus a broker-scoping component (workspace or window), so distinct conversations never share a session.

**Injection: stamp and strip.** Both halves of the seam are single points in code the gateway already owns. Every live tool-call part is constructed at one site in the stream adapter; the stamp is added there. Every replayed tool call is serialized back toward the model through one function in message translation, which already transforms argument values in flight (special-token sanitization); the strip is added there, on arguments that come from VS Code's stored copy of the part and therefore carry the stamp. The wire request to the model carries clean arguments, and the model never observes the stamp in either direction. One sequencing constraint is load-bearing: in the gateway's stateful mode, earlier turns persist in the provider's server-side transcript, so the strip must ship in the same change as the stamp or stamped arguments get baked into server state that cannot be retro-stripped.

**Consumption.** The visible-browser-lab extension extracts the reserved key, passes the identity through the same seam that already injects `workspace_root` into `surface call`, and forwards clean arguments to the broker. The broker mints the session on first use, keyed by the injected identity; repeated calls with the same identity join the same session. Explicit `agent_session_id` parameters, when present, take precedence, which keeps every existing client working unchanged.

**Teardown.** The gateway already tracks conversation liveness for its own purposes: each turn transitions a conversation's persisted status from active to idle, the transition is already detected internally, and a last-activity timestamp is written on every turn. No teardown mechanism exists today, but the signals compose into one: an idle transition debounced by a quiet period declares the conversation abandoned, a staleness sweep over last-activity catches conversations a crash left permanently active, and extension shutdown ends everything. The extension relays the end signal to the broker. Because end detection in VS Code is heuristic where codex's is structural, the RFC 00009 TTL remains armed for this tier, scoped now to conversations the gateway lost rather than to every session ever minted.

**Mode transitions.** VS Code allows a conversation to change model providers mid-stream, which makes identity mode a per-call fact, not a per-conversation one. Sessions therefore carry a mode, ambient or explicit, and the transitions are part of the contract:

*Demotion.* A conversation running ambiently switches to a non-gateway model; the next tool call arrives unstamped while the extension holds a live ambient session full of tabs and leases. Minting a fresh session here would orphan everything the agent was working with, silently. Instead the tool result overtly describes the downgrade: ambient identity is no longer available, and here is what to do about it. When exactly one live ambient session exists, the response hands over its handle and instructs the agent to pass `agent_session_id` explicitly from then on. When several exist, the response presents the candidates with identifying detail (tab titles, URLs) and lets the agent adopt one: the collaborative adoption surface RFC 00010 anticipated, arriving at its first concrete need. This is the one moment a handle legitimately enters the context window, because the infrastructure that held it can no longer see the conversation.

*Adoption converts governance.* After demotion the gateway may still conclude the conversation ended, because from where it stands, it did. An explicitly adopted session must therefore detach from gateway end signals and re-arm the TTL as its governor. Demotion is not just an identity change; it transfers the session from ambient stewardship to the explicit regime, teardown authority included.

*Disclosure.* Each mode states its rules where the agent can see them. In ambient mode, the first tool result notes that session identity is managed by infrastructure and no id should be tracked, a mode statement, not a handle, so RFC 00010's rule is preserved. In explicit mode, `start_session` returns operating instructions: the handle, the obligation to pass it, the TTL. Promotion (a gateway model joins mid-conversation) is already safe because explicit parameters take precedence; the tool result may note that ambient identity has become available.

## Tier 3: bare MCP, unchanged

Explicit `start_session`, explicit handle threading, TTL sweep. This tier is the documented floor, not the design.

## Arbitration

Takeover and handoff currently arbitrate between distinct explicit session identities. This design preserves distinctness exactly, one session per conversation, so the lease model's arbitration carries over without modification. The multi-agent question RFC 00010 left open resolves the same way: two agents are two conversations, whether they share a window or not.

# Drawbacks

The stamp is visible between the gateway and the strip point, but only on the single VS Code hop. VS Code's chat UI can render tool-call inputs, so a user inspecting a call may see the reserved key there. It must be visibly namespaced and boring, infrastructure metadata a reader can dismiss, not something that looks like the agent's intent. Every MCP-visible surface carries identity in `_meta` instead, where nothing renders it.

The tool declaration on the VS Code hop must tolerate the reserved key. A tool declared with `additionalProperties: false` would reject stamped arguments, so the participating declaration admits the key explicitly, in one place. MCP servers consuming identity via `_meta` need no schema change at all.

This tier exists only when the gateway serves the conversation's model. A chat using another provider (the built-in models, a different BYOK extension) never passes through the gateway, and no stamp arrives. The consuming extension must treat the stamp as optional and degrade to tier 3 behavior, explicit sessions with the TTL backstop, so the design falls back to today's contract rather than breaking. But the ambient experience is only as universal as gateway adoption, and this RFC does not pretend otherwise.

The gateway becomes load-bearing infrastructure for other extensions. Identity export is a public contract with compatibility obligations, on top of a codebase that currently serves one consumer.

Gateway end signals are heuristic. A conversation the user silently abandons looks identical to one they will resume tomorrow. The composed signal (idle debounce, staleness sweep, shutdown) makes the common cases deterministic, but the TTL backstop is permanent for this tier, and honest documentation says so. Conversation identity itself has a rewrite edge: a history rewrite that strips every marker-bearing assistant message forks the identity, stranding the old session for teardown to collect.

# Rationale and alternatives

**Extension-host-scoped sessions.** Simplest implementation, no gateway involvement. Rejected: two chats sharing a session is a correctness failure, not a granularity trade.

**Peeking inside `toolInvocationToken`.** The token plausibly carries a session identifier internally, and shape-detection with graceful degradation could read it. Rejected as the primary mechanism: it couples every tool extension to undocumented internals, and this design's whole point is that tools should not own correlation. Remains available as a cross-check inside the gateway tier if stamp routing ever needs validation.

**Waiting for stable chat-session APIs.** The right shape may eventually arrive upstream, and the channel analysis names what to actually want: from VS Code, provider-attached metadata on tool calls, `_meta` passthrough in the LM tools API; from MCP, the identity convention blessed as spec-level metadata. Waiting is rejected, but building toward those outcomes is not: the polyfill is how the proposal earns its evidence. The stamp-and-strip seam is deliberately narrow so that either upstream landing is a provenance change, invisible to consumers.

**Stamping via a side channel instead of arguments.** An exported gateway API keyed by call id would avoid touching arguments, but the tool extension has no stable access to its own call id at invocation time, which is precisely the correlation gap this design routes around.

# Prior art

Distributed tracing solved this problem for RPC: trace context rides in designated metadata fields injected and stripped by infrastructure, invisible to application logic, surviving hops that lose all other state. The stamp is trace context for tool calls. On the teardown side, DHCP's lease-with-renewal remains the model for the tier where the infrastructure cannot see the client die, exactly as RFC 00009 filed it.

# Unresolved questions

Whether the gateway stamps all tool calls or maintains an opt-in registry of participating tools, and where that registry lives.

The exact reserved namespace and payload schema, which becomes a public contract between the gateway and every consumer. The natural split: mcp-twill defines the `_meta` convention as the canonical consumer-side capability, so ported servers inherit extraction and ambient identity from the framework, and the VS Code argument stamp is specified as an adapter that converts to it at the extension boundary. Whether twill also owns the schema document or merely implements it is the cross-repo governance question.

What the gateway's end-signal export concretely is (event subscription via extension exports, which the gateway already uses for auth, or a polled API) and the debounce and staleness thresholds: ending a session the user resumes is recoverable via adoption, but the cost must be measured before the signal is made aggressive.

The exact wording and shape of demotion notices and mode disclosures, which are agent-facing UX with the same care requirements as error messages.

Whether the broker's session-scoping component should include workspace identity so that two windows on the same workspace share arbitration but two workspaces never collide.
