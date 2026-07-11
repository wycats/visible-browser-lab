<!-- exo:12 ulid:01kx79feb0qfea58tw8fq4djjq -->

# RFC 00012: Ownership-Aware Browser Cleanup

# Summary

Visible Browser Lab closes every browser target it created when the responsible session expires, whether that session is ambient or explicit and whether the target is still leased or has been ordinarily released. Targets that existed before VBL claimed them remain durable human state and are never closed by session expiry. A caller can explicitly preserve a VBL-created target with `release_tab({ leave_visible: true, user_instruction: "..." })`, which permanently removes VBL's cleanup authority for that target.

This RFC supersedes only the target-disposition rules in RFC 00009 and RFC 00011. Their session TTL, conversation identity, workspace binding, and non-disclosure contracts remain unchanged. Twill RFC 0013 is unaffected because cleanup ownership is VBL application policy, not conversation-identity infrastructure.

# Motivation

VBL currently records cleanup provenance only for targets created by ambient sessions while they remain actively leased. Explicit-session targets and released ambient targets survive indefinitely. Dogfood exposed the operational result: old agent-created windows accumulated in the managed Chrome profile across sessions and broker lifetimes, leaving many claimable tabs and gigabytes of renderer memory for the user to clean up manually.

The existing distinction between ambient and explicit sessions is not the relevant ownership boundary. VBL knows whether it created a target. That fact supplies cleanup authority regardless of how the session was identified. Conversely, a target discovered through the global inventory and adopted with `claim_tab` may contain durable human work, so session expiry is never authority to close it.

Release and preservation are also different operations. A routine release should end exclusive control and make a target claimable without silently turning an agent-created window into permanent browser state. Permanent preservation should require an explicit user instruction.

# Guide-level explanation

Agents continue to use `new_tab`, `claim_tab`, `release_tab`, and `close_tab` normally. A tab VBL opened remains eligible for automatic cleanup until it is closed or explicitly preserved. Calling `release_tab` makes it immediately claimable by another session, but does not by itself make it permanent.

When the user asks to leave a VBL-created tab visible after the session ends, the agent calls:

```json
{
  "tab_id": "tab-...",
  "leave_visible": true,
  "user_instruction": "Leave this page open so I can inspect it."
}
```

The non-empty instruction records the user's authority for the handoff. The target stays visible and claimable and VBL no longer closes it at expiry.

Tabs that were already open before VBL claimed them are unchanged by this RFC. Expiry releases their leases and leaves the targets open.

# Reference-level explanation

## Cleanup provenance

Cleanup provenance is private, target-level state recording that VBL created a target and which live session currently carries responsibility for it. `new_tab` and `start_session(start_url)` establish provenance for ambient and explicit sessions.

Claiming a target without cleanup provenance creates an ordinary borrowed lease. Claiming or taking over a target with cleanup provenance transfers responsibility to the adopting session. Transfer must include released targets: an earlier session may have released the target before the new session claimed it. The former session must not later close a target that the new session owns.

`close_tab`, confirmed target disappearance, and explicit preservation remove provenance. Broker restart does not reconstruct provenance from URLs, profiles, or target age; historical targets therefore receive no automatic startup sweep.

## Release contract

`release_tab` gains a dedicated parameter type with two optional fields:

- `leave_visible: boolean`, defaulting to `false`;
- `user_instruction: string`, accepted only with `leave_visible: true` and required to contain non-whitespace text in that mode.

A normal release transitions the lease to `Released`, removes active ownership, and keeps cleanup provenance attached to the target and session. The target is immediately claimable. If it remains unclaimed when that session expires, the expiry sweep closes it.

A preserving release also transitions the lease to `Released` and makes the target claimable, then removes cleanup provenance. The result is:

```json
{
  "released": true,
  "leave_visible": true
}
```

Normal release returns the same shape with `leave_visible: false`. Supplying `user_instruction` without `leave_visible: true` is invalid input.

## Expiry and races

The expiry sweep closes every cleanup-owned target associated with the expiring session in either `Active` or `Released` state. Borrowed active or missing leases transition to `Released` without touching Chrome.

Before an asynchronous close, the registry reserves the target. Claims fail while the reservation exists. A successful close or an already-missing target clears provenance and records the existing closed-target tombstone. A failed close releases the reservation and cleanup provenance so the surviving target is claimable rather than permanently stranded or repeatedly retried without a session owner.

Closing the final VBL-created page in a VBL-managed Chrome instance closes that managed browser when only Chrome-synthesized replacement targets remain, rather than leaving replacement New Tab windows behind. The next browser operation relaunches Chrome lazily. External CDP runtimes retain their existing lifecycle, and VBL does not close a managed browser while another independently created page target remains.

## Compatibility

Existing `release_tab` calls remain schema-valid and still leave the target visible immediately. Their longer-term behavior changes: a VBL-created target may close when its session expires unless the caller supplies the explicit preservation signal. This is the intended bug fix.

The optional `agent_session_id` ambient-session contract is unchanged. No conversation identity, workspace value, cleanup provenance, or internal session handle is rendered or logged.

# Drawbacks

An old client that used `release_tab` as a permanent handoff must opt into `leave_visible`. The new behavior is safer for unattended automation but makes the durability choice explicit.

Target-level provenance is more stateful than the current ambient-created lease set. Claims and takeovers must transfer it correctly, and expiry must consider released leases without allowing an old session to close a newly adopted target.

Provenance remains in memory. That is deliberate: persisting or heuristically reconstructing cleanup authority could close human state after a restart.

# Rationale and alternatives

**Keep ambient-only cleanup.** Rejected because explicit VBL-created targets have the same provenance and create the same window and memory leak.

**Treat ordinary release as permanent preservation.** Rejected because routine agents release leases as cleanup. Conflating lease release with a durable human handoff recreates the accumulation problem.

**Close claimed targets too.** Rejected because VBL did not create them and cannot infer that their contents are disposable.

**Reconstruct old provenance on startup.** Rejected because profile, URL, title, and age do not prove who created a tab. Existing windows require a user-reviewed cleanup manifest.

**Add a second session TTL for released targets.** Rejected because one session TTL already defines abandonment. A second clock adds policy without adding authority.

# Acceptance tests

- Ambient and explicit `new_tab` targets close when their sessions expire.
- A normally released VBL-created target remains claimable and closes if still unclaimed at expiry.
- A preserving release requires non-empty `user_instruction`, rejects an instruction without the flag, and survives expiry.
- A claimed pre-existing target is released but not closed at expiry.
- Cleanup provenance transfers when another session claims or takes over a VBL-created target, including after release; expiry of the former session cannot close it.
- `close_tab` and target disappearance clear provenance.
- Claim-versus-expiry reservations prevent adoption of a target selected for closure.
- Close failure releases the reservation and leaves the target claimable without cleanup ownership.
- Tool schemas, help, skill guidance, VS Code catalogs, and broker protocol agree on the new release shape.
- No cleanup provenance, user instruction, conversation identity, or internal ambient handle appears in ordinary logs or browser-operation results.
