# Milestone 1 integrated vertical slice — validation evidence

Status: recorded 2026-08-27 for issue #35 and parent issue #20.

## Recommendation

**GO to Milestone 2 for the macOS prototype.**

Milestone 1 has one usable integrated Supervisor slice on macOS arm64. The
recommendation is not a release decision and does not make a Linux, Windows,
or security-containment claim.

## Host and toolchain

- macOS 26.6.2, build 25G83
- Darwin 25.6.0, arm64
- Rust 1.93.0
- Cargo 1.93.0
- Zig 0.16.0 was present on the validation host. This pass used the existing
  contributor build state and did not repeat a clean offline native build.
- Validation time: 2026-08-27T16:01:28Z
- Starting commit for issue #35: `f689c442875949c7d9b4f42d2a303c5ca64f5562`

## Exact commands and results

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --all-targets
cargo test
```

All commands passed. `cargo test` passed:

- 209 library tests;
- 15 integration tests across the executable terminal matrix, integrated
  interaction fixture, output-pressure fixture, synthetic Project fixture,
  small real-Project smoke fixture, and sustained-output fixtures;
- all document tests.

The executable fixtures use fixed internal deadlines. The full suite completed
without an unbounded wait.

## Integrated synthetic Project

`tests/project_fixture.rs` creates one YAML version 1 Project with:

- multiple Services and one successful One-shot;
- enabled, disabled, autostart, and optional Processes;
- direct and shell commands;
- pipe and PTY terminal modes;
- focused and disabled input;
- `started`, `ready`, and `completed_successfully` Dependencies;
- real local TCP and HTTP readiness endpoints;
- a pipe Process with an ordinary descendant.

The fixture first observes exact Waiting reasons for the delayed graph edges:
`hello: started`, `setup: completed_successfully`, and `http-ready: ready`. It then verifies that
each dependent starts no earlier than its required Dependency condition. It
checks retained stdout and stderr identity, direct-command and shell-pipeline
output, inline environment, One-shot completion, TCP and HTTP readiness, and
bounded controlled Project shutdown. After successful shutdown, `kill(pid, 0)`
returns `ESRCH` for the recorded ordinary descendant PID.

## TUI and lifecycle interaction

`tests/interaction_fixture.rs` drives the production pane key seam. It proves:

- Process selection across PTY and pipe panes;
- selected PTY input and rejection for input-disabled Processes;
- output inspection, scroll/follow, resize, and continued ingestion;
- Service stop, start, and restart;
- One-shot start and rerun with retained Run markers;
- PID, age, CPU, memory, and narrow-layout metric degradation;
- controlled Project shutdown after the interaction proof.

The terminal executable matrix also covers input encoding, paste, rendering,
mouse ownership, selection, copy behavior, and output drain while unfocused.

## Output pressure and repeated cycles

The output-pressure fixture runs several concurrent producers and verifies that
retained output stays bounded while lifecycle commands continue to flow. The
runtime stress suite verifies repeated Run cleanup and thread/file-descriptor
convergence.

`tests/real_project_smoke.rs` uses this Stackhand repository as a small real
Project. A configured One-shot runs `cargo metadata --no-deps --format-version
1` from the repository root. A Service waits for its successful completion.
The fixture performs three complete Project start/shutdown cycles in one
process, checks retained output against the configured memory bound, verifies
that each Project output owner is released after shutdown, and checks that
thread and file-descriptor counts converge to within two of their starting
values. Each clean shutdown result also confirms that the Run workers joined.

## Linux evidence is separate

No current Milestone 1 Linux run was performed. No Linux completeness claim is
made.

The earlier [Run ownership evidence](run-ownership-evidence.md) records
historical Ubuntu 24.04 x86-64 container results. Pipe-mode Run seams,
Process Tree signaling, bounded shutdown, metrics, outcomes, and repeated-cycle
convergence passed in that earlier suite. Those results predate later
corrections and are not reused as current completion evidence.

That same earlier evidence records three Linux PTY limitations:

1. F12 encoded differently in the input fixture.
2. Container PTY drain throughput was too slow for the scrollback bound.
3. The paste fixture hung.

Bare-metal Linux was not tested. These gaps must be reproduced and fixed before
Stackhand makes a Linux interactive-PTY support claim.

## Observed limits

- Process-group containment is best effort. A deliberately detached Process
  can escape. Stackhand is not a security boundary.
- This pass did not repeat physical keyboard, IME, non-US layout, or manual
  shell/pager/editor checks. The earlier terminal prototype validation records
  the manual macOS program matrix; this pass reran the automated interaction
  and executable fixtures.
- Ghostty scrollback has a requested memory target, not an exact visible line
  count. Exact scrollback use and a Ghostty truncation event remain unavailable.
- Metrics are aggregate best-effort samples and can be absent before the first
  sample or during rapid Process exit.
- Windows remains out of scope.

## Milestone 2 entry

Proceed with lifecycle hardening: the full transition table, startup timeout,
liveness, restart policy and budget, richer readiness, stale-event race cases,
and explicit dependency recovery. Keep the accepted UI/terminal boundary and
current-state Dependency rule unchanged.
