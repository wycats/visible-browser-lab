<!-- exo:10 ulid:01kwypzsth8hyq8sx120ftd94c -->

# RFC 10: Stewardship by Construction

# Summary

Three incidents in one release cycle had the same anatomy: the broker handed out a capability whose safety depended on a paired action that never came. Remote object handles pinned fifteen gigabytes of detached DOM because nothing released them. Sessions locked tabs forever because nothing ended them. A screencast starved to zero frames because activating a sibling tab hid the page and nothing put its visibility back. Each got a fix, and the fixes fall into two families: machinery that watches for the bad state and compensates after the fact (sweeps, TTLs, tombstones), and structure that makes the bad state unrepresentable in the first place (per-operation object groups released on every path, guards that clean up on drop).

This RFC names the second family as the design rule: normal usage of normal tools produces correct resource behavior, by construction. The rule has two axes. First, every capability the broker hands out must be owned by a broker-side resource whose lifetime the broker can observe and whose teardown is deterministic, a drop, not a sweep. Second, handles must live in the lowest layer that has an observable lifetime, and the agent's context window is not such a layer. The RFC applies the rule to two live problems: screencast frame starvation, where the constructive fix is designed and evidence-backed, and session identity placement, where this RFC proposes a direction.

# Motivation

## The agent cannot be trusted to call free

This is not a criticism of agents; it is their operating reality. An agent's context window is its working memory, and that memory is unreliable in ways an ordinary program's memory is not. Compaction truncates it mid-task. Conversation end is a process exit that nothing signals. And bookkeeping obligations, the boring pairings of acquire with release, are exactly the material that agent attention deprioritizes, because they are never the point of the task. A protocol that requires the agent to remember to release what it acquired will leak, not at the rate of programmer error but at the rate of agent memory failure, which is orders of magnitude higher and not improvable by better prompting.

The broker's job is to be the reliable party in this pairing. Whatever the agent forgets, abandons, or loses in a compaction, the broker must be able to observe and unwind, deterministically, without a clock having to guess.

## Three incidents, one shape

The memory crash (fixed in the CDP stewardship work): every semantic snapshot and evaluation acquired remote object handles in Chrome's default object group, and nothing released them. Detached DOM trees accumulated behind the handles until Chrome reached fifteen gigabytes of resident memory and the machine went down. A discriminating spike confirmed the mechanism (10,005 live DOM nodes grew to 410,045 over forty operations when handles were retained, and stayed flat when released). The durable fix was not a memory monitor; it was ownership: each operation acquires a uniquely named object group and releases it on every exit path, including errors. The bad state, a handle with no owner, became unrepresentable.

The session leak (RFC 00009): `start_session` had no end. Sessions accumulated until broker exit, each one pinning its tab leases, and a crashed client left tabs locked to a ghost. The shipped fix is a TTL sweep on the maintenance tick, with tombstones so returning agents get an honest error, an in-flight guard so a session cannot expire mid-request, and a sweep-side emulation reset so expired leases do not orphan viewport overrides. All of it is correct, and all of it is compensation: four interacting mechanisms approximating what an observable lifetime would provide for free.

The screencast starvation (spiked, unfixed): broker-created tabs share one Chrome window, so activating any second tab makes the screencasted tab a background tab. macOS then reports the page hidden, the compositor stops producing frames, and a recording collapses to a single frame. The spike matrix made the boundary precise: full window occlusion by another application is harmless under the launch flags we already ship, activating a second window is harmless, but activating a sibling tab in the same window starves the cast to zero frames per second, and `Emulation.setFocusEmulationEnabled(true)` fully restores it. The vigilance-shaped fix is a listener that watches for the starved state and patches visibility back. The constructive fix is topology: tabs that never share a window cannot background each other.

## The ceiling

RFC 00009 is at the ceiling of acceptable compensating machinery, and this RFC exists partly to say so before the next incident pushes past it. The TTL interacts with the broker's idle-exit veto. The tombstone set needs its own cap and eviction order. The in-flight guard exists because a clock cannot see a request in progress. Each mechanism is small; the product of their interactions is the real complexity, and it grows multiplicatively with each addition. The lesson is not that the TTL was wrong. For bare MCP clients nothing observable exists today, so a clock is the honest fallback. The lesson is that the TTL should be understood as the fallback tier, not the primary design, and that new capabilities should not add machinery of this shape where a constructive alternative exists.

# Guide-level explanation

## What by construction means at the tool surface

An agent using the tools naively, calling them in the obvious order, forgetting everything a compaction would make it forget, gets correct behavior anyway. Correctness is a property of the tool shapes, not of the agent's diligence.

For agents: `new_tab` produces a tab that is alone in its browser window, so focusing one tab never silently stops another tab's recording. Starting a screencast means frames flow until the screencast stops; the contract does not have a footnote about window arrangement. Nothing new to remember, and one thing less (window topology) that could ever matter.

For humans: the managed Chrome grows one window per agent tab, cascaded, instead of one window sprouting tabs. Each window is still ordinary Chrome, inspectable and closable. Nothing else changes.

For contributors: the review question for new capabilities changes from did we handle the bad state to can the bad state be represented at all. A proposed feature that hands the agent something revocable must name the broker-side owner whose drop revokes it. If the only available owner is a clock, that is a signal the design wants restructuring, and the clock must be justified as a fallback tier rather than accepted as the mechanism.

# Worked example one: screencast frame starvation

This example is implementation-ready; the evidence lives in the `occlusion_starvation` spike.

**Topology by construction.** `create_page` passes `newWindow: true` to `Target.createTarget`, so every broker-created tab owns its window. The starvation hazard between broker tabs stops being a state we detect and becomes a state that cannot occur. The window-creation parameters accept position and size, so windows cascade rather than stack. The spike verified both halves on this exact machine: a fully covered window casts at full rate under our launch flags, and a second window activated directly over the casting window costs nothing.

**The screencast owns its frame guarantee.** One hole remains: pages spawn sibling tabs themselves, through `window.open` and `target=_blank`, and the protocol offers no command to move a tab between windows (verified against the vendored CDP surface). So the screencast resource itself carries the guarantee as internal behavior: while a cast is active, a `screencastVisibilityChanged visible=false` event engages focus emulation on the casting target, and stopping the cast, through any path including client disappearance, disengages it. The spike showed focus emulation restores a starved cast to full rate with the page reporting visible again. This is compensation, but encapsulated where compensation belongs: inside a resource whose lifetime the broker owns and whose teardown is deterministic. The agent-facing contract stays clean, and no agent action can leak the emulation.

**Adopted tabs run on the universal layer.** The two mechanisms compose as tiers. The screencast's internal guarantee is universal: it attaches to the cast, not to the tab's window arrangement, so it holds for any tab however the tab came to exist. The topology rule is the stronger tier broker-created tabs get for free: they never need the guarantee because no sibling can background them. A tab the broker adopted rather than created simply runs on the universal tier alone, and the contract is one sentence: adoption preserves the tab's existing window arrangement, and frame guarantees come from the screencast resource, which does not care.

Working through where an adopted tab actually comes from shows how little the weaker tier is exercised. Re-claims after session expiry or broker restart adopt tabs the broker itself created, which already own their windows; the guarantee survives adoption untouched. Page-spawned children (OAuth popups, `target=_blank` flows) are born siblings, and they are precisely what the universal tier exists for. The genuinely new case is collaboration with a human, and its realistic shape favors construction too: the managed Chrome for Testing is not anyone's daily browser, so a human almost never has a tab sitting in it ready to hand over. The flow that makes sense starts on the agent's side. The agent opens a window and brings it to the front, the human navigates it to the right place, and the human tells the agent to pick it up from there. The tab the agent picks up is broker-created and alone in its window, by construction, before the human ever touched it. If the human opened sibling tabs while driving, the universal tier covers them.

# Worked example two: session identity placement

The session handle, `agent_session_id`, lives today in the least reliable memory in the system: the agent's context window. Every downstream mitigation in RFC 00009 compensates for that placement.

RFC 00009 considered and rejected tying session lifetime to connection lifetime, because the MCP facade reconnects per request and no persistent connection exists whose closure means anything. That rejection was correct about the layer it examined and silent about the layer above it. The facade's connections are ephemeral, but the shims that own the facade are not: the VS Code extension surface and the codex plugin both know which conversation is calling, observe that conversation's end, and outlive every individual request. Identity can be injected there. The shim mints and attaches session identity per call, the model never sees a handle, and teardown fires on the conversation-end event the shim already receives. Forgetting becomes impossible because there is nothing to remember.

Bare MCP clients keep the explicit `start_session` protocol, and the RFC 00009 TTL remains as that tier's safety net. The reframe this RFC proposes: explicit handles plus TTL is the fallback tier for clients we do not control, not the primary design for the deployments we do.

This example is direction, not specification. The shim-side changes span two codebases, and the multi-agent cases (takeover, handoff between conversations) need the lease model's arbitration to survive the identity becoming implicit. A Stage 2 revision specifies this or splits it out.

# Drawbacks

One window per tab changes what the human sees. Some users will prefer tabs collected in one window, and window-per-tab consumes more screen and Dock real estate. The cascade mitigates but does not remove this.

Focus emulation is a lie told to the page. While engaged, `document.hasFocus()` reports true on a tab the user is not looking at, and pages that pause media or animations on blur behave differently under capture than they would for a real user. Scoping the emulation to the screencast's active lifetime bounds the lie but does not eliminate it, and a recording is precisely a claim about what the page did.

Shim-injected identity forks behavior between deployments. The VS Code and codex paths would carry implicit sessions while bare MCP carries explicit ones, and every session-adjacent feature would need to answer for both tiers.

The audit has a cost. Naming the rule obligates the existing surface to be reviewed against it, and some existing machinery (the TTL among it) survives the review as fallback tier rather than being removed, which means the codebase carries both shapes with a documented boundary between them.

# Rationale and alternatives

**Keep accumulating compensating machinery.** Each addition is locally cheaper than restructuring. Rejected as the default because the interactions already visible among the TTL, the idle veto, the tombstones, and the in-flight guard show the real cost is multiplicative, and because compensation degrades the agent contract (errors that say you waited too long) where construction leaves the contract clean.

**Fix the screencast reactively without the topology change.** The spike proves the listener works. Rejected as the primary fix because it is a daemon watching for a state we chose to keep representable, it engages only during screencasts (a backgrounded tab still loses rAF and frame production for any other consumer), and it treats the common broker-created case with the machinery that only the uncontrollable page-spawned case requires.

**Block page-spawned tabs outright.** A launch switch to that effect was not verifiable in the Chrome for Testing 150 string table, and blocking would break legitimate popup flows (OAuth among them). The encapsulated emulation handles this case without breaking anything.

**Relocate page-spawned tabs into their own windows.** No CDP command moves a tab between windows; verified against the vendored protocol. Not available.

**Hidden targets for agent-only tabs.** `Target.createTarget` accepts `hidden: true`, producing a target observable via protocol, absent from the tab strip, with lifetime bounded by the CDP session. That is RAII-shaped and appealing, but the lifetime binds to the broker's CDP connection, not the agent session, and a hidden tab inverts this project's premise that the browser is the user's visible workspace. Worth a spike for genuinely ephemeral work; not the default.

# Prior art

Rust's ownership model is the obvious namesake: the language-level insight that resource safety enforced by structure outlives resource safety enforced by discipline, and that the party holding the resource is the wrong party to trust with remembering its release. The codebase already practices this internally (`InFlightGuard`, per-operation object groups); this RFC extends the practice across the tool boundary.

Operating systems settled the handle-placement question decades ago: file descriptors live in a kernel-owned table keyed by process, and process exit closes them, no cooperation required. Capability systems make the same move more generally, and their literature is explicit that capabilities held in unreliable client memory must be revocable by the granting side. Erlang's monitors give the observing process a deterministic signal when the observed party dies, which is the shim-side conversation-end event in different clothing.

tmux is the interactive precedent: the server owns sessions, a client dying detaches rather than destroys, and reattachment is cheap. RFC 00009 borrowed its lease framing from DHCP; this RFC keeps that framing but files it where DHCP itself files it, as the recovery protocol for parties that cannot hold a connection, not the primary lifetime mechanism.

# Unresolved questions

Window placement policy: cascade geometry, multi-display behavior, and whether window position should be stable per tab across broker restarts.

Adoption as a first-class surface: `claim_tab` today serves both recovery re-claims and genuine adoption, and those deserve to be distinguished. Adoption of a page-spawned child and the collaborative pick-it-up-from-here flow (agent opens and fronts a window, human navigates, agent adopts) are different surfaces with different contracts, and neither is designed here. Both inherit this RFC's constraint: adoption never rearranges windows, and frame guarantees come from the screencast resource.

Implicit identity and multi-agent arbitration: takeover and handoff currently lean on distinct explicit session identities. The shim-injected design must preserve that arbitration, and the mapping from conversation to identity needs a story for one conversation driving multiple concurrent sessions, if that is ever legitimate.

Scope of the audit: which existing surfaces get reviewed against the rule in this RFC's implementation, and which are explicitly deferred with the TTL as their documented fallback.

