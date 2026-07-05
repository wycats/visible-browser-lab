<!-- exo:7 ulid:01kwsyhcwdss932zmswcs9eqa6 -->

# RFC 7: Broker Lifecycle Self-Ownership

# Summary

The Visible Browser Lab broker gains an intrinsic lifecycle. It re-verifies its own tenancy claim — the state directory, socket, and pid file that justify its existence — on a short cadence, and it exits on its own when that claim disappears or when it has served no one for a bounded window. Today, every lifecycle correction is extrinsic: a future client must connect to the same socket and notice that something is wrong. After this RFC, the broker can answer the question "should I still exist?" from its own observations, and the extrinsic probe becomes a second line of defense rather than the only one.

# Motivation

The broker is spawned fully detached. Nothing waits on it, nothing supervises it, and its accept loop has no exit condition. Sessions and leases live until a client releases them, and the process itself lives until something outside it decides otherwise. This was a deliberate simplification: the broker exists to outlive any single client, so tying it to a parent process was wrong, and in the common case it works exactly as intended.

The mechanism that keeps this arrangement honest is the client-side probe. When a client needs a broker, `ensure_running` connects to the configured socket, pings, and compares the reported status against its own expectations. If the running broker doesn't match — wrong protocol version, wrong runtime mode, wrong CDP endpoint — the client terminates it and spawns a replacement. Correction happens at the moment of next contact.

The trouble is the phrase *next contact*. The probe-and-replace design silently assumes that every socket a broker listens on will eventually be probed again. That assumption is true for exactly one socket — the shared singleton in the user's cache directory, which every installed client connects to — and it is false everywhere else. We have now been bitten by both halves of this observation, in the same week.

The first incident was version skew. The singleton socket *is* re-probed constantly, but the probe's compatibility predicate was incomplete: it compared the broker protocol version, which has not changed since 0.3.x, and said nothing about the package version. When the 0.4.2 extension was installed, it connected to the still-running 0.4.0 broker, found the protocol agreeable, and proceeded — so a freshly upgraded installation silently ran months-old broker behavior. The stale daemon was only discovered because a live smoke test checked for a behavior change that had just shipped, and it cost a debugging round before anyone thought to suspect the daemon's age. PR #41 fixed this instance by adding `package_version` to `BrokerStatus` and treating any difference as grounds for replacement.

The second incident was quieter and larger. Each `headless_mcp` property test spawns its own broker on a socket inside a per-test temporary directory. When the test finishes, the harness kills the MCP client it spawned, and the temporary directory — socket file included — is deleted. No client will ever connect to that socket path again, which means the extrinsic correction will never fire, which means the broker is immortal. An audit on 2026-07-05 found **195 orphaned broker daemons** on a single development machine, accumulated over three days of ordinary test runs.

It would be easy to treat these as two unrelated bugs: patch the probe (done, in #41), add a `Drop` implementation to the test harness that kills the broker (easy enough), and move on. But they are not two bugs. They are two instances of one class: **a daemon whose continued existence is justified by nothing it can observe.** The version-skew daemon had a live socket and a supervisor that was checking the wrong things; the orphaned daemons had sockets nobody would ever check. In both cases the broker itself had everything it needed to notice the problem — its binary had been replaced on disk, its socket had been deleted out from under it — and no code that looked.

Fixing instances leaves the class open. The third instance will be some socket we have not thought about yet, and we will find it the way we found these: by surprise, after accumulation. The class-level fix is to make the broker supervise itself.

What acceptable looks like, concretely: a broker whose state directory or socket vanishes should notice within seconds and exit; a broker that has served no clients for a bounded period should exit cleanly, because recreating one on demand costs well under a second and the managed Chrome window survives the gap; the test suite should leave zero broker processes behind and should assert as much. None of this may depend on a future client happening to connect.

# Guide-level explanation

## What users see

In normal operation, nothing changes. The broker still starts on demand, still shares one managed Chrome across every client on the machine, and still hands out isolated tabs per session. The changes appear at the edges of the lifecycle, which is precisely where today's behavior is surprising.

If you clear the cache directory — or an uninstall, upgrade, or cleanup task does it for you — the running broker notices within a few seconds and exits. Today it would squat on the deleted socket path indefinitely, invisible except in `ps` output.

If nothing has used the broker for a while — no connections, no live sessions, for fifteen minutes by default — it exits on its own. The next tool call transparently starts a fresh broker, which re-adopts the managed Chrome profile that is already running. Your browser window, tabs, and login state are untouched; the only cost is a sub-second broker startup on the first call after a quiet stretch.

And with #41's probe fix and this RFC together, `pkill visible-browser-lab-mcp` stops being part of anyone's upgrade ritual. The probe handles the case where a new client meets an old broker; self-ownership handles the case where no new client is coming.

## How contributors should think about it

The broker's claim to exist is not abstract — it is a set of filesystem artifacts the broker owns: its state directory, its socket (on platforms where the IPC endpoint is a filesystem object), and its pid file. Those artifacts are how clients find the broker and how replacement brokers displace it. This RFC's central move is to treat that claim as a *lease on the broker's own existence*, re-verified from inside on a timer.

When the claim fails verification — the state directory is gone, the socket file is gone, or the pid file now names a different process because a replacement broker has taken over — the broker's tenure has ended, and it shuts down. When the claim is intact but nobody has needed the broker for the full idle window, the broker concludes that its work is done and shuts down too, releasing its artifacts on the way out.

The key property in both cases is that shutdown is *safe by construction*: the client side was already built to treat "no broker answering" as "take the start lock and spawn one," so a broker that exits at an inconvenient moment costs the next caller one retry, not an error. Cheap recreation is what makes aggressive self-termination reasonable — the same insight that underlies socket-activated system services.

Test harnesses configure a short idle window, so even a harness that dies by SIGKILL — where no cleanup code runs at all — leaves a broker that expires seconds later on its own.

# Reference-level explanation

## Tenancy verification

A tenancy task runs inside the broker on a five-second cadence. Each tick re-verifies the broker's claim with three checks, all of them cheap local filesystem operations.

The first check is that `config.state_dir` still exists and is a directory. This is the check that catches the orphaned-test-broker case: the harness's temporary directory is deleted when the test ends, and the broker notices on the next tick.

The second check is that the IPC endpoint's filesystem object still exists, on platforms where there is one. Unix domain sockets have a socket file (`endpoint.stale_path()` returns `Some`); Windows named pipes do not, so this check is skipped there. That asymmetry is acceptable because the state-directory check carries the weight on Windows: test brokers place their state directory inside the same tempdir as everything else, so deletion of the tempdir still trips verification, just via a different check.

The third check is that the pid file, if present and parseable, names this process. The restart flow in `restart_incompatible_broker` deletes the old broker's files and the replacement writes its own pid file; if an incumbent broker survives its own termination attempt for any reason, it will observe a pid file naming its successor and stand down voluntarily. This makes displacement safe from both directions — the replacer kills by pid, and the displaced broker also self-evicts.

Verification is deliberately conservative about what counts as failure. A transient read error — an interrupted syscall, a permissions hiccup — does not end the broker's tenure; only a definitive negative does (the path affirmatively does not exist, or the pid file affirmatively names someone else). The failure mode this conservatism accepts is that a genuinely dead claim survives a few extra ticks; the failure mode it prevents is a healthy broker exiting because of filesystem noise.

## Idle tracking

The broker already has the raw material for an idleness judgment; this RFC only asks it to keep two counters current. Open connections are counted by incrementing on accept and decrementing when `handle_connection` returns. Live sessions are already counted by the lease table. The broker records a `last_activity` timestamp whenever either count is nonzero or any request is dispatched.

The tenancy tick then computes idleness as: both counters zero, *and* the time since `last_activity` meets the idle window. Long-running operations need no special treatment — an in-flight request necessarily holds its connection open, so it defers idleness by existing.

The idle window defaults to **fifteen minutes**. It is configurable through the `VISIBLE_BROWSER_LAB_BROKER_IDLE_TIMEOUT_SECS` environment variable and an `--idle-timeout-secs` flag on the broker subcommand, with the flag taking precedence. A value of zero disables idle exit entirely, as an escape hatch for unusual deployments. Tenancy verification has no such escape hatch: a broker whose claim is gone has no legitimate reason to keep running, in any deployment.

## Shutdown sequence

Shutdown proceeds in five steps, and the ordering matters.

The broker first logs the reason it is exiting — `tenancy: state dir removed`, `idle: 900s with no connections or sessions` — so that "why did the broker exit" is always answerable from `broker.stderr.log`. It then closes its listener, so new connectors fail fast with connection-refused rather than queueing behind a shutdown in progress; `ensure_running` already interprets that failure as "no broker here; take the start lock and spawn," so the racing client recovers without new code. Third, it drains: open connections get a short bounded grace period (five seconds) to finish their in-flight work, after which shutdown proceeds regardless. Fourth, it releases its claim, removing the socket file and pid file — each only after re-checking that the artifact still belongs to it, so a displaced broker cannot delete its successor's files. Finally it exits with status zero.

One step deserves emphasis for what it does *not* do: shutdown never touches managed Chrome. Re-adoption of a live Chrome profile is the designed path — `ensure_managed_chrome` returns `reused: true` against a running profile — and terminating a user's visible browser because a daemon's idle timer fired would destroy real user state to save nothing. The broker's lifecycle and the browser's lifecycle are deliberately decoupled; this RFC tightens the former and leaves the latter alone.

## Races

Three races are constructible, and each resolves through machinery that already exists.

A client can connect in the window between the listener closing and the socket file being unlinked. It experiences a refused connection, retries, and lands in the start-lock path, which serializes broker creation; it gets a fresh broker after the old one finishes releasing its files. This is the same path a client takes today when it beats the broker's startup, so no new behavior is required.

A replacement broker can find its predecessor still alive — termination is asynchronous, and a process can linger briefly after being signaled. The pid-file check resolves this from the incumbent's side: it observes the successor's pid file and self-evicts. The successor, meanwhile, has already signaled it by pid. Displacement converges from both directions.

An idle verdict can race a brand-new session: the tenancy tick judges the broker idle at the same moment a client's connection is being accepted. Shutdown re-checks the connection counter after closing the listener; if a connection slipped in before the close, shutdown aborts and the broker resumes normal operation. If the client instead arrived just after the close, it is the first race again — one retry, fresh broker. Either way no session is lost, because the session had not been established yet.

## Test harness changes

The harness changes are defense in depth rather than the primary mechanism — the primary mechanism is the broker expiring on its own.

`McpClient::shutdown` in test-support additionally terminates the broker named by the harness state directory's pid file, covering suite runs against broker binaries that predate this RFC. Property-test and integration-test configurations set the idle window to two seconds, so even a SIGKILLed harness — the case where no `Drop` implementation anywhere will run — strands a broker for seconds rather than forever. And the suite gains an end-of-run assertion, in xtask or CI, that no broker process with a state directory under the temp root survived. That assertion is the regression test for this RFC's core promise, phrased in the only terms that matter: process count, zero.

## Relationship to the extrinsic probe

PR #41's probe-and-replace path remains unchanged and remains necessary. The probe is the *replacement* mechanism: it handles the case where an upgrade arrives while a healthy broker is running on the shared socket and must be swapped for a newer one. This RFC adds the *abandonment* mechanism: it handles the case where no future client will ever connect. The two compose into a complete supervision story — every broker is guaranteed a supervisor, either the next client to arrive or, in the limit, itself.

# Drawbacks

Idle exit trades a small, visible cost for the invisible one we have been paying. The first tool call after a quiet stretch pays broker startup plus Chrome re-adoption — sub-second in practice, since the profile and `DevToolsActivePort` file are already on disk, but not free. Fifteen minutes is chosen to make this cost rare in an active working session while still bounding daemon lifetime to something a human would recognize as reasonable.

Self-terminating daemons are also genuinely harder to reason about than immortal ones: "the broker exited" becomes an expected log line rather than evidence of a crash. The mitigation is that every self-initiated exit logs its reason before doing anything else, so the log always distinguishes a tenancy exit from an idle exit from an actual failure.

Finally, more lifecycle states mean more interleavings. The race analysis above covers the three we can construct, and all three resolve through the pre-existing start-lock and retry machinery — but "the races we can construct" is not a proof, and the property tests should continue to hammer session establishment against broker churn.

# Rationale and alternatives

**Tie the broker's lifetime to its parent.** The simplest lifecycle is "die when the spawning client dies," and it is the wrong one here. The broker is deliberately a shared, multi-client daemon; the first client exiting must not tear down the second client's session. Parent-tied lifetime solves the orphan problem by giving up the design's central feature.

**Version-stamp the socket path** (`broker-0.4.2.sock`). This makes version skew structurally impossible — each release talks to its own socket — but it solves only skew, leaks one orphaned daemon per release by construction, and abandons the handshake-and-replace mechanism that already works. It was considered and rejected during #41's design discussion.

**Fix the harness and call it done.** A `Drop` implementation that kills the broker fixes the known instance and not the class. Harnesses die by SIGKILL, panic-abort, and CI timeout cancellation; destructors are a courtesy, not a guarantee. The 195-daemon audit is precisely the residue of three days of "cleanup code that usually runs." Harness cleanup is worth having — this RFC includes it — but as a supplement to self-supervision, not a substitute.

**Delegate supervision to the OS** (launchd, systemd socket activation). Socket activation embodies the right insight — cheap restart makes aggressive shutdown safe — but adopting it means per-platform service definitions, install-time registration, and a supervision model that does not exist for the per-tempdir test brokers, which are exactly the population that leaks. The RFC borrows the insight and skips the machinery.

**Expire leases instead of the process.** Lease TTLs bound *session* lifetime, not *process* lifetime; an empty broker still runs forever, so the orphan class survives. Lease expiry may be independently worthwhile — a crashed client currently strands its leases until the broker restarts — but it is a different problem, and it is out of scope here.

# Prior art

`gpg-agent` and `ssh-agent` are the closest relatives: user-level daemons, started on demand, shared by many clients, with no natural parent to tie themselves to. Both self-terminate when displaced — `gpg-agent` watches its socket directory and exits when the socket is removed or replaced — and both treat idle expiry of their contents as normal operation. Their longevity as designs suggests that self-supervision is the stable equilibrium for this shape of daemon.

Language servers converge on the same answer from a different direction: the LSP lifecycle convention is that a server exits when its client connection closes. The broker is the multi-client generalization, and "exit when all clients are gone and none has arrived for a window" is the natural generalization of that convention.

systemd's socket activation is prior art for the enabling assumption rather than the mechanism: when restart is cheap and state is recoverable, processes do not need to be precious. The broker's state is recoverable by design — Chrome re-adoption, sessions re-established by clients — so it qualifies.

# Unresolved questions

The fifteen-minute idle default is a judgment call, balancing restart latency against daemon lifetime, and real usage may argue for a longer window. The configuration surface exists precisely so this can be tuned from evidence without another design round.

Windows verification currently leans entirely on the state-directory check, because named pipes leave no filesystem object to watch. If a Windows deployment ever separates the state directory's lifetime from the pipe's, the second check needs a platform-specific answer — likely a periodic zero-timeout self-connect to the pipe — and that work is deferred until such a deployment exists.

Whether idle exits deserve telemetry beyond the log line — "broker exited idle N times today" surfacing somewhere a user would see it — is deferred until there is evidence anyone needs it.

# Implementation plan

Implementation proceeds in four steps, each independently testable, followed by release.

1. Configuration plumbing: the idle window through `RuntimeConfig`, the environment variable, and the broker flag, with precedence tests.
2. The tenancy task and shutdown sequence, with unit tests driving each check against a scratch state directory (delete the dir, delete the socket, swap the pid file) and asserting clean file release.
3. Idle counters threaded through `serve`, `handle_connection`, and the lease table, with an integration test: a broker configured with a two-second window exits after its last session closes, and its socket and pid file are gone afterward.
4. Harness updates and the suite-level no-survivors assertion.
5. Ship in 0.4.3 alongside #41, with release notes presenting both halves as one story: the probe now catches version skew at next contact, and the broker no longer depends on a next contact ever coming.
