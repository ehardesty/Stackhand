# Milestone 2 macOS validation evidence

## Scope and recommendation

This record validates the Milestone 2 prototype on one macOS arm64 host. It
covers the integrated synthetic Project, a small real Project, repeated
shutdown, resource convergence, and a focused terminal regression.

**Recommendation: GO to Milestone 3 prototype work on macOS.**

This is not a release decision. It is not a Linux, Windows, security, or
process-containment claim.

## Stable starting revision

The stable starting revision for issue #53 is:

```text
652c17969f27fd745e9e1d8b8a00c95e736cb5d5
```

This revision is the completed Milestone 2 integrated lifecycle fixture from
issue #52. The input UX work was complete before this validation started. The
terminal and input regression remains covered by
`tests/fixture_round_trip.rs`; issue #52 completed that work before this
revision was selected. The worktree was clean when the revision was selected.

## Host and toolchain

- Validation time: 2026-08-28T15:23:18Z
- Operating system: macOS 26.6.2, build 25G83
- Kernel: Darwin 25.6.0
- Architecture: arm64
- Rust: `rustc 1.93.0 (254b59607 2026-01-19)`
- Cargo: `cargo 1.93.0 (083ac5135 2025-12-15)`
- Zig: `0.15.2`, selected with `brew --prefix zig@0.15`

## Checks and results

All commands used the pinned Zig path:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo fmt --all -- --check
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo clippy --locked --all-targets --all-features -- -D warnings
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo build --locked --all-targets
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo test --locked --all-targets -- --test-threads=1
```

Results:

- Formatting: pass.
- Clippy for all targets with warnings denied: pass.
- All targets build: pass.
- Full automated suite: pass.

The full suite reported:

| Test target | Passed |
| --- | ---: |
| `src/lib.rs` | 368 |
| `src/main.rs` | 0 |
| `fixture_round_trip` | 6 |
| `input_backlog` | 1 |
| `interaction_fixture` | 1 |
| `output_pressure` | 1 |
| `project_fixture` | 4 |
| `real_project_smoke` | 7 |
| `run_convergence` | 2 |
| `sustained_output` | 2 |
| **Total** | **392** |

No test failed or was ignored in this run.

## Integrated synthetic Project

`project_fixture` passed all four tests. Its main test loaded one production
YAML Project and exercised:

- startup Dependencies and visible blocked reasons;
- TCP, HTTP, exec, log, and composite `all` readiness;
- readiness loss and in-place recovery;
- liveness failure and recovery;
- unhealthy restart and the Automatic Restart Budget;
- failed One-shot rerun and Dependency recovery;
- retained pipe and PTY output;
- startup timeout Process Tree cleanup;
- controlled Project shutdown, including suppression of a pending restart.

The fixture uses real local TCP and HTTP endpoints and bounded waits.

## Small real Project

`real_project_smoke::stackhand_repository_runs_as_a_small_real_project` passed
three focused runs and passed in the full suite. It generated and loaded a
production YAML Project through `--fixture-smoke`. The Project used:

- a direct `cargo metadata --no-deps --format-version 1` One-shot from the
  repository directory;
- a shell Service with an ordinary `sleep` descendant and retained PID output;
- a Service with an HTTP `ready` check against a real loopback endpoint;
- a Service that waited for the readiness Dependency.

The test changed the real HTTP endpoint from 200 to 503 and back through the
fixture checkpoint stream. Each cycle proved:

- readiness passed before the dependent Service started;
- readiness loss stayed visible with the 503 diagnostic;
- the Service recovered in the same Run;
- the already-running dependent kept its Run;
- retained output stayed within the configured memory bound;
- Project shutdown completed without a timeout or cleanup failure;
- every Process had no current Run after shutdown;
- the ordinary descendant returned `ESRCH` from `kill(pid, 0)` after shutdown;
- the Project output owner was released.

The fixture completed three full start and shutdown cycles in one fixture
process. The three cycles run inside one spawned `stackhand --fixture-smoke`
fixture process. The parent integration test owns the endpoint workers and
consumes the checkpoint stream. This is the executable-process reading of the
issue's "one test process" criterion; the parent Cargo test process does not
execute all three Supervisor cycles itself.

Its emitted evidence was:

```text
real-project-cycle-1-cleanup-ok
real-project-cycle-2-cleanup-ok
real-project-cycle-3-cleanup-ok
real-project-resources-ok: fds 4 -> 4; threads 2 -> 2; tolerance 2
real-project-cycles-ok: 3
real-project-smoke-ok
```

The resource rule is `final <= baseline + 2` for both file descriptors and
threads. The endpoint worker is owned by the test and joined by its drop path.
Supervisor shutdown reports confirmed cleanup when its failure list is empty.

## Focused macOS terminal regression

The focused executable fixture target passed all six tests:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --test fixture_round_trip -- --test-threads=1
```

The target uses a real `/bin/sh` child in a real PTY. It covers input round
trip, terminal rendering, encoded input and terminal responses, paste,
scrolling while unfocused, and mouse ownership. This confirms that the
Milestone 1 terminal and input path remains usable on the validation host.

The earlier manual macOS program matrix remains in
[terminal prototype validation](terminal-prototype-validation.md). This issue
did not repeat physical keyboard, IME, non-US layout, or manual editor checks.

## Configuration and user instructions

The audit covered `README.md`, `examples/README.md`, and the five checked-in
YAML examples: `basic.yaml`, `dependencies.yaml`, `failures.yaml`,
`output-pressure.yaml`, and `readiness.yaml`. The configuration test
`src/config/tests.rs::checked_in_example_projects_load` loads every checked-in
YAML example and passed.

The examples use direct `program` and `args` commands. The integrated fixtures
also exercise the explicit `command.shell` form. The instructions explain the
user-visible rules:

- readiness uses `ready`;
- liveness uses `liveness`;
- durations use readable values such as `20ms` and `5s`;
- direct commands use `program` and `args`;
- shell expressions use the explicit `shell` form;
- the default shell-expression runner remains `/bin/sh -c` and does not use
  the user's `$SHELL`.

The README now describes the Milestone 2 prototype and links to this record.
No change to the accepted terminal ownership model was needed.

## Observed limits

- This evidence is for macOS 26.6.2 arm64 only.
- Linux implementation and validation are deferred. Earlier Linux evidence
  does not become current Milestone 2 evidence.
- Windows remains out of scope.
- Process Tree containment is best effort. A Process that creates a new
  session or otherwise escapes its owned group can remain outside cleanup.
  Stackhand is not a security boundary.
- Automated terminal input uses PTY fixture bytes. Physical keyboard,
  non-US layout, IME, and every outer-terminal capability combination remain
  unverified. Alt-character behavior remains partial as recorded in the
  terminal prototype evidence.
- Resource checks measure the fixture process's thread and file-descriptor
  counts. They do not prove total host resource stability or allocator memory.
- The loopback endpoint and test-owned workers are not a remote or hostile
  deployment test.

The evidence supports continued macOS prototype work. It does not establish a
release platform list or infer completeness on Linux or Windows.
