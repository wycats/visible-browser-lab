<!-- exo:9 ulid:01kwwjym2ehpd629ammmxdhxn4 -->

# RFC 9: Session Lease Expiry

# Summary

Agent sessions gain a bounded lifetime. A session that no request has touched for a configurable window (sixty minutes by default) expires: its tab leases are released back to the pool, its private resources are reclaimed, and the next call that names it receives a precise error telling the agent to start a new session. Today a session lives until the broker exits, however long that is, and everything it owns lives with it. After this RFC, ownership is something a session keeps by using it, and an abandoned session returns what it borrowed.

This is the companion piece to RFC 00007. That RFC gave the broker process a bounded lifetime; this one does the same for the sessions inside it. The two together close a gap RFC 00007 deliberately left open: its idle exit is vetoed by live sessions, so a single abandoned session could still pin a broker indefinitely. Session expiry removes the pin.

# Motivation

A session is a standing claim on shared state. The broker hands out tabs from one managed Chrome that every client on the machine shares, and a session's leases are exclusive: while session A owns a tab, session B cannot drive it without an explicit, human-authorized takeover. Exclusivity is the point of the lease system, but exclusivity granted forever to a claimant that may no longer exist is not a lease, it is a leak.

Nothing today ends a session. The recon for this RFC confirmed it directly: there is no `end_session` operation anywhere in the codebase, not in the lease registry, not in the broker dispatch table, not on the tool surface. The sessions map grows monotonically until the broker process exits. Every crashed VS Code window, every killed agent harness, every conversation that simply ended leaves a session behind, still owning whatever tabs it had claimed. The tabs themselves remain visible in the user's browser, but they are locked to a ghost: any new session that tries to claim one gets `target_owned` and must escalate to a takeover, with the human instruction requirement that entails, to displace an owner that will never object.

The cost has been invisible for the same reason the orphaned-broker cost was invisible before RFC 00007: brokers restarted often enough during development that sessions rarely lived long. RFC 00007 makes brokers longer-lived in exactly the case that matters here, a machine where the agent works all day and the broker's idle exit never fires because sessions keep vetoing it. We built that veto with a staleness bound (a session defers idle exit only while touched within the last four idle windows) precisely because immortal sessions would otherwise make idle exit unreachable. That bound was a workaround for the absence of session expiry, and it has a real hole: the touch signal it consults is too narrow, so the veto can expire a session's *influence* while the session is actively in use. Fixing the signal and expiring the session itself subsumes the workaround.

There is also housekeeping that never happens. Sessions accumulate artifacts, screenshots, exported files, page captures, stored under a per-session directory on disk and indexed in a per-session registry. The artifact store has a `remove_session` method that removes a session's records and files. It has zero callers. Someone built the cleanup half of the lifecycle and nothing ever drives it, because nothing ever decides a session is over.

What acceptable looks like, concretely: a session that an agent is actively using, in any way, should never expire out from under it. A session whose client vanished should return its tabs to the claimable pool within a bounded window, without destroying anything a human can see. An agent that comes back to an expired session should get one crisp error that tells it exactly how to recover, and recovery should be cheap because expiry destroyed nothing it needs. And the broker's idle exit should stop being veto-able by ghosts.

# Guide-level explanation

## What agents see

In an active conversation, nothing changes. Every tool call an agent makes on its session, clicking, filling, taking snapshots, navigating, listing tabs, counts as using it, and a session in use does not expire. The expiry window only starts running when the agent stops calling entirely.

If an agent returns after a long silence, its first call fails with a `session_expired` error that names the session, says how long it sat idle, and carries the same recovery guidance as an unknown session: start a new session. The agent starts one, lists tabs, and re-claims the tab it was working in. The tab is still there, still showing whatever page it showed, because expiry released the agent's claim on the tab without touching the tab itself. The recovery is three calls and no lost browser state.

What an expired session does lose is its private, session-scoped material: artifacts it had captured but not exported, and the element references from its last snapshots. Both are reconstructible (take a new snapshot, capture a new screenshot), and neither is visible to the human, so reclaiming them destroys nothing a person was relying on. An agent that wants an artifact to survive its own absence should export it while it is still around; that was already true across broker restarts, and this RFC makes it true across long silences too.

## What humans see

Nothing, in the common case, and that is the design's central constraint. The tabs in the managed Chrome window are the user's visible workspace. Expiry releases the *lease* on a tab, the bookkeeping that says which agent may drive it, and deliberately never closes the tab, navigates it, or otherwise disturbs what is on screen. A user who walks away from a machine with agent-driven tabs open comes back to exactly those tabs, whether or not the sessions that opened them survived.

The one human-visible improvement is negative space: tabs stop being mysteriously locked. Today, when an agent's client dies, its tabs stay owned and a new agent needs a human-authorized takeover to reclaim them. After expiry, the dead session's tabs return to the pool on their own, and the new agent claims them without ceremony.

## How contributors should think about it

The lease registry already treats a session's relationship to a tab as a lease with explicit states. This RFC extends the same idea one level up, in exactly the way RFC 00007 extended it to the broker process: a session's existence is itself a lease, kept alive by use, expiring by default. The system's ownership story becomes uniform. Brokers own their tenancy while their files exist and someone needs them. Sessions own their leases while their clients use them. Nothing in the system owns anything forever.

The load-bearing definition is *use*. Expiry is only safe if the signal it watches is complete, meaning every request that names a session refreshes it. The current touch signal is not complete (it tracks ownership changes, not activity), so this RFC widens it before anything consumes it. That ordering matters and the implementation plan enforces it.

# Reference-level explanation

## The touch signal

`touch_session` today fires when a session's ownership state changes: claiming a tab, creating one, releasing one, closing one, and on the snapshot-refresh path that navigation and focus travel through. It does not fire for the interaction verbs. A session driving one tab through clicks, fills, key presses, snapshots, and evaluations, the actual shape of an agent working, generates no touches at all. Any expiry built on this signal would expire working sessions, and the staleness-bounded idle veto from RFC 00007 already consults it, which means an agent interacting steadily for longer than the grace window silently loses its power to defer the broker's idle exit.

The fix is to move the touch to the choke point. Every broker request that authenticates a session passes through a single dispatch path before fanning out to per-operation handlers; that path touches the session as part of resolving it. Once the touch lives there, the definition of "using a session" becomes "sending any request that names it," which is the only definition an agent author could reasonably assume. The per-operation touches become redundant and are removed rather than left as trap for future readers.

This step is a prerequisite, not a feature of expiry itself. It lands first, and it independently repairs the idle-veto hole whether or not expiry ships.

## The expiry sweep

Expiry rides the maintenance tick that RFC 00007 added to the serve loop. Each tick, alongside tenancy verification, the broker sweeps the session table and expires every session whose last touch is older than the TTL. There is no per-session timer and no new task; the sweep is a scan of a small in-memory map on a five-second cadence that already exists.

The TTL defaults to **sixty minutes**, configurable through `VISIBLE_BROWSER_LAB_SESSION_TTL_SECS`, with `0` disabling expiry entirely, the same convention as the broker's idle window. Sixty minutes is four times the broker's default idle window, deliberately mirroring the staleness bound it replaces: long enough that no plausible pause in an active working session (a meeting, a meal, a long build) hits it, short enough that a machine's tab pool recovers from a crashed client within the hour. The dispatch-time touch makes the in-flight race a non-issue by construction: a request touches its session on entry, so a session with a request in flight is by definition younger than any TTL worth configuring.

## What expiry releases, and what it removes

Expiry distinguishes shared state from private state, and the distinction determines the verb.

**Tab leases are released, never closed.** Each of the expired session's active leases transitions to `Released`, the same state an explicit `release_tab` produces, and the tab's entry leaves the active-target index. The Chrome tab itself is untouched. This is the same principle RFC 00007 applied to managed Chrome at broker shutdown: the browser is the user's visible state, and no bookkeeping deadline is ever grounds for destroying it. A released tab is immediately claimable by any session without takeover ceremony.

**Session-private resources are removed.** The session's artifact records and their on-disk files go through the existing (and currently orphaned) `remove_session` path; artifacts are session-scoped, unreachable from any other session, so retaining them after the session is unrecoverable would be a pure leak. Element references for the session's tabs are dropped the same way; they are meaningless outside the session that captured them.

**Target-keyed state is untouched.** Console and network diagnostics and screencast recordings key by browser target, not by session, and are shared across sessions by design. A session's death says nothing about whether that state is still wanted.

The session record itself is removed from the registry. This is the first code path in the system that removes a session, which is worth saying plainly because it means every downstream consumer of the session table meets a new possibility: a session id that once resolved and no longer does.

## The error contract

A request naming an expired session fails with a dedicated `session_expired` error code, distinct from `unknown_session`, carrying the session id, the idle duration that triggered expiry, and `RecoveryAction::StartSession`, the same recovery an unknown session prescribes. The distinct code exists for the reader, not the machine: `unknown_session` means "you never had this," while `session_expired` means "you had this and waited too long," and an agent (or a human reading an agent's transcript) diagnosing the two failures should not have to guess which happened. Distinguishing them requires the registry to remember expired ids, a small tombstone set bounded by normal broker lifetime, which the sweep maintains alongside the removal.

The MCP layer needs no new machinery: `BrowserToolError` already flows structured codes and recovery actions to agents, and every dispatch path already funnels through session resolution, so the new error emerges at exactly one place.

## Human handoff

The recon that preceded this RFC flagged handoff as the open question: `focus_tab` brings a tab forward precisely so a human can use it, the human's browsing generates no agent requests, and so handoff time looks like abandonment to any activity clock.

This RFC's answer is that handoff needs no special case, because expiry was designed so that being wrong is cheap. If a human inspects a handed-off tab for ninety minutes and the agent's session expires meanwhile, the recovery is the standard one: `session_expired`, start a new session, re-claim the tab, which is guaranteed still to be there because expiry never closes tabs. Nothing the human did is lost; nothing the agent needs is unrecoverable except un-exported artifacts, and an agent handing a tab to a human for indefinite inspection should export anything it means to keep regardless, since the human might equally close the client.

The alternative, a handoff marker that suspends expiry until the agent's next call, was considered and rejected because it recreates the immortal-session problem through the front door: an agent that hands off and never returns pins its leases forever, and handoff (the case where a human is *most* likely to walk away) becomes the case expiry cannot reach. A suspension with its own timeout is just a second, longer TTL with extra state to explain. One clock, cheap recovery, no suspension.

## Relationship to the broker's idle exit

RFC 00007's idle exit consults the session table through a staleness-bounded veto: sessions defer idle exit only while touched within four idle windows. That bound existed because sessions were immortal and an unbounded veto would have made idle exit unreachable. With expiry live, the bound is redundant: expired sessions leave the table, so the veto simplifies back to its natural form, *any live session defers idle exit*. The two clocks compose without coordination: a broker whose last session went quiet expires the session after the TTL, then, with the veto gone and no connections arriving, exits after the idle window. Worst-case time from last human-visible activity to a fully clean machine is TTL plus idle window, seventy-five minutes at the defaults, all of it reversible at any point by a single tool call.

The staleness-bounded veto ships in 0.4.3; this RFC removes it in favor of the simpler live-session veto once expiry provides the guarantee it approximated.

# Drawbacks

Expiry introduces the first path by which the broker unilaterally revokes something an agent was given. However cheap the recovery, an agent author now has a new failure mode to know about, and a skill or harness written against the old contract ("sessions live as long as the broker") could loop on a stale session id if it ignores the recovery action. The mitigation is the same one the rest of the tool surface leans on: structured errors with explicit recovery actions, which well-behaved agents already follow for `unknown_session`.

Losing un-exported artifacts on expiry is a real cost, not a rounding error. An agent that captured twenty screenshots during a long investigation and then paused for an hour comes back to none of them. The alternative, retaining artifacts past session death, was rejected as an unbounded leak (they are unreachable by construction from any other session), but the tradeoff is genuinely uncomfortable and the TTL default is chosen with it in mind.

The tombstone set that distinguishes `session_expired` from `unknown_session` is unbounded within a single broker lifetime. In practice broker lifetimes are now themselves bounded (RFC 00007) and sessions are created at human conversational rates, so the set stays trivially small, but a pathological client creating and abandoning sessions in a loop grows it until the broker recycles.

# Rationale and alternatives

**Expire leases but keep the session.** A lighter design would release an idle session's tabs but leave the session itself valid, so a returning agent resumes without an error. Rejected because it makes the system quieter but less honest: the agent's next act would operate on a session whose world changed underneath it (tabs gone, references stale) with no signal that anything happened. The explicit error is one call of overhead and removes a whole class of silent confusion. It also would leave artifacts and the session record leaking, which is half the problem this RFC exists to fix.

**Close tabs on expiry.** Symmetric cleanup would close the expired session's tabs too. Rejected for the same reason RFC 00007's shutdown never touches Chrome: tabs are human-visible state, and no timer should destroy what a person can see. The asymmetry (private state removed, shared state released) is deliberate and is the load-bearing safety property of the whole design.

**Tie session lifetime to connection lifetime.** LSP-style: session dies when its client's connection closes. Rejected because the MCP facade reconnects per request by design; there is no persistent connection whose closure means anything. The activity clock is the connection-free generalization, which is the same move the broker's idle exit made.

**Per-operation touch calls instead of dispatch-time touch.** Keeping touches distributed and just adding the missing verbs was considered. Rejected because the current incompleteness is exactly what distributed touch produces: every new operation is a chance to forget, and the failure mode (a verb that silently does not count as activity) is invisible until someone's session expires mid-work. One choke point makes completeness structural.

**A much shorter TTL with a keepalive.** Something like five minutes, with agents expected to ping. Rejected because it moves work onto every agent author to save the broker a small map, inverts the convenience gradient this tool surface tries to maintain, and still fails for agents that legitimately pause (human handoff, long approvals). Sixty minutes with no keepalive protocol asks nothing of well-behaved agents.

# Prior art

The nearest precedent is inside the project. RFC 00007 established the pattern this RFC completes: a standing claim (there, the broker's tenancy; here, a session's leases) must be re-justified by something observable, on a clock, with the reclamation path designed to be safe when it fires wrongly. The staleness-bounded idle veto in #42 was this RFC's design in embryo, built narrow because full expiry was out of scope.

DHCP is the classic external model: an address lease is kept by renewal, reclaimed on lapse, and the protocol's success rests on making renewal implicit in normal operation rather than a separate obligation. Kubernetes `Lease` objects work the same way for controller leadership, with holders refreshing `renewTime` and challengers displacing holders whose leases lapse; the displaced holder's recovery is to re-acquire, not to be consulted. Web application sessions are the ubiquitous version, idle-expiry with a bounded window and a re-authentication path, and they settled long ago on the property this RFC borrows: expiry must never destroy durable user data, only the authenticated claim to it.

The release-not-close asymmetry echoes how tmux and screen treat detached sessions' terminal state, and how sccache's idle shutdown (reviewed at source level for RFC 00007) drains client connections but never touches the cache it serves. The shared thing survives; the claim on it does not.

# How we prove it

Stage 3 requires all of the following, with evidence:

1. **The touch signal is complete.** A test drives a session through every interaction verb (click, fill, type, key, snapshot, evaluate, scroll) with expiry configured aggressively short, and the session survives, demonstrating that pure interaction defers expiry. A control session doing nothing expires in the same window.
2. **Expiry releases and removes the right things.** After a session expires: its tabs are claimable by a new session without takeover; the Chrome targets still exist; its artifact directory is gone; target-keyed diagnostics for its tabs are intact.
3. **The error contract is exact.** A call on an expired session yields `session_expired` (not `unknown_session`) with the idle duration and `StartSession` recovery; a call on a never-existent session still yields `unknown_session`.
4. **The veto simplification holds.** With expiry enabled, a broker whose only session is abandoned exits on its own within TTL plus idle window (compressed to seconds in the test configuration), demonstrating the two clocks compose.
5. **Dogfooded.** A release carrying expiry has been driven live through a real multi-session working day, including at least one deliberate long pause and recovery, without a session expiring mid-work or a tab being lost.

# Implementation plan

1. **Widen the touch signal** at the dispatch choke point; remove the per-operation touches; regression-test that every request verb touches. Ships independently and first, since it repairs the live idle-veto hole regardless of what follows.
2. **Expiry sweep and config plumbing**: TTL through `RuntimeConfig` (env var, `0` disables), sweep on the existing maintenance tick, release/remove semantics per the reference section, tombstone set.
3. **Error contract**: `session_expired` code, constructor, MCP flow-through, and the tombstone-backed distinction from `unknown_session`.
4. **Veto simplification**: replace the staleness-bounded idle veto with the live-session veto; adjust the RFC 00007 tests that encoded the bound.
5. **Ship and validate** per the Stage 3 criteria, in the release after 0.4.3.

# Unresolved questions

Whether the TTL should be surfaced per session at `start_session` time (an agent declaring "I intend long pauses") or remain a broker-level setting. The broker-level answer is simpler and is what this RFC specifies; a per-session override is backward-compatible to add later if a real agent workload demands it, and no current workload does.

Whether expired-session tombstones should survive broker restart. Nothing currently persists session state across restarts, and a post-restart `unknown_session` for a pre-restart session id is the same answer agents already receive today, so this RFC says no; revisit only if transcript-debugging experience shows the distinction matters across restarts in practice.
