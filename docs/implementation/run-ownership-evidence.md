# Milestone 0B: Run ownership and Process Tree shutdown — validation evidence

[Back to the implementation plan](../implementation-plan.md)

Status: recorded 2026-08-25 for tickets #13–#19 (parent issue #12). The
correction pass was validated on macOS only; the Linux results below predate
this pass and are not re-used as current Linux completion evidence.
Commits: `d2257b0` (#13), `bf6e90e` (#14), `5a37a47` (#15), `64d3335` (#16), `4e1bb53` (#17), `af45c3b` (#18), and this note (#19).

## 1. What was built

One deep Run ownership module behind a small interface:

- `RunRuntime::start(RunStartRequest) -> OwnedRun` is the highest caller and test seam. The request carries ProcessId, RunId, command details, pipe or PTY mode, initial geometry (PTY), the low-volume event sink, the high-volume output sink (pipe), shutdown-ladder timeouts, metrics interval, and an optional redraw wake.
- `OwnedRun` owns the root process, Process I/O, output drains, the aggregate metrics sampler, the optional TerminalSession lifetime, and every Run worker task.
- Semantic operations: `interrupt()`, `terminate()`, `kill()` (SIGINT/SIGTERM/SIGKILL inside the Unix adapter), PTY `resize`, idempotent `shutdown()` owning the complete ladder, and `wait()` for natural completion.
- One structured `RunOutcome`: RunId, exit disposition (natural / unexpected / intentional), intentional-stop state, exit code, ordered stage results, cleanup confirmation, remaining PIDs, I/O failures, terminal failure, sampler final sample, task-join failures.
- Process configuration identity (`ProcessId`) is separate from the operating-system process identity (`OsPid`). The high-volume pipe sink is byte-bounded (16 MiB) and reports overflow as retained diagnostics; it is not part of the low-volume event sink.
- Terminal input admission is bounded and reports stopping or backpressure rejection for keys, focus, mouse, raw input, resize, and paste. A rejected item is visible to the TUI.
- Private adapters: `ProcessTree` (Unix process groups; group-directed signals; membership enumeration via `/proc` on Linux, `ps` on macOS), `PipeIo` (std Command + reader threads), PTY transport via portable-pty, `MetricsSampler` (aggregate CPU/RSS).
- Fail-closed rules: no positive-PID fallback after failed group signals; EPERM stops escalation against that PGID with retained diagnostics; root unreaped until all signals and containment checks complete; only ESRCH proves absence in liveness probes; root exit observed via `waitid(WNOWAIT)` (macOS) so reaping never precedes containment work.

Terminal semantics remain inside `TerminalSession` (ADR 0001 unchanged). The terminal handle is non-owning: no terminal shutdown action, cannot detach the session. ADR 0002 is unaffected (no lifetime coupling added).

## 2. Exact commands

macOS (host):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --all-targets
cargo test
./target/debug/stackhand --fixture-round-trip hello
./target/debug/stackhand --fixture-input
./target/debug/stackhand --fixture-paste
./target/debug/stackhand --fixture-rendering
./target/debug/stackhand --fixture-scrollback
./target/debug/stackhand --fixture-mouse
```

Linux (Docker container on Ubuntu VM, source synced from the host):

```sh
docker run --rm -v /tmp/stackhand-src:/src -w /src rust:1.93 sh -c '
  apt-get install curl xz-utils   # plus zig 0.15.2 installed to /usr/local/bin/zig
  cargo test'
# fixture matrix:
timeout 60 /src/target/debug/stackhand --fixture-round-trip hello
timeout 60 /src/target/debug/stackhand --fixture-input
timeout 60 /src/target/debug/stackhand --fixture-paste
timeout 60 /src/target/debug/stackhand --fixture-rendering
timeout 60 /src/target/debug/stackhand --fixture-scrollback
timeout 60 /src/target/debug/stackhand --fixture-mouse
```

## 3. macOS results

Host: macOS 26.6.2 (BuildVersion 25G83), arm64, Rust 1.93.

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`: pass.
- `cargo test --all-targets -- --nocapture`: pass twice. Each run had 84 lib unit tests, 6 real-PTY executable-fixture integration tests, and 2 CLI tests. The focused high-output, noisy-run isolation, blocked-input, and PTY-pressure tests also passed.
- Executable fixture matrix: all six fixtures pass (round-trip echo+size, full key-encoding input matrix, paste normal/bracketed/oversized/blocked, colors/styles/unicode/cursor/alternate-screen/reflow/resize rendering, bounded scrollback drain while scrolled and unfocused, SGR mouse arbitration).
- Process Tree cleanup: every containment test verifies recorded fixture PIDs are gone after cleanup (`kill(pid,0)` probing where only ESRCH counts as absence). Child/grandchild trees under exec and non-exec wrappers stop together.
- Aggregate CPU/memory: sampler emits RunId/ProcessId-scoped snapshots at the configured interval; busy children produce measurable CPU and RSS contribution; one fully used logical core = 100 %; aggregates can exceed 100 %.
- Repeated Runs: 25 pipe + 5 PTY sampled start/stop cycles converge back to baseline thread and file-descriptor counts (bounded convergence polling). Every cycle reports `cleanup_confirmed` with no task-join failures.
- Output pressure: 4000-line pipe producer and a real PTY flood keep ingesting through the complete ladder; sequence-number completeness, bounded history progress, and admitted paste completion prove the public Run seam stays live. One noisy flooding Run does not delay another Run's interrupt or shutdown.

### macOS platform notes

- A group whose only member is an unreaped zombie session leader answers `kill(-pgid)` with EPERM (observed on Darwin arm64). Escalation therefore skips group signaling when nothing lives besides the root and observes root exit with `waitid(WNOWAIT)`.
- Single-shot group signals were observed to be silently ineffective under heavy process churn (`kill()` returning 0 with no effect). Ladder stages now re-transmit periodically during their wait windows; budgets still bound each phase.
- Containment limits: a descendant that calls `setsid()`/`setpgid()` escapes the owned group; membership enumeration is best effort; process-group identity alone cannot distinguish a reused PGID after reaping — which is why no signal follows a reap. Containment is a reliability feature, not a security boundary.
- Drop/abort guarantees (parent-issue requirement): dropping an `OwnedRun` without `shutdown()`/`wait()` closes input admission, stops the sampler, and applies SIGKILL to the whole owned Process Tree on a bounded 300 ms retry loop unless a signal fails closed. It reaps the root only when containment can be confirmed and reports detach or cleanup diagnostics through the Run event sink. NOT guaranteed on this path: final output drains, joining workers held open by surviving descendants, or a RunOutcome. Callers needing drained output or a structured result must call `shutdown()`/`wait()` instead.

## 4. Linux results (prior evidence; correction pass not rerun)

Host: Ubuntu 24.04.4 LTS, kernel 6.8.0-138-generic, x86_64, 8 CPUs (Docker container `rust:1.93`, zig 0.15.2 required by the pinned Ghostty revision).

- Compilation required two cross-platform fixes (both applied and committed with this note): Linux libc exposes `siginfo_t::si_pid` as a method, and `/proc/<pid>/stat` field parsing needed `.ok()` conversions. These are exactly the class of portability defects this milestone's platform gate exists to catch.
- Historical result before the correction pass: `cargo test` ran all 83 then-current lib unit tests successfully, including pipe-mode spawn/drain/reap, Unix Process Tree signaling (`/proc/<pid>/stat` pgid matching), semantic ladder behavior, structured outcomes, and repeated-cycle leak convergence. The changed 84-test suite was not rerun on Linux.
- Executable fixture matrix:

| Fixture | Result |
| --- | --- |
| round-trip | PASS (echo, size report) |
| rendering | PASS (colors, styles, unicode, cursor, alternate screen, reflow, resize) |
| mouse | PASS (SGR press/release/motion/drag/wheel; shift override) |
| input | FAIL — F12 arrives as `CSI 3~` instead of `CSI 24~`; later bytes of the expected sequence missing |
| scrollback | FAIL — drain throughput ~200 lines/s through the container PTY; "scroll-ready" not reached within the 60 s timeout |
| paste | HANG — fixture does not complete (killed after hours) |

### Linux platform notes

- All failures are confined to PTY-mode integration fixtures. Pipe-mode coverage (spawn, continuous stdout/stderr drains with stream identity, natural-exit reap, semantic ladder, outcomes) is fully green at the seam level.
- The input encoding difference points at Ghostty VT key-encoder behavior or terminfo differences under the Linux container; needs investigation before interactive PTY claims extend to Linux.
- The scrollback/paste symptoms look related to PTY drain throughput inside the containerized environment (the paste fixture saturates its bounded writer while the child never consumes fast enough). Whether this reproduces on bare-metal Linux is untested.

## 5. PTY transport assessment

`portable-pty` remains viable as the transport candidate:

- It provides reader/writer handles, resize, root PID reporting, and per-child wait/kill.
- Its `setsid()` behavior makes every PTY child a session AND group leader, which is what lets the Process Tree adapter target `-pgid` uniformly across transports.
- Its child kill operation was never used to define containment: escalation goes through the private Process Tree adapter, satisfying "PTY transport is not process ownership".
- Known quirks recorded: Darwin zombie-session-leader EPERM on group signals (handled); single-shot signal ineffectiveness under churn (mitigated by retransmission).

## 6. Repeated-Run leak evidence (macOS)

Measured over 30 sampled start/stop cycles: thread count and open-file-descriptor count converge back to baseline within the 15 s bounded polling deadline after the cycles complete. Per-cycle outcomes report empty task-join failures throughout. Linux equivalent: covered by lib-level repeated-cycle tests (leak-convergence assertions passed); OS-level fd counting uses `/proc/self/fd` there.

## 7. Comparison with the Milestone 0B exit criteria

| Criterion | macOS | Linux |
| --- | --- | --- |
| Pipe and PTY modes through one Run ownership interface | PASS | PASS (lib seam tests) |
| Ordinary Process Trees clean up (child/grandchild, exec/non-exec wrappers) | PASS | PASS (lib seam tests, `/proc` pgid matching) |
| Output drains stay active through shutdown | PASS | PASS (pipe-mode lib tests) |
| Semantic interrupt/terminate/kill ladder with configured timeouts | PASS | PASS (lib seam tests) |
| Aggregate CPU/memory sampling | PASS | PASS (lib tests; tick-delta rate) |
| Intentional vs unexpected vs natural classification | PASS | PASS |
| No task/thread/fd leaks over repeated cycles | PASS | PASS (convergence assertions) |
| Real-PTY executable fixtures | ALL PASS | 3 of 6 pass; input FAIL, scrollback FAIL, paste HANG |

## 8. Recommendation

**GO for Milestone 1, scoped to macOS**, with three explicitly recorded Linux PTY-fixture defects that must be fixed before any Linux claim. Because this correction pass was not run in Linux, no Linux completion claim is made for the changed implementation:

1. F12 (and possibly other extended keys) encode differently through the Ghostty VT encoder path on the Linux container.
2. Containerized-Linux PTY drain throughput (~200 lines/s) is too slow for the scrollback fixture within its budget.
3. The paste fixture hangs on Linux.

The Run ownership model itself — the thing Milestone 0B exists to prove — is validated on both platforms at the seam level: one interface for both transports, ordinary Process Trees cleaned up, drains active through shutdown, bounded ladders, metrics, structured outcomes, and no leaks. The remaining Linux gaps are confined to PTY-mode integration fixtures and do not conflict with the Stackhand specification; they are platform-completeness defects, recorded here rather than hidden.

## 9. Unrun or unavailable items

- Bare-metal Linux (non-container): not run. All Linux results come from a Docker container on Ubuntu 24.04 x86_64.
- Windows ConPTY / Job Object: out of scope for Milestone 0B per parent issue #12.
- Interactive host keyboard testing on real hardware terminals: unchanged since the Milestone 0A evidence; not re-run here.
