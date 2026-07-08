<!-- exo:11 ulid:01kx14nwe5wncfqqak1z8ck90z -->

# RFC 00011: Conversation-Scoped Session Identity

# Summary

RFC 00010 established that session handles belong below the agent's context window, in infrastructure that can observe a stable lifetime. This RFC defines that infrastructure for Visible Browser Lab.

A host adapter supplies an ambient `ConversationIdentity` for each tool call. Codex already sends its persisted thread identifier in MCP request metadata. VS Code already associates language-model tool calls with a persisted chat-session resource inside its host-created invocation token. The VBL adapters normalize those host facts into one versioned identity and forward it to the broker outside model-visible tool arguments.

The broker maps the identity to its internal browser session on first use. Calls in that conversation reuse the same session without `start_session` or `agent_session_id`; different conversations remain isolated. An explicitly supplied `agent_session_id` still takes precedence, and clients without ambient identity retain the explicit protocol. RFC 00009's sixty-minute TTL remains the cleanup governor for every session in this release.

mcp-twill owns the reusable conversation-identity contract: metadata schema, authority, validation, framework declaration, handler context, fingerprint binding, and non-disclosure rules. VBL mirrors that contract while it remains a native-rmcp server and owns only the browser-specific mapping from conversation identity to sessions and leases.

# Motivation

## Handles are not conversation identity

The explicit VBL protocol is sound for clients that provide no conversation context: call `start_session`, retain the returned handle, and pass it to every operation. It is not the right default for an agent conversation. Compaction, handoff, and long tool sequences make the model's context an unreliable place to store an infrastructure capability. Forgetting the handle either strands browser state or encourages the agent to invent a replacement.

The host already maintains the relationship the model cannot. A Codex thread persists independently of any individual turn and Codex attaches that thread identifier to MCP tool calls. A VS Code chat persists a session resource across turns and history restoration, and the chat participant passes a host-created token through tool invocation so VS Code can associate the call with the correct conversation. Those are authoritative host facts, not model claims.

Using those observations makes session continuity a property of the integration. The model calls browser operations; the host identifies the conversation; the broker resolves the session.

## Evidence that revised the original design

The original Stage-0 design treated Codex's MCP process lifetime as the conversation lifetime and treated vscode-ai-gateway as the only VS Code component capable of correlation. Recon against the shipped hosts changed both premises.

Codex 0.136 inserts its persisted thread identifier into every MCP tool call as top-level `_meta.threadId`, overwriting any stale caller value. A facade-generated process identifier would be less stable and would mistake leaked or restarted processes for conversation boundaries.

VS Code 1.128 constructs each chat request with a `toolInvocationToken` whose internal invocation context carries the persisted `sessionResource` and working directory. The token's public contract is intentionally opaque, so an extension must shape-check it and degrade safely, but the proposed API already exposes the same resource directly as `chatSessionResource`. The source is independent of the selected language-model provider.

The gateway alternative also collides with strict tool schemas. VS Code validates tool inputs against their declared schemas before invoking an extension; an extra stamped argument is rejected by tools with `additionalProperties: false` unless every participating schema admits it. It would also cover only conversations served by that provider. Host-owned invocation context covers the conversation at the tool boundary without altering arguments, transcripts, or provider behavior.

# Guide-Level Explanation

In Codex or a supported VS Code chat, an agent calls `new_tab`, `list_tabs`, `snapshot`, and the other browser tools directly. It does not call `start_session` first and does not copy a session handle between calls.

On the first stateful browser call, the broker creates a browser session for the host conversation. Later calls carrying the same identity resolve to that session. Two Codex threads or two VS Code chats receive distinct identities and therefore distinct lease sets, even when they share the same Chrome profile and broker.

The identity is infrastructure metadata. It never appears in the tool's input schema, the model-visible arguments, normal tool results, help examples, or logs. The internal `agent_session_id` remains available to the broker and to explicit clients but is not disclosed during ambient operation.

If the host does not provide a usable identity, VBL behaves like the explicit protocol. A stateful call without `agent_session_id` returns `session_required`; the agent calls `start_session` and passes the returned handle thereafter. Global VS Code tool invocations, older host versions, and bare MCP clients therefore degrade to an existing supported mode rather than failing unpredictably.

A persisted host identity does not make broker state persistent. Reopening a chat or resuming a Codex thread reuses the session while the broker and its TTL binding remain live. If the broker restarts or the session expires, the next ambient call creates a new session. RFC 00009 leaves the old tabs open and claimable, so the failure is recoverable without destroying browser state.

# Reference-Level Explanation

## Canonical Conversation Identity

The canonical MCP metadata key is:

`io.github.wycats.mcp-twill/conversation-identity`

Its value is:

```json
{
  "version": 1,
  "issuer": "com.openai.codex",
  "id": "opaque-host-conversation-id"
}
```

`version` is the schema version. Version 1 is the only accepted value in this RFC. `issuer` is a non-empty stable reverse-DNS identifier for the host identity authority. `id` is a non-empty opaque string; consumers do not require a UUID or URI shape.

The complete `(version, issuer, id)` tuple is the identity. IDs from different issuers never alias. Raw identity values are correlation data and must not appear in model-facing responses, previews, help, events, telemetry, or ordinary logs.

mcp-twill defines this schema and the corresponding public framework type. VBL mirrors the wire-compatible type until it is ported to Twill.

## Host Observations

### Codex

Codex supplies top-level MCP request metadata `_meta.threadId`. The VBL MCP adapter normalizes it to:

```json
{
  "version": 1,
  "issuer": "com.openai.codex",
  "id": "<threadId>"
}
```

A future Codex version may send the canonical namespaced value directly. When both observations are present, they must normalize to the same identity. A mismatch is an invalid request and fails before broker dispatch.

The process identifier, MCP connection, and stdio lifetime are not identity sources or teardown authority.

### VS Code

The VBL language-model tool adapter shape-checks `options.toolInvocationToken` for a URI-like `sessionResource`. When present, it constructs:

```json
{
  "version": 1,
  "issuer": "com.microsoft.vscode",
  "id": "<sessionResource.toString(true)>"
}
```

The adapter also reads the token's URI-like `workingDirectory` when present. When the token shape is absent or changes, the adapter supplies no conversation identity and the explicit protocol remains available.

Once VS Code stabilizes `chatSessionResource` on `LanguageModelToolInvocationOptions`, the adapter reads that public field first and retains the token bridge only for supported older versions. This provenance change does not affect the canonical identity or broker behavior.

### Other MCP Clients

Other clients send the namespaced metadata value directly. Missing metadata is not an error. Malformed canonical metadata is an error because silently treating a claimed but invalid identity as absent could route a call into the wrong governance mode.

## Authority and Precedence

Identity resolution has two layers.

At the framework/transport layer:

1. Canonical namespaced metadata.
2. Codex `_meta.threadId`, normalized to the canonical type.
3. No ambient identity.

At the VBL application layer:

1. A non-empty explicit `agent_session_id` in the declared tool arguments.
2. The normalized ambient conversation identity.
3. No session authority.

An explicit session handle therefore continues to select exactly the session the caller named, even when ambient identity is also available. Ambient metadata never overwrites an explicit argument.

Model-visible arguments are not an ambient observation source. There is no reserved argument key and no pre-validation stripping rule.

## Twill Boundary

Twill's public surface is:

- `ConversationIdentity` and `CONVERSATION_IDENTITY_META_KEY`.
- `CommandBuilder::uses_conversation_identity()` and the corresponding optional command-spec declaration.
- `CommandContext::conversation_identity() -> Option<&ConversationIdentity>`.
- rmcp-adapter normalization for canonical metadata and Codex `threadId`.
- An explicit host/test injection path for direct registry execution.

The declaration means that a command can consume ambient identity when the host provides it; it is not a hard requirement. Catalog and help projections expose the capability without exposing a value. The raw identity travels in a private, non-serializing invocation context. A digest participates in the invocation fingerprint for declaring commands so a permission approval cannot replay across conversations, while the raw identity remains absent from the plan and response.

Malformed canonical values, unsupported versions, and conflicting trusted observations fail before handler dispatch with framework diagnostics. Direct registry calls are identity-free unless their host explicitly supplies an invocation context.

A future Twill-based VBL uses only the declaration and context accessor. It does not reproduce metadata parsing or source precedence.

## VBL Adapter and Broker Protocol

`surface call` accepts `--conversation-identity-json` alongside its existing workspace option. The VS Code extension passes the canonical JSON through this out-of-band option; tool input remains the exact model-supplied object.

Broker protocol version 4 adds an optional request context:

```json
{
  "conversation_identity": {
    "version": 1,
    "issuer": "com.microsoft.vscode",
    "id": "vscode-local-chat-session:..."
  },
  "workspace_root": "/path/to/project"
}
```

The context is a sibling of `params`, never part of operation parameters. Old callers may omit it. Existing broker compatibility checks replace a running broker whose protocol or package version does not match the invoking binary.

The broker maintains an in-memory map from `ConversationIdentity` to `agent_session_id`. Browser sessions record whether they were created explicitly or ambiently.

Before dispatching a stateful operation, the broker resolves the session:

1. If `params.agent_session_id` is present, validate and use that explicit session.
2. Otherwise, if request context contains conversation identity, look up or create its ambient session and inject the internal session id before typed parameter decoding.
3. Otherwise return `session_required` with `StartSession` recovery.

`help` remains stateless and does not create a session.

An ambient `start_session` call resolves or creates the ambient session rather than creating a second session. Its result reports `mode: "ambient"` and omits `agent_session_id`. An explicit `start_session` reports `mode: "explicit"` and the handle as today. An optional `start_url` still creates and leases a tab in the selected session.

When the TTL sweep expires a session, it removes the identity binding as part of the same critical section that removes the session and releases its leases. The next call for that host identity can therefore mint a new ambient session. Conversation identity does not weaken RFC 00009's release-not-close behavior or in-flight expiry protection.

## Workspace Binding

Workspace identity is not part of `ConversationIdentity`. Codex thread ids and VS Code session-resource URIs are globally unique within their issuers, and combining the workspace with them would split one conversation when a multi-root editor changes focus.

The first non-empty workspace root supplied when an ambient session is created is canonicalized and bound to that browser session. A later equal root is accepted. A later conflicting non-empty root fails with a workspace-context diagnostic rather than silently changing where artifact operations write. An absent later root does not erase the binding.

For VS Code, the invocation token's working directory takes precedence over the existing active-editor/workspace-folder heuristic. Codex continues to obtain its workspace observation from sandbox metadata.

## Tool Contract

Every operation that currently declares `agent_session_id` keeps the property but removes it from the schema's required list. Typed broker parameter structs may continue to require an internal session id because resolution injects it before decoding.

The new `session_required` error explains that no ambient identity or explicit session was available and points to `start_session`. Existing explicit clients remain source- and wire-compatible.

Server instructions and generated VS Code descriptions teach one workflow:

1. Call the desired browser operation directly.
2. When the host supplies ambient identity, do not request or retain a handle.
3. If the server returns `session_required`, call `start_session` and use the explicit handle.

## Lifecycle

The sixty-minute session TTL from RFC 00009 is the sole teardown governor in v0.4.6. Every resolved ambient request touches the internal session through the same dispatch-time path as explicit calls.

This RFC does not interpret process exit, MCP disconnect, VS Code provider changes, chat idleness, or extension shutdown as conversation end. Provider changes preserve the same VS Code session resource and require no demotion or adoption flow.

Deterministic host lifecycle signals may later end ambient sessions sooner. They must be additive evidence with a clear authority contract; they do not change identity resolution.

# Drawbacks

The VS Code token bridge reads a field from an opaque public token. The implementation is host-owned and the proposed API exposes the same value directly, but the field is not yet a stable extension contract. Shape-checking and explicit fallback limit the failure to loss of ambient convenience.

Making `agent_session_id` optional in model-facing schemas moves one validation rule from JSON Schema to broker resolution. Calls without either authority fail at runtime with a precise recovery rather than at schema validation.

Conversation identity survives model context changes, but browser-session state remains in memory. Broker restart and TTL expiry still require recovery through the claimable global inventory.

VBL temporarily mirrors a type and normalization rule that Twill ultimately owns. The duplication is bounded to the native-rmcp transition and must be removed when VBL ports.

# Rationale And Alternatives

**Gateway argument stamping.** Rejected. It changes model-visible tool inputs, conflicts with strict schemas, requires transcript stripping, applies only to gateway-served models, and creates provider-switch demotion machinery. Host invocation context is broader and does not alter arguments.

**Codex facade process identity.** Rejected. Codex already supplies the persisted thread identity on each call. Processes can restart or outlive threads and are not the conversation authority.

**Extension-host identity.** Rejected. Concurrent VS Code chats share an extension host and must not share browser leases.

**Workspace-composite identity.** Rejected. The host identifiers are already globally scoped by issuer. Workspace remains session context so multi-root focus changes do not fork a conversation.

**Waiting for stable VS Code APIs.** Rejected as the only path. The guarded bridge provides current value and degrades to the explicit protocol. The canonical boundary is designed so stabilization changes only the adapter source.

**Persisting broker session state now.** Deferred. Identity correlation and durable broker state are separate capabilities. RFC 00009 already defines a safe recovery floor.

# Prior Art

Distributed tracing separates transport-specific extraction from a canonical context propagated through application infrastructure. The issuer and opaque id play the same correlation role here, while VBL's session remains application-owned state.

Language servers and IDE chat systems similarly attach workspace and session context outside command arguments. DHCP-style lease expiry remains the model for reclaiming state when infrastructure cannot observe a definitive end.

# Acceptance Tests

- Canonical metadata parses to the exact version/issuer/id tuple.
- Codex `threadId` normalizes to issuer `com.openai.codex`.
- Matching canonical and Codex observations succeed; conflicting or malformed observations fail before dispatch.
- Raw identities do not serialize through plans, responses, help, previews, events, or logs.
- Repeated ambient calls reuse one session; two identities receive disjoint sessions and lease sets.
- An explicit `agent_session_id` takes precedence over ambient identity.
- A stateful call with neither source returns `session_required`; the explicit workflow remains functional.
- Ambient `start_session` omits the handle and explicit `start_session` retains it.
- Expiry removes the identity binding and releases rather than closes tabs.
- Equal workspace observations are accepted and conflicting observations fail without changing the binding.
- Codex installed-artifact testing proves direct operation, thread isolation, and resume/compaction continuity while the TTL binding remains live.
- VS Code 1.128 installed-artifact testing proves direct operation, simultaneous-chat isolation, history restoration, provider-independent continuity, and explicit fallback for a global invocation.
- Tool-call arguments and normal results in both hosts contain no ambient session handle.

# Unresolved Questions

No unresolved question blocks v0.4.6. Dogfood must measure how often the VS Code token bridge is unavailable on supported stable versions and whether workspace observations remain stable across real multi-root conversations.

# Future Possibilities

- Replace token shape detection with stable `chatSessionResource` when VS Code publishes it.
- Port VBL to Twill and delete the local identity schema and normalization.
- Add authoritative host lifecycle events for earlier ambient-session teardown.
- Persist selected broker session state across broker replacement or machine restart.
- Propose a standard MCP conversation-identity metadata field if multi-implementation evidence supports it.
