<!-- exo:13 ulid:01kxc6epqr49amhsdnbqznbjmj -->

# RFC 00013: Durable Streaming Screencasts

# Summary

Visible Browser Lab will produce long-form screencasts as durable streaming jobs rather than retaining every frame and encoding the complete recording inside the `stop` request.

A screencast reserves private artifact storage when recording begins, moves frames through a bounded pipeline while recording is active, and finalizes under broker ownership. The public lifecycle is `recording → finalizing → ready | error`. Once a caller has successfully started a recording, cancellation or timeout of the caller that stops it cannot discard the completed capture.

The proposed primary backend is a hidden Chrome recorder target. The source tab supplies bounded CDP screencast frames; a broker-owned hidden target decodes those frames, draws them into a fixed-size canvas, and uses `MediaRecorder` to emit timesliced VP8-in-WebM chunks into the reserved artifact. A native six-runner feasibility matrix validated this backend on VBL's macOS, Linux, and Windows ARM64 and x86_64 release platforms.

This RFC changes only screencast acquisition, encoding, and artifact finalization. It preserves tab leases, conversation identity, cleanup provenance, the default silent WebM result, and the existing session-owned artifact model. It supersedes the screencast implementation details in RFC 00005 and applies the stewardship rule from RFC 00010 to recording and finalization.

# Motivation

VBL 0.4.8 records a screencast by collecting JPEG frames in memory. `screencast stop` then decodes every retained JPEG to RGB, converts every pixel to I420 in a scalar loop, and encodes the complete recording as AV1 with a two-thread rav1e encoder. Only after that synchronous work finishes does the broker create the artifact.

That shape failed under ordinary reviewer-video workloads. A 45-second 1440×900 recording at 15 frames per second and a 50-second Retina recording both exceeded the host's 300-second tool-call timeout. An isolated 45-second 1280×720 recording at 5 frames per second and quality 50 still did not finish within 300 seconds. During encoding, the broker consumed roughly two cores and grew to approximately two gigabytes of resident memory.

The timeout is also a durability failure. The blocking encoder can continue after the request future is cancelled, but artifact publication belongs to the cancelled request. The recording can therefore consume several more minutes of CPU and still leave no artifact to retrieve.

The problem is architectural rather than an AV1 tuning problem:

- acquisition retains a duration-sized batch instead of feeding a bounded consumer;
- sampling assumes a 60 Hz source instead of using CDP presentation timestamps;
- encoding starts only after recording stops;
- finalization belongs to the caller's request rather than the broker resource;
- artifact identity and storage do not exist until all expensive work has succeeded.

A recording is a broker-owned resource. Its lifetime and result should not depend on a model, host, or transport keeping one RPC alive through the slowest part of the pipeline.

# Guide-level explanation

An agent starts and stops a screencast as it does today. The common successful stop remains simple: it returns the completed session-owned video artifact.

The default recording fits the source page within 1280×720, preserves its aspect ratio, records for at most 30 seconds unless the caller raises the existing duration limit, and produces a silent VP8-in-WebM artifact. Callers can request a different bounded size, frame rate, quality, or maximum duration.

If finalization does not complete during stop's five-second wait, stop returns a `finalizing` state rather than holding the tool call indefinitely. The agent can call `screencast status` or repeat `stop`; both observe the same job. Once ready, either operation returns the artifact. The agent does not restart the capture or carry an internal encoder handle.

If the source tab navigates while recording, the recording continues as long as the underlying target remains the same. If the target disappears, Chrome becomes unavailable, or the encoder fails, status reports a stable error for that recording and VBL removes its owned partial output.

Closing a tab or expiring its session cancels an unfinished recording and removes partial output. No hidden recorder target appears in the user's tab strip, lease inventory, cleanup manifest, tool results, or logs.

# Reference-level explanation

## Public parameters

The `start` operation keeps its existing parameters:

- `fps`, default 10 and bounded from 1 through 30;
- `quality`, default 70 and bounded from 1 through 100;
- `max_duration_ms`, default 30,000 and bounded from 1,000 through 300,000.

`fps` is a target upper bound, not a frame-delivery guarantee. VBL drops frames when the source compositor or encoder cannot sustain the requested rate. `quality` is a backend-independent fidelity hint: higher values may increase source JPEG quality, encoder bitrate, and artifact size, but do not promise a particular codec quantizer or bitrate.

The operation adds `max_width` and `max_height`. They must be supplied together, must be even integers, and are bounded from 16×16 through 3840×2160. When absent they default to 1280×720. Chrome scales the source to fit within those bounds while preserving aspect ratio; VBL does not stretch or crop the page.

## Recording lifecycle

Starting a recording creates a broker-owned job and reserves a private partial path under the existing session artifact generation. The job owns:

- the source target and session association;
- source CDP capture and focus-emulation cleanup;
- the bounded frame queue;
- the hidden recorder target and its CDP binding;
- the partial WebM output;
- public counters and terminal error information;
- final publication into the artifact registry.

The public states are:

- `recording`: accepting source frames;
- `finalizing`: source capture has stopped and the WebM is being completed;
- `ready`: an immutable artifact summary is available;
- `error`: the job failed and its partial output has been removed.

The existing `recording` boolean remains for compatibility. It is true while the job is `recording` or `finalizing`, indicating that the tab cannot start another recording, and false in terminal states. The new `state` field is authoritative.

`stop` is idempotent for the current job. The first call stops source capture and begins finalization. Repeated calls during `finalizing` observe the same job; repeated calls after `ready` return the same artifact; repeated calls after `error` return the same stable error. `status` has the same observational behavior without initiating stop.

A tab cannot start another screencast while its current job is recording or finalizing. After a terminal state, a new start replaces that tab's current-job status while any completed artifact remains available through the artifact registry.

## Acquisition and backpressure

VBL continues to use `Page.startScreencast` for the source tab. The command supplies `maxWidth` and `maxHeight` so Chrome performs source scaling. VBL uses frame metadata timestamps and a monotonic presentation schedule to select frames at the requested rate instead of deriving a stride from an assumed 60 Hz compositor.

Each recording owns a bounded frame channel. The CDP listener acknowledges source frames promptly. When the encoder cannot keep up, the listener drops intermediate frames while preserving monotonic presentation order. Memory use is therefore bounded by configuration rather than recording duration.

Source navigation does not restart the job or create a new artifact. If the CDP target survives, frame acquisition continues across the new document.

## Hidden Chrome recorder

The primary backend creates a broker-private target with `Target.createTarget({ hidden: true, background: true })`. The target hosts a canvas and `MediaRecorder`. It receives bounded JPEG frames, decodes them with `createImageBitmap`, letterboxes them into the configured canvas without distortion, and requests frames from `canvas.captureStream(0)`.

`MediaRecorder` emits timesliced `video/webm;codecs=vp8` chunks through a private CDP binding. The broker appends each chunk to the reserved partial file. No duration-sized frame or video buffer crosses back into Rust.

The recorder target is separate from the source document, so source navigation does not destroy its encoder context. VBL explicitly closes the recorder target when the job terminates. Losing the creating CDP session also closes it by Chrome's hidden-target lifetime contract.

The target and binding are private broker machinery. They do not enter the tab registry, create cleanup provenance, consume a user lease, or appear in VBL's global tab projections. A separate headed macOS probe confirmed that the hidden target has protocol type `other` and does not appear in Chrome's tab strip.

## Stop and artifact publication

The first stop request signals source capture to stop and waits up to five seconds for finalization. If the job becomes ready during that interval, stop returns:

```json
{
  "operation": "stop",
  "recording": false,
  "state": "ready",
  "artifact": { "...": "existing ArtifactSummary" },
  "metrics": {
    "received_frames": 0,
    "encoded_frames": 0,
    "dropped_frames": 0
  }
}
```

If finalization is still active, stop returns `recording: true`, `state: "finalizing"`, and current metrics without an artifact. The broker-owned job continues after the request returns or is cancelled.

The artifact registry exposes only completed immutable artifacts. The reserved partial file and provisional artifact identity remain private. `artifacts list`, `metadata`, `read`, and `export` therefore require no provisional-state extension.

On success, the broker closes the partial file, validates non-empty WebM output, computes size and SHA-256, atomically publishes the existing `ArtifactSummary`, and transitions the job to `ready`.

## Diagnostics

Status exposes only content-free counters and lifecycle timing:

- `started_at_ms`;
- `state`;
- `received_frames`;
- `encoded_frames`;
- `dropped_frames`;
- completed artifact summary in `ready`;
- stable error code and redacted message in `error`.

VBL does not expose queue depth, frame bytes, partial paths, hidden target identity, CDP session identity, codec-process details, conversation identity, or internal session handles.

## Cleanup and failures

Stopping, tab closure, session expiry, target disappearance, Chrome loss, and broker shutdown use one job-owned cleanup path. That path stops source capture when possible, disengages focus emulation, closes the private recorder target, closes the partial file, and either publishes a complete artifact or removes the partial output.

Tab closure, session expiry, and broker shutdown cancel a recording that is still recording or finalizing. VBL does not finish an artifact for an expired or unreachable session. The existing ready artifact follows normal session retention and is removed when its session expires.

If Chrome fails while a recording is active, the job records a stable error, removes partial output, and releases broker state without replaying source actions. A later Chrome recovery starts with no orphan recorder targets or phantom active screencasts.

# Output and compatibility contract

The durable implementation produces silent VP8-in-WebM. WebM remains the public container and `video/webm` remains the declared media type. RFC 00005's AV1 codec detail is superseded; consumers must use the media type rather than assume an unreported codec. The release notes will call out the codec change, but it does not require a parallel versioned tool option.

H.264-in-MP4 is outside this RFC. Browser H.264 availability varies by platform and build, while the six-runner matrix established VP8 support everywhere VBL ships. A later RFC may add an explicit format option if a concrete workflow requires it.

The installed package remains self-contained and adds no media executable. The current rav1e encoder and its RGB-to-I420 batch pipeline are removed when the streaming implementation lands.

RFC 00005 continues to define the `screencast` tool and session-owned artifact surface. This RFC supersedes its requirements that stop synchronously return an AV1 artifact, that media encoding run inside the VBL executable, and that encoding produce an in-memory byte vector.

RFC 00010 continues to define the source tab's frame guarantee and focus-emulation teardown. This RFC extends that stewardship: the broker-owned recording job also owns frame memory, recorder lifetime, partial output, and final publication.

RFCs 00009, 00011, and 00012 remain unchanged. Session TTL, conversation-scoped identity, target cleanup provenance, and explicit preservation do not depend on the video backend.

# Set A: backend feasibility evidence

The authoritative probe ref is `wycats/rfc-0013-probe-matrix` at `30958122aac861898fa685cd43cbe64aeadb47fa`. GitHub Actions run [29215724391](https://github.com/wycats/visible-browser-lab/actions/runs/29215724391) executed the same standalone CDP/MediaRecorder harness on the six native release runners.

Every runner:

- created a hidden recorder target and recorded approximately 46 seconds at 1280×720;
- produced VP8-in-WebM that Chrome loaded at 1280×720;
- continued across a source-target navigation;
- exercised an intentionally slow consumer and dropped frames instead of growing an unbounded queue;
- completed every CDP endpoint health probe successfully;
- removed the hidden target when its creating CDP session closed;
- returned the Chrome process tree below its starting RSS after finalization;
- exited the isolated Chrome process.

| Release target | Chrome | Output | Peak RSS increase | Maximum endpoint probe |
| --- | --- | ---: | ---: | ---: |
| `aarch64-apple-darwin` | 150.0.7871.47 | 2.93 MiB | 152.3 MiB | 39 ms |
| `x86_64-apple-darwin` | 149.0.7827.201 | 1.79 MiB | 326.3 MiB | 48 ms |
| `aarch64-unknown-linux-musl` | 149.0.7827.0 | 3.18 MiB | 181.3 MiB | 10 ms |
| `x86_64-unknown-linux-musl` | 150.0.7871.46 | 3.08 MiB | 157.3 MiB | 41 ms |
| `aarch64-pc-windows-msvc` | 150.0.7871.47 | 2.59 MiB | 126.7 MiB | 13 ms |
| `x86_64-pc-windows-msvc` | 149.0.7827.201 | 2.37 MiB | 286.2 MiB | 14 ms |

The Windows binaries reported native ARM64 and x86_64 PE machine types. Linux ARM64 reported an AArch64 ELF binary; Linux x86_64 executed the installed Chrome on the native x86_64 runner. macOS used a universal Chrome binary on the matching native runners.

The six evidence archives are:

- macOS ARM64: artifact `8266493909`, SHA-256 `217dd3a0d1b33c92d76f718d6f13bcb6f1ae4eb3ad781dafef99c878164d0a34`;
- macOS x86_64: artifact `8266498075`, SHA-256 `453b9fa5f3e885833ffcf2baf972870fbe18bc945c94bc4fe2b96338cc404182`;
- Linux ARM64: artifact `8266497135`, SHA-256 `e1d00136adc7a2a2650f4880b8ac0abd3d7d641e73bc3cf86d2f72f66ff00264`;
- Linux x86_64: artifact `8266493662`, SHA-256 `3c8cb846c3e47c312aaa87f5608696ebc75e588bb97422b4c0b933b24c5f5c54`;
- Windows ARM64: artifact `8266497059`, SHA-256 `4fdde87a515f856024a9a875ffca13f479f10573c0121de756eb6401e81ed7a9`;
- Windows x86_64: artifact `8266495951`, SHA-256 `0b76383884ccabfa79a63c04abd2d5ef36ac6fc39d3da56efc539821345cf203`.

This evidence selects hidden-target MediaRecorder as the proposed primary backend. The minimal FFmpeg/libvpx path remains useful prior art and a benchmark oracle, but it does not ship as a fallback in the initial implementation.

# Set B: implementation acceptance

The backend matrix does not claim to prove VBL product behavior that does not exist yet. The following are implementation acceptance gates:

- caller cancellation during stop does not cancel broker-owned finalization;
- status and repeated stop recover the same ready artifact after caller cancellation;
- only one recording or finalization job exists per target;
- tab closure, target disappearance, session expiry, Chrome loss, and broker shutdown use the same idempotent cleanup path;
- partial output and hidden recorder targets never survive cleanup;
- a ready artifact remains session-owned and follows existing retention;
- schema, catalog, VS Code projection, help, and skill guidance agree on dimensions and lifecycle states;
- packaged VBL binaries pass the six-platform 45-second matrix;
- installed Codex and VS Code can capture, finalize, export, and play a 40–50 second reviewer video without exceeding the host tool timeout;
- process counts and memory return to baseline after two maintenance intervals.

# Drawbacks

The durable lifecycle adds job state where stop previously appeared synchronous. Callers and help text must understand `finalizing`, even though ordinary recordings should usually finish within the five-second stop wait.

A hidden recorder target uses an experimental CDP facility. The six-platform matrix substantially reduces compatibility risk, but Chrome can still change this surface. The encoder also runs inside the managed Chrome process tree whose reliability VBL depends on for interaction.

VP8 replaces the existing AV1 codec detail. This improves realtime portability and follows established browser-automation practice, but it gives up AV1's compression efficiency and may surprise a consumer that inspected codec details rather than the declared `video/webm` media type.

The hidden recorder adds roughly 127–326 MiB of peak Chrome-tree RSS in the feasibility workload. That is bounded and far below the duration-sized batch path, but it is not free.

# Rationale and alternatives

**Optimize the existing rav1e batch.** More threads, SIMD color conversion, and faster resizing would reduce latency, but recording duration would still determine peak memory and stop cost. Caller cancellation would still threaten artifact publication unless the lifecycle were redesigned. These optimizations do not address the controlling failure.

**Run the current batch encode in the background.** This would preserve artifacts across the original RPC timeout, but it would continue consuming duration-sized memory, provide poor completion latency, and defer rather than remove the scaling problem.

**Ship minimal FFmpeg immediately.** Playwright and Puppeteer establish this as strong prior art, and the exact installed Playwright encoder processed an equivalent 45-second stream in 4.249 seconds. Shipping it would add executable provenance, license notices, checksum verification, process teardown, and release artifacts. Playwright also does not publish a native Windows ARM64 build matching VBL's complete target matrix. The hidden backend passed all six native runners without adding a sidecar.

**Use a native Rust libvpx binding.** This avoids a sidecar but retains native library and cross-compilation complexity. VBL would still need optimized JPEG decoding, color conversion, muxing, and streaming lifecycle machinery. Available prebuilt coverage does not clearly match all six release targets.

**Record DOM events with rrweb.** DOM replay is valuable as a semantic diagnostic artifact but is not a faithful replacement for pixels across canvas, video, cross-origin frames, browser-rendered controls, and arbitrary applications.

**Use an extension offscreen document and `tabCapture`.** This could capture a native tab media stream without CDP JPEG transport and deserves future investigation. It depends on extension permissions and user-invocation semantics, so it does not cover generic MCP clients and cannot be the common backend.

# Remaining implementation choices

Stage 1 should leave only internal tuning choices open:

- the exact bounded queue capacity and which intermediate frame to retain when full;
- the mapping from `quality` to source JPEG quality and MediaRecorder bitrate;
- the internal finalization-task structure and synchronization primitive;
- how long terminal job status remains cached after its artifact is deleted.

These choices must preserve the public lifecycle, bounded-memory behavior, redacted diagnostics, and cleanup contract above.
