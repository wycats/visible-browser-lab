<!-- exo:7 ulid:01kwsyhcwdss932zmswcs9eqa6 -->

# RFC 7: Broker Lifecycle Self-Ownership

# Summary

The Visible Browser Lab broker gains an intrinsic lifecycle: it continuously verifies its own tenancy claim (state directory, socket, pid file) and exits when the claim is gone or when it has been idle past a bounded window. Today every lifecycle correction is extrinsic — a *future client* must connect to the same socket and notice a mismatch. This RFC makes the broker able to answer "should I still exist?" from its own observations, and keeps the extrinsic probe as a second line of defense.

# Motivation

## The broker outlives every reason for its existence

`spawn_broker` starts the broker fully detached: no parent process ties, nothing ever waits on it. Its accept loop has no exit condition. Leases never expire. Once started, a broker runs until something outside it decides otherwise.

The existing correction mechanism is the client-side probe: `ensure_running` connects to the configured socket, pings, and compares the reported status against its own expectations (`broker_status_mismatch`). On mismatch it terminates the old daemon and spawns a replacement. This design silently assumes that **every socket will eventually be probed again**.

That assumption holds for exactly one socket — the user-cache singleton that installed clients share — and we have now hit both of the ways it fails:

1. **Version skew across upgrades.** The singleton *is* re-probed, but the probe's compatibility predicate was incomplete: it compared `protocol_version` (unchanged since 0.3.x) but not the package version. After the 0.4.2 upgrade, the freshly installed extension silently kept talking to the 0.4.0 daemon; the stale behavior surfaced during live smoke testing and cost a debugging round before the daemon was killed by hand. PR #41 fixed this instance by adding `package_version` to `BrokerStatus` and treating any difference as incompatible.
2. **Orphaned test brokers.** Each `headless_mcp` property test spawns a broker on a per-tempdir socket. When the test ends, the harness kills the MCP *client*; the tempdir — socket file and all — is deleted. Nobody ever contacts that socket path again, so the extrinsic correction never fires. The broker is immortal. An audit on 2026-07-05 found **195 orphaned broker daemons** accumulated over three days of test runs on a single development machine.

These are not two bugs. They are two instances of one class: **a daemon whose continued existence is justified by nothing it can observe.** Fixing instances (as #41 did, and as a harness `Drop` impl would) leaves the class open — the third instance will be some socket we have not thought of yet, discovered the same way these were: by surprise.

## What acceptable looks like

- A broker whose state directory or socket vanishes exits promptly on its own.
- A broker that has served no one for a bounded period exits cleanly, because `ensure_running` can recreate it on demand in well under a second and managed Chrome re-adoption (`reused: true`) means the user's visible browser survives the gap.
- The test suite leaves zero broker processes behind, and asserts so.
- None of this depends on a future client happening to connect.

# Guide-level explanation

## For users

Nothing visible changes in normal operation. The broker still starts on demand and is shared by every client on the machine. What changes is what happens when the broker stops being needed:

- If you clear the cache directory (or an upgrade replaces it), the running broker notices within seconds and exits, instead of squatting on the old socket until something kills it.
- If no client has used the broker for a while (default: 15 minutes with no connections and no active sessions), it exits. The next tool call transparently starts a fresh one. Your managed Chrome window is untouched — the new broker re-adopts it.
- `pkill visible-browser-lab-mcp` is no longer part of anyone's upgrade ritual. (PR #41 already handles upgrades; this RFC removes the need for extrinsic correction at all in the common case.)

## For contributors

The broker's tenancy claim is the set of filesystem artifacts it owns: its state directory, its socket path (on platforms where the endpoint is a filesystem object), and its pid file. The broker now treats that claim as a lease on its own existence, re-verified on a timer:

- **Claim gone → exit.** State dir deleted, socket file missing, or pid file present but naming a different pid (a replacement broker has taken over) — each means this broker's tenure has ended.
- **Idle → exit.** Zero open client connections and zero live sessions for the full idle window means nobody needs this broker. It shuts down cleanly: stops accepting, removes the socket and pid file (only if the pid file still names it), and exits 0 with a logged reason.

Test harnesses get a short idle window via configuration, and the suite gains an end-of-run assertion that no broker with a test state-dir survives.

# Reference-level explanation

## Tenancy verification

A `tenancy` task runs inside the broker on a fixed cadence (5 seconds). Each tick performs three checks, all local `stat`-class operations:

1. **State directory exists.** `config.state_dir` must still be a directory. Tempdir deletion (the test-orphan case) trips this within one tick.
2. **Endpoint object exists,** on platforms where `endpoint.stale_path()` is `Some` (Unix domain sockets). Windows named pipes have no filesystem object; the state-directory check carries the weight there, which is sufficient because per-tempdir test brokers place their state dir inside the tempdir.
3. **Pid file names us.** If `config.pid_path` exists and parses to a pid other than `std::process::id()`, a replacement broker has claimed the socket path (the restart flow in `restart_incompatible_broker` deletes and rewrites these files). The incumbent must stand down.

Any failed check logs the specific reason and begins shutdown. The checks are deliberately conservative: transient read errors (EINTR, permission blips) do not trip them; only a definitive negative does.

## Idle tracking

The broker maintains two counters it already has the raw material for:

- **Open connections**: incremented on accept, decremented when `handle_connection` returns.
- **Live sessions**: the lease table's session count.

The broker records `last_activity` whenever either counter is nonzero, or when any request is dispatched. The tenancy tick computes idleness as *both counters zero* and *now − last_activity ≥ idle window*. In-flight long operations hold a connection open, so they inherently defer idleness — no separate bookkeeping needed.

The idle window defaults to **15 minutes**, configurable via `VISIBLE_BROWSER_LAB_BROKER_IDLE_TIMEOUT_SECS` and a `--idle-timeout-secs` broker flag (flag wins). `0` disables idle exit (opt-out for unusual deployments); tenancy verification cannot be disabled.

## Shutdown sequence

1. Log the reason (`tenancy: state dir removed`, `idle: 900s with no connections or sessions`, ...).
2. Stop accepting: close the listener. New connectors get `ECONNREFUSED`/pipe-not-found, which `ensure_running` already treats as "no broker; take the start lock and spawn" — the existing retry loop absorbs the race.
3. Drain: wait for open connections to finish, bounded by a short deadline (5 seconds), then proceed regardless.
4. Release the claim: remove the socket file and pid file, each only if it still belongs to this broker (pid file re-read and compared before unlink).
5. Exit 0.

Managed Chrome is **always left running**. Re-adoption is the designed path (`ensure_managed_chrome` returns `reused: true` against a live profile), and terminating a visible browser on an idle timer would destroy user state for no benefit.

## Race analysis

- **Client connects during shutdown.** Window between "listener closed" and "socket unlinked": connector fails, retries, hits the start lock, spawns a fresh broker after the old one releases its files. The start-lock path already serializes this.
- **Replacement broker vs. lingering incumbent.** The pid-file check makes the incumbent self-evict; `restart_incompatible_broker` also terminates it by pid, so this is belt-and-suspenders in both directions.
- **Idle exit races a new session.** The accept between tick N's idle verdict and shutdown's listener close: shutdown re-checks the connection counter after closing the listener; if a connection slipped in, shutdown aborts and the broker resumes. (Listener close before the re-check means the racing client may need one retry — again absorbed by `ensure_running`.)

## Test harness changes

- `McpClient::shutdown` (test-support) additionally terminates the broker recorded in the harness state dir's pid file, as defense in depth for suite runs on machines predating this RFC's broker.
- Property-test and integration-test configs set a short idle window (2 seconds) so that even SIGKILLed harnesses leave brokers that expire within seconds.
- A suite-level check (xtask or CI step) asserts no `visible-browser-lab-mcp broker` process with a `--state-dir` under the temp root survives the run.

## Interaction with PR #41 (extrinsic correction)

The probe-and-restart path remains unchanged and necessary: it is the *replacement* mechanism (upgrade arrives while a healthy same-socket broker is running and must be swapped). This RFC covers the *abandonment* mechanism (no future client will ever connect). The two compose: every broker is guaranteed a supervisor — either the next client, or itself.

# Drawbacks

- **Restart latency after idle exit.** The first tool call after a quiet quarter-hour pays broker startup (sub-second) plus Chrome re-adoption (fast; the profile and DevToolsActivePort are already on disk). This is a real but small cost, and the alternative — daemons that live forever — has now produced two field incidents.
- **Self-terminating daemons are harder to reason about in logs.** Mitigated by logging every exit with its reason and by the broker's existing structured logging; "why did the broker exit" is answerable from `broker.stderr.log`.
- **More lifecycle states means more races.** The race analysis above covers the three we can construct; the start-lock and retry loop in `ensure_running` were built for exactly this shape of problem and absorb each residual window.

# Rationale and alternatives

- **Parent-tied lifetime** (broker dies with the client that spawned it): wrong model — the broker is deliberately a shared multi-client daemon; the first client exiting must not kill the second client's session.
- **Version-stamped socket paths** (`broker-0.4.2.sock`): solves skew only, leaks one daemon per release, and abandons the handshake-and-replace mechanism that works today. Rejected in #41's design discussion.
- **Harness-only cleanup** (test `Drop` kills the broker): fixes the known instance, not the class. Harnesses die by SIGKILL and panic-abort; `Drop` does not run. The 195-daemon audit is precisely the residue of "cleanup code that usually runs."
- **OS service supervision** (launchd/systemd socket activation): heavyweight, per-platform, and wrong for per-tempdir test brokers, which are exactly the population that leaks.
- **Lease expiry instead of broker exit**: expiring leases would bound *session* lifetime but not *process* lifetime — an empty broker still runs forever. Lease TTLs may be worth pursuing separately; they are out of scope here.

# Prior art

- `gpg-agent` and `ssh-agent` both self-terminate on socket removal and support idle-based expiry of cached material; `gpg-agent` in particular watches its socket directory and exits when displaced.
- Language-server processes conventionally exit when their client connection closes or their workspace disappears; the multi-client analogue is "exit when *all* clients are gone for a window."
- systemd socket activation demonstrates the recreate-on-demand pattern this RFC relies on: cheap restart makes aggressive shutdown safe.

# Unresolved questions

- **Idle window default.** 15 minutes is a judgment call balancing restart latency against daemon lifetime; usage may argue for longer. The configuration surface makes this tunable without another RFC.
- **Windows endpoint verification.** Named pipes leave the state-directory check as the only filesystem anchor on Windows. If a Windows deployment ever separates state dir from pipe lifetime, check 2 needs a platform-specific answer (e.g., periodic zero-timeout pipe self-connect).
- **Telemetry on idle exits.** Whether to surface "broker exited idle N times today" anywhere beyond the log file. Deferred until there is evidence anyone needs it.

# Implementation plan

1. Idle/tenancy config plumbing (`RuntimeConfig`, env var, flag) with tests.
2. Tenancy tick + shutdown sequence in the broker, behind the existing tokio runtime; unit tests for each check with a scratch state dir.
3. Idle counters threaded through `serve`/`handle_connection` and the lease table; integration test: broker with 2-second window exits after last session closes, files released.
4. Harness updates and the suite-level no-survivors assertion.
5. Release in 0.4.3 alongside #41; release notes mention both halves (probe completeness + self-ownership).

