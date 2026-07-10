<!-- exo:11 ulid:01kx14nwe5wncfqqak1z8ck90z -->

# RFC 00011: Conversation-Scoped Session Identity

# Summary

RFC 00010 established that session handles belong below the agent's context window, in infrastructure that can observe a stable lifetime. This RFC defines that infrastructure for Visible Browser Lab.

A host adapter supplies an ambient `ConversationIdentity` for each tool call. Codex already sends its persisted thread identifier in MCP request metadata. VS Code already associates language-model tool calls with a persisted chat-session resource inside its host-created invocation token. The VBL adapters normalize those host facts into one versioned identity and forward it to the broker outside model-visible tool arguments.

The broker maps the identity to its internal browser session on first use. Calls in that conversation reuse the same session without `start_session` or `agent_session_id`; different conversations remain isolated. An explicitly supplied `agent_session_id` still takes precedence, and clients without ambient identity retain the explicit protocol. RFC 00009's sixty-minute TTL remains the cleanup governor for every session in this release. When that TTL expires an ambient session, VBL closes only targets it created for that session and still owns; claimed human targets and every explicit-session target remain open and become claimable.

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

# Guide-level explanation

In Codex or a supported VS Code chat, an agent calls `new_tab`, `list_tabs`, `snapshot`, and the other browser tools directly. It does not call `start_session` first and does not copy a session handle between calls.

On the first stateful browser call, the broker creates a browser session for the host conversation. Later calls carrying the same identity resolve to that session. Two Codex threads or two VS Code chats receive distinct identities and therefore distinct lease sets, even when they share the same Chrome profile and broker.

The identity is infrastructure metadata. It never appears in the tool's input schema, the model-visible arguments, normal browser-operation results, help examples, or logs. The internal `agent_session_id` remains available to the broker and to explicit clients. A caller that deliberately invokes `start_session` still receives the handle for backward compatibility, but ordinary ambient calls do not disclose it.

If the host does not provide a usable identity, VBL behaves like the explicit protocol. A stateful call without `agent_session_id` returns `session_required`; the agent calls `start_session` and passes the returned handle thereafter. Global VS Code tool invocations, older host versions, and bare MCP clients therefore degrade to an existing supported mode rather than failing unpredictably.

A persisted host identity does not make broker state persistent. Reopening a chat or resuming a Codex thread reuses the session while the broker and its TTL binding remain live. If the broker restarts or the session expires, the next ambient call creates a new session. Claimed human tabs remain open and claimable. Windows VBL created for the expired ambient conversation are closed if the conversation still owns them, preventing abandoned conversations from accumulating windows indefinitely.

# Reference-level explanation

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

`version` is the schema version. Version 1 is the only accepted value in this RFC. `issuer` is a lowercase reverse-DNS name with at least two labels. Each label contains ASCII lowercase letters, digits, or internal hyphens and starts and ends with a letter or digit. `id` is a non-empty opaque string; consumers do not require a UUID or URI shape.

The complete `(version, issuer, id)` tuple is the identity. IDs from different issuers never alias. Raw identity values are correlation data and must not appear in model-facing responses, previews, help, events, telemetry, or ordinary logs.

mcp-twill defines this schema and the corresponding public framework type. VBL mirrors the wire-compatible type until it is ported to Twill.

## Host Observations

### Codex

Codex supplies top-level MCP request metadata `_meta.threadId`. Because that compatibility key is not namespaced or authenticated, VBL honors it only when its deployment explicitly enables trusted Codex compatibility mode. The packaged Codex integration enables that mode; generic MCP deployments leave it disabled and require the canonical namespaced value. When enabled, the VBL MCP adapter normalizes `threadId` to:

```json
{
  "version": 1,
  "issuer": "com.openai.codex",
  "id": "<threadId>"
}
```

A future Codex version may send the canonical namespaced value directly. The canonical value is accepted independently of compatibility mode. When trusted Codex compatibility is enabled and both observations are present, they must normalize to the same identity. A mismatch is an invalid request and fails before broker dispatch. The compatibility mode is deployment configuration, never request metadata.

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
2. Codex `_meta.threadId`, normalized to the canonical type only when trusted Codex compatibility is enabled by deployment configuration.
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
- rmcp-adapter normalization for canonical metadata, plus an explicit trusted-host policy that enables Codex `threadId` compatibility only for known Codex deployments.
- An explicit host/test injection path for direct registry execution.

The declaration means that a command can consume ambient identity when the host provides it; it is not a hard requirement. Catalog and help projections expose the capability without exposing a value. The raw identity travels in a private, non-serializing invocation context. A digest participates in the invocation fingerprint for declaring commands so a permission approval cannot replay across conversations, while the raw identity remains absent from the plan and response.

Malformed canonical values, unsupported versions, and conflicting trusted observations fail before handler dispatch with framework diagnostics. Direct registry calls are identity-free unless their host explicitly supplies an invocation context.

A future Twill-based VBL uses only the declaration and context accessor. It does not reproduce metadata parsing or source precedence.

## VBL Adapter and Broker Protocol

`surface call` keeps identity out of process arguments. The VS Code extension invokes it with the non-sensitive flag `--request-envelope-version 1` and writes a versioned private envelope to stdin containing separate `arguments` and `context` members. The context carries conversation identity and workspace root; `arguments` remains the exact model-supplied object. Existing callers that omit the envelope flag continue to send the legacy raw argument object on stdin.

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

The broker maintains an in-memory map from `ConversationIdentity` to `agent_session_id`. Browser sessions record whether they were created explicitly or ambiently. Each active lease also retains private acquisition provenance: a lease created together with a new target for an ambient session is eligible for expiry closure; a lease obtained by claiming an existing target is not.

Before dispatching a stateful operation, the broker resolves the session:

1. If `params.agent_session_id` is present, validate and use that explicit session.
2. Otherwise, if request context contains conversation identity, look up or create its ambient session and inject the internal session id before typed parameter decoding.
3. Otherwise return `session_required` with `StartSession` recovery.

`help` remains stateless and does not create a session.

An ambient `start_session` call resolves or creates the ambient session rather than creating a second session. Its result reports `mode: "ambient"` and retains `agent_session_id` for backward compatibility with the documented explicit workflow. An explicit `start_session` reports `mode: "explicit"` and the same handle shape as today. An optional `start_url` still creates and leases a tab in the selected session. New ambient guidance does not require the agent to call `start_session` or carry the returned handle.

When the TTL sweep expires a session, it removes the identity binding as part of the same critical section that removes the session and reclaims its leases. The next call for that host identity can therefore mint a new ambient session. Claimed targets and every explicit-session target follow RFC 00009's release-not-close behavior. A target VBL created for an ambient session is closed only when its lease is still active at expiry; explicitly releasing it first removes the closure disposition. Targets selected for async closure remain reserved until the browser reports them closed or the close fails, so a concurrent caller cannot claim a target the sweep is about to close. A failed close releases the reservation and leaves the surviving target claimable. In-flight expiry protection is unchanged.

## Workspace Binding

Workspace identity is not part of `ConversationIdentity`. Codex thread ids and VS Code session-resource URIs are globally unique within their issuers, and combining the workspace with them would split one conversation when a multi-root editor changes focus.

The first non-empty workspace root supplied when an ambient session is created is canonicalized and bound to that browser session. Later workspace observations do not retarget the session and do not block ordinary browser operations such as navigation, snapshot, or interaction. Before a workspace-sensitive operation reads or writes local files—artifact export, file upload, or a file-backed drop—the broker compares the current non-empty observation with the bound root. An equal root is accepted; a conflicting root fails that workspace-sensitive operation with a workspace-context diagnostic. An absent later root does not erase the binding.

For VS Code, the invocation token's working directory takes precedence over the existing active-editor/workspace-folder heuristic. Codex continues to obtain its workspace observation from sandbox metadata.

## Tool Contract

Every operation that currently declares `agent_session_id` keeps the property but removes it from the schema's required list. Typed broker parameter structs may continue to require an internal session id because resolution injects it before decoding.

The new `session_required` error explains that no ambient identity or explicit session was available and points to `start_session`. Existing explicit clients remain source- and wire-compatible.

Server instructions and generated VS Code descriptions teach one workflow:

1. Call the desired browser operation directly.
2. When the host supplies ambient identity, do not request or retain a handle.
3. If the server returns `session_required`, call `start_session` and use the explicit handle.

## Lifecycle

The sixty-minute session TTL from RFC 00009 is the sole teardown governor in v0.4.6. Every resolved ambient request touches the internal session through the same dispatch-time path as explicit calls. On expiry, governance and acquisition provenance determine target disposition:

- an active target VBL created for an ambient session is closed;
- a target the ambient session claimed is released and remains open;
- every explicit-session target is released and remains open; and
- a target explicitly released before expiry remains open.

This asymmetry is the ownership boundary. Ambient browser windows exist on behalf of one host conversation and otherwise accumulate without a natural owner. Claimed targets and explicit sessions may represent durable human work, so a conversation deadline is not authority to close them.

This RFC does not interpret process exit, MCP disconnect, VS Code provider changes, chat idleness, or extension shutdown as conversation end. Provider changes preserve the same VS Code session resource and require no demotion or adoption flow.

Deterministic host lifecycle signals may later end ambient sessions sooner. They must be additive evidence with a clear authority contract; they do not change identity resolution.

# Drawbacks

The VS Code token bridge reads a field from an opaque public token. The implementation is host-owned and the proposed API exposes the same value directly, but the field is not yet a stable extension contract. Shape-checking and explicit fallback limit the failure to loss of ambient convenience.

Making `agent_session_id` optional in model-facing schemas moves one validation rule from JSON Schema to broker resolution. Calls without either authority fail at runtime with a precise recovery rather than at schema validation.

Conversation identity survives model context changes, but browser-session state remains in memory. Broker restart and TTL expiry still require recovery. Claimed targets remain available through the global inventory; an ambient-created target that expired with its conversation must be recreated.

An ambient-created window handed to a human through `focus_tab` still belongs to the ambient session and may close after sixty minutes without another request. The bounded cleanup is intentional, but integrations should claim existing human tabs when durability matters or explicitly release a VBL-created tab before a long handoff.

VBL temporarily mirrors a type and normalization rule that Twill ultimately owns. The duplication is bounded to the native-rmcp transition and must be removed when VBL ports.

# Rationale and alternatives

**Gateway argument stamping.** Rejected. It changes model-visible tool inputs, conflicts with strict schemas, requires transcript stripping, applies only to gateway-served models, and creates provider-switch demotion machinery. Host invocation context is broader and does not alter arguments.

**Codex facade process identity.** Rejected. Codex already supplies the persisted thread identity on each call. Processes can restart or outlive threads and are not the conversation authority.

**Extension-host identity.** Rejected. Concurrent VS Code chats share an extension host and must not share browser leases.

**Workspace-composite identity.** Rejected. The host identifiers are already globally scoped by issuer. Workspace remains session context so multi-root focus changes do not fork a conversation.

**Release every target on ambient expiry.** Rejected after dogfood. VBL creates one Chrome window per broker-created target to preserve capture reliability. Releasing those targets without closing them makes abandoned conversations accumulate visible windows and shifts cleanup to the user.

**Close every target on ambient expiry.** Rejected. A target claimed from the global inventory may be human-created durable state. Acquisition provenance gives VBL authority over the windows it created without extending that authority to tabs it merely borrowed.

**Waiting for stable VS Code APIs.** Rejected as the only path. The guarded bridge provides current value and degrades to the explicit protocol. The canonical boundary is designed so stabilization changes only the adapter source.

**Persisting broker session state now.** Deferred. Identity correlation and durable broker state are separate capabilities. RFC 00009 already defines a safe recovery floor.

# Prior Art

Distributed tracing separates transport-specific extraction from a canonical context propagated through application infrastructure. The issuer and opaque id play the same correlation role here, while VBL's session remains application-owned state.

Language servers and IDE chat systems similarly attach workspace and session context outside command arguments. DHCP-style lease expiry remains the model for reclaiming state when infrastructure cannot observe a definitive end.

# Acceptance Tests

- Canonical metadata parses to the exact version/issuer/id tuple; invalid versions, unknown fields, malformed lowercase reverse-DNS issuers, and empty ids fail before dispatch.
- Codex `threadId` is ignored when trusted Codex compatibility is disabled and normalizes to issuer `com.openai.codex` only when the deployment enables it.
- Matching canonical and Codex observations succeed; conflicting or malformed observations fail before dispatch.
- Raw identities do not serialize through plans, responses, help, previews, events, or logs.
- Repeated ambient calls reuse one session; two identities receive disjoint sessions and lease sets.
- An explicit `agent_session_id` takes precedence over ambient identity.
- A stateful call with neither source returns `session_required`; the explicit workflow remains functional.
- Ambient and explicit `start_session` both retain the legacy handle shape; ordinary ambient browser operations expose no handle.
- Expiry removes the identity binding, closes active targets VBL created for the ambient session, and releases claimed targets without closing them.
- Explicit-session expiry remains release-only, and explicitly releasing an ambient-created target before expiry keeps it open.
- A target reserved for ambient-expiry closure cannot be claimed concurrently; a failed close removes the reservation and leaves the target claimable.
- Workspace changes do not block ordinary browser operations; equal observations are accepted for workspace-sensitive operations, and conflicting observations fail only those operations without changing the binding.
- Codex installed-artifact testing proves direct operation, thread isolation, and resume/compaction continuity while the TTL binding remains live.
- VS Code 1.128 installed-artifact testing proves direct operation, simultaneous-chat isolation, history restoration, provider-independent continuity, and explicit fallback for a global invocation.
- Tool-call arguments and normal non-`start_session` results in both hosts contain no ambient session handle.

# Unresolved Questions

No unresolved question blocks v0.4.6. Dogfood must measure how often the VS Code token bridge is unavailable on supported stable versions and whether workspace observations remain stable across real multi-root conversations.

# Future Possibilities

- Replace token shape detection with stable `chatSessionResource` when VS Code publishes it.
- Port VBL to Twill and delete the local identity schema and normalization.
- Add authoritative host lifecycle events for earlier ambient-session teardown.
- Persist selected broker session state across broker replacement or machine restart.
- Propose a standard MCP conversation-identity metadata field if multi-implementation evidence supports it.
