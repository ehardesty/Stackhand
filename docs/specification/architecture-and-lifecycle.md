# Architecture and lifecycle

[Back to the product specification](../product-specification.md)

## 7. High-level architecture

```text
                                  control plane

┌──────────────────────┐   commands      ┌──────────────────────────────┐
│      Ratatui UI      │ ───────────────►│       Supervisor Core        │
│                      │◄───────────────  │                              │
│ list / modes / panes │ state snapshots │ desired state                │
└──────────┬───────────┘                  │ dependency graph             │
           │                              │ lifecycle transitions        │
           │ selected render/log request  │ probes / restarts / hooks    │
           ▼                              │ metrics metadata             │
┌──────────────────────┐                  └──────────────┬───────────────┘
│ Output View Gateway  │                                 │ typed control events
└──────────┬───────────┘                                 ▼
           │                              ┌──────────────────────────────┐
           │                              │      Process Runtimes       │
           │                              │ spawn / process tree / exit  │
           │                              │ pipes / PTY / shutdown       │
           │                              └──────────────┬───────────────┘
           │                                             │ bytes
           │                                             ▼
           │                              ┌──────────────────────────────┐
           └─────────────────────────────►│   Per-process Output Owner  │
                                          │                              │
                                          │ Ghostty terminal             │
                                          │ bounded log history          │
                                          │ line normalization/search    │
                                          │ render snapshots             │
                                          └──────────────────────────────┘

                                   data plane
```

### 7.1 Required ownership boundaries

#### Supervisor core

The supervisor task owns authoritative mutable project state:

- enabled/disabled processes after config merge;
- desired running/stopped intent;
- lifecycle state;
- dependency satisfaction;
- current `RunId`;
- restart counters and timers;
- readiness and liveness status;
- last exit/failure diagnostics;
- current process metadata;
- scheduling decisions.

No other component may mutate this state directly.

#### Process runtime

A process runtime owns the current process attempt:

- spawn operation;
- root process handle;
- process-group or job ownership;
- stdout/stderr or PTY handles;
- exit observation;
- interrupt/terminate/kill escalation;
- child-tree cleanup.

#### Process output owner

A per-process output owner owns:

- current run terminal engine;
- serialized Ghostty mutation;
- PTY write-back effects;
- bounded retained log history;
- logical-line assembly;
- search state/indexes;
- current selection state;
- render snapshot generation;
- coalesced dirty notifications.

The output owner spans multiple Runs for retained Logs history, but it MUST create a fresh terminal session for each new Run.

#### TUI/application state

The UI task owns:

- selected process;
- focus and input mode;
- active pane/view;
- search query and cursor;
- list scroll position;
- zoom state;
- help/modals;
- transient user notifications.

### 7.2 Control-plane events

The supervisor receives typed events such as:

- spawn succeeded/failed;
- process exited;
- shutdown stage completed/timed out;
- readiness changed;
- health changed;
- hook started/finished;
- restart timer elapsed;
- metrics updated;
- Process output owner failed;
- user command.

It MUST NOT receive every `OutputChunk`.

### 7.3 Output-plane notifications

The output owner SHOULD emit coalesced metadata notifications such as:

```rust
OutputDirty {
    process_id,
    run_id,
    generation,
    new_bytes,
    last_output_at,
}
```

The UI can request an owned terminal render snapshot or log page by generation. Multiple output writes SHOULD collapse into one redraw notification.

### 7.4 Future daemon compatibility

The UI communicates with the supervisor through commands and immutable snapshots rather than direct shared-state access. This preserves a path to a future daemon/attach protocol, but no daemon or persistence layer is required by this specification.

---

## 8. Core process model

### 8.1 Process kinds

The core supports exactly two process kinds.

#### `service`

A long-running process expected to remain alive until stopped.

Examples:

- API server;
- Vite dev server;
- worker;
- Docker Compose foreground command;
- SSH tunnel defined in a local overlay;
- emulator wrapper;
- interactive shell.

#### `oneshot`

A bounded command expected to exit.

Examples:

- storage initialization;
- database migration;
- schema generation;
- smoke check;
- prerequisite validation.

New process kinds MUST NOT be added until concrete use cases cannot be represented cleanly by these two kinds plus dependencies, probes, hooks, and ordinary scripts.

### 8.2 Enabled state, desired state, and observed state

Configuration enablement, user intent, and observed lifecycle are distinct.

Illustrative model:

```rust
enum DesiredState {
    Stopped,
    Running,
}

enum LifecycleState {
    Stopped,
    Blocked,
    Starting,
    Running,
    Stopping,
    RestartBackoff,
    Completed,
    Failed,
}

struct ProcessState {
    enabled: bool,
    desired: DesiredState,
    lifecycle: LifecycleState,
    current_run: Option<RunId>,
    // readiness, health, metrics, diagnostics...
}
```

A disabled process is represented by `enabled == false`; it does not need to overload the lifecycle enum. The UI may project this as `DISABLED`.

Both `enabled` and `autostart` default to `true`. After the complete configuration is merged and validated, each enabled autostart Process becomes desired running and schedules its enabled Dependencies. An invalid Project starts no Processes.

### 8.3 Run identity

Every process attempt receives a monotonically increasing per-process `RunId`:

```rust
struct RunId(u64);
```

The identifier changes for:

- initial start;
- automatic restart;
- manual restart;
- manual oneshot rerun.

Every asynchronous result tied to an attempt MUST carry the `RunId`, including:

- spawn completion;
- exit;
- probe result;
- startup timeout;
- restart timer;
- hook result;
- metrics sample;
- output/session event;
- shutdown result.

The supervisor MUST ignore an event whose `RunId` is not the process's current run. This is a core invariant, not an optimization.

### 8.4 Readiness and health state

Readiness and liveness use a shared check-state vocabulary:

```rust
enum CheckState {
    NotConfigured,
    Inactive,
    Pending,
    Passing,
    Failing,
}
```

Interpretation:

- `NotConfigured`: no check exists.
- `Inactive`: configured but process is not in a phase where the check runs.
- `Pending`: running, but the success/failure threshold has not been reached.
- `Passing`: threshold satisfied.
- `Failing`: failure threshold satisfied.

Effective readiness is defined as:

- a running service with no readiness probe is ready immediately after successful spawn;
- a service with a readiness probe is ready only while readiness is `Passing`;
- a oneshot is not considered ready unless a future explicit use case adds that behavior.

Health/liveness starts only after effective readiness is first reached for the run. If no readiness probe is configured, liveness may start immediately after spawn.

### 8.5 Completion and failure

For a oneshot:

- success exit code -> `Completed`;
- unsuccessful exit, spawn failure, blocking hook failure, or timeout -> `Failed`.

A successfully completed oneshot remains dependency-satisfied for the project session until it is explicitly rerun. Starting a rerun immediately invalidates the previous completion condition until the new run completes.

For a service:

- manual or project-requested stop -> `Stopped`;
- unexpected unsuccessful exit with no restart remaining -> `Failed`;
- unexpected successful exit with no `always` restart -> `Stopped` with a visible `unexpected exit 0` diagnostic;
- a run waiting to restart -> `RestartBackoff`.

An unexpected successful service exit leaves Desired State as `Running`, but it does not trigger `on_failure` or start another Run. Only the `always` policy or a new user command starts it again.

### 8.6 Structured diagnostics

Failures and blocking MUST use structured values rather than only formatted strings.

Examples:

```rust
enum BlockedReason {
    DependencyUnsatisfied {
        dependency: ProcessId,
        waiting_for: DependencyCondition,
        actual: DependencyActualState,
    },
    DependencyFailed {
        dependency: ProcessId,
        failure: FailureSummary,
    },
    DependencyDisabled {
        dependency: ProcessId,
    },
}

enum FailureReason {
    BeforeStartHookFailed,
    SpawnFailed,
    StartupTimedOut,
    ProcessExited,
    LivenessFailed,
    ShutdownFailed,
    ProcessOutputFailed,
    InternalError,
}
```

A process's own `before_start` failure makes that process `Failed`; it does not make the process itself `Blocked`. Its dependents may then become blocked by the failed dependency.

### 8.7 Run history

The supervisor SHOULD retain a small bounded metadata history per process:

- `RunId`;
- start/end timestamps;
- exit status;
- failure reason;
- restart cause;
- whether stop was intentional.

This metadata is separate from retained output and need not become a persistent audit log.

---

## 9. Normative lifecycle behavior

### 9.1 Start scheduling

When a user starts a process or an autostart process is scheduled:

1. Validate that the process is enabled.
2. Set the process and each required enabled dependency to desired running.
3. Detect already-failed or disabled dependencies and expose a blocked reason.
4. Wait until all dependency conditions are satisfied.
5. Allocate a new `RunId`.
6. Run `before_start` hooks in configured order.
7. Create a fresh terminal session for the Run.
8. Spawn the command without losing early output.
9. Begin readiness evaluation or mark the service effectively ready when no readiness probe exists.
10. Run `after_start` hooks independently and best-effort.
11. Satisfy waiting dependencies on the first readiness success, then run `after_ready` independently and best-effort.
12. Start liveness after first effective readiness.

### 9.2 Before-start failure

A failed or timed-out `before_start` hook:

- prevents spawn;
- ends the current attempt as failed;
- records hook output and diagnostics;
- may trigger restart policy as a failed attempt;
- causes dependents to remain blocked.

If the user stops the Process while `before_start` is running, Stackhand cancels the hook and records an intentional cancellation. The Process becomes `Stopped`; it does not run stop hooks, invoke `on_failure`, or consume the Automatic Restart Budget because no supervised process was spawned.

### 9.3 Spawn success without readiness configuration

A service with no readiness probe transitions to `Running` immediately after successful spawn. The dependency condition `ready` is satisfied at that point.

### 9.4 Spawn success with readiness configuration

A service remains `Starting` while its process is alive but readiness has not passed. Once the success threshold is reached, it transitions to `Running` and readiness becomes `Passing`.

### 9.5 Startup timeout

When `startup_timeout` expires before first readiness success:

1. Mark the attempt as startup timed out.
2. Cancel readiness and liveness work for the run.
3. Stop the owned process tree using normal shutdown escalation.
4. Transition the attempt to failed after cleanup.
5. Apply restart policy.

A startup timeout is not merely a UI warning while the process continues indefinitely.

### 9.6 Readiness loss after startup

After first readiness success, later readiness failures:

- set readiness to `Failing` after the configured threshold;
- keep lifecycle `Running` while the process remains alive;
- do not automatically stop already-running dependents;
- may later recover to `Passing`;
- do not rerun `after_ready` for the same `RunId`.

### 9.7 Liveness failure

When liveness reaches its failure threshold:

- health becomes `Failing`;
- the process is visibly unhealthy;
- if `restart.on_unhealthy` is true, the supervisor performs a controlled restart;
- otherwise the process remains running and unhealthy.

### 9.8 Dependency recovery

A blocked desired-running process is reevaluated whenever relevant dependency state changes.

If a failed dependency is rerun and later satisfies the required condition, the blocked dependent automatically becomes eligible to start. The user does not need to issue another start command.

### 9.9 Default runtime dependency behavior does not cascade

Dependencies gate startup. Once a dependent has started, a dependency becoming unready, unhealthy, stopped, or failed MUST NOT automatically stop or restart that dependent by default.

A future explicit lifetime-coupling policy may add this behavior. It MUST NOT be implicit.

### 9.10 Manual stop and project stop

A manual stop or project shutdown:

- records intentional stop intent for the active `RunId`;
- sets a blocked or waiting Process to desired stopped and cancels its pending start;
- cancels pending restart backoff;
- suppresses automatic restart for the resulting exit;
- cancels probes and timers;
- performs bounded hooks and shutdown escalation;
- ends in `Stopped` after cleanup.

Stopping a waiting Process does not stop Dependencies that were already scheduled for it. They can serve other Processes and now have their own desired-running state.

### 9.11 Manual restart

Manual restart:

- cancels any pending automatic restart timer;
- stops the active run intentionally;
- resets the Automatic Restart Budget;
- starts a new `RunId` after cleanup.

### 9.12 Stale events

Any late probe result, hook completion, timer, exit notification, metrics sample, or terminal callback from an older run MUST be discarded without changing current state.

---
### 9.13 Readiness and liveness probes

#### 9.13.1 Probe kinds

The core probe model supports:

- `http`;
- `tcp`;
- `exec`;
- `log`;
- composite `all`.

Composite `any` is optional and is not required by this specification.

#### 9.13.2 Common probe scheduling

A probe supports:

```yaml
initial_delay: 1s
interval: 2s
timeout: 5s
success_threshold: 1
failure_threshold: 3
```

Rules:

1. At most one attempt for a particular probe is in flight at a time.
2. The next interval begins after the previous attempt completes; attempts do not overlap.
3. Probe work is canceled when the run stops, restarts, or is superseded.
4. Every result includes the current `RunId`.
5. Invalid zero/negative intervals, thresholds, and timeouts are rejected during configuration validation.
6. Probe diagnostics are bounded and retained separately from full process output.
7. Threshold counters reset appropriately after the opposite result is observed.

#### 9.13.3 Readiness example

```yaml
ready:
  http:
    url: http://127.0.0.1:5300/health
  initial_delay: 1s
  interval: 2s
  timeout: 5s
  success_threshold: 1
  failure_threshold: 3
  startup_timeout: 10m
```

`startup_timeout` applies only to reaching readiness for the first time. It is distinct from the timeout of one HTTP/TCP/exec attempt.

#### 9.13.4 HTTP probe semantics

Defaults:

- method: `GET`;
- success: any `2xx` response;
- redirects: not followed unless explicitly enabled;
- response body: not required for success and capped to a small diagnostic limit;
- system trust store for TLS;
- connection, request, and body-read work are bounded by the configured timeout.

Illustrative configuration:

```yaml
ready:
  http:
    url: https://127.0.0.1:5300/health
    method: GET
    follow_redirects: false
    expected_status: 200
```

Exact expected-status ranges MAY be added if straightforward, but `2xx` is the default.

#### 9.13.5 TCP probe semantics

A TCP probe succeeds when a connection to the configured host and port is established within the timeout.

```yaml
ready:
  tcp:
    host: 127.0.0.1
    port: 10000
```

The connection is closed immediately after success unless a future protocol-specific probe requires otherwise.

#### 9.13.6 Exec probe semantics

Exec probes use the same direct-command or shell-command model as processes and hooks.

```yaml
ready:
  exec:
    command: [./scripts/check-ready.sh]
```

Rules:

- no PTY by default;
- stdin closed;
- process environment and cwd inherited from the process unless overridden;
- success determined by configured success exit codes, default `[0]`;
- stdout/stderr captured only up to a small diagnostic cap;
- timeout kills the probe process tree;
- probe children are not included in the process's runtime metrics.

#### 9.13.7 Log probe semantics

A log readiness probe observes the current run's live output stream:

```yaml
ready:
  log:
    contains: "Listening on http://localhost:5000"
```

Rules:

1. Matching is literal by default.
2. Matching operates across output-chunk boundaries.
3. ANSI/VT control sequences are stripped for matching by default.
4. Carriage-return updates are normalized consistently.
5. Matching is scoped to the current `RunId`.
6. Matching is performed live and MUST NOT depend on retained-history entries remaining untruncated.
7. The matcher has a bounded rolling window based on pattern length, not an unbounded line buffer.
8. Regex matching MAY be added later.

#### 9.13.8 Composite readiness

`all` succeeds when every child probe is passing:

```yaml
ready:
  all:
    - http:
        url: http://127.0.0.1:5300/health
    - tcp:
        host: 127.0.0.1
        port: 10000
    - tcp:
        host: 127.0.0.1
        port: 10001
    - tcp:
        host: 127.0.0.1
        port: 10002
  startup_timeout: 10m
```

Each child probe maintains its own threshold state. The composite passes only while all children pass.

#### 9.13.9 Readiness diagnostics

The latest useful diagnostic should be visible without opening internal debug logs:

```text
Readiness: HTTP GET http://127.0.0.1:5300/health
State: pending
Last error: connection refused
Attempts: 12
Elapsed: 24s / 10m
```

#### 9.13.10 Liveness

Liveness uses the same probe kinds and scheduling rules.

```yaml
liveness:
  http:
    url: http://127.0.0.1:5000/health/live
  interval: 10s
  timeout: 3s
  failure_threshold: 3
```

Liveness starts after the run first becomes effectively ready. It is canceled at stop/restart. A liveness failure marks the process unhealthy and applies `restart.on_unhealthy` if enabled.

---

## 10. Dependencies

### 10.1 Dependency conditions

The dependency vocabulary is:

```text
started
ready
exited
completed_successfully
```

`exited` explicitly means that success is not required.

### 10.2 Condition semantics

#### `started`

Satisfied while the dependency's current Run has spawned successfully and remains active in `Starting` or `Running`.

A stopping, completed, or exited Run does not satisfy `started` for a Process that has not yet started. A dependent that already started continues running when the dependency stops.

#### `ready`

Satisfied while a service is effectively ready:

- process alive and readiness `Passing`; or
- process alive with no readiness probe configured.

#### `exited`

Satisfied when a oneshot's most recent scheduled run has ended, regardless of success. It remains satisfied until a rerun begins.

#### `completed_successfully`

Satisfied when a oneshot's most recent scheduled run ended with a configured success exit code. It remains satisfied until a rerun begins.

### 10.3 Kind validation

The implementation SHOULD validate conditions by process kind:

- service dependencies: `started`, `ready`;
- oneshot dependencies: `started`, `exited`, `completed_successfully`.

A concrete future use case may relax this through an explicit specification change, but ambiguous service-completion semantics are not part of the core model.

### 10.4 Scheduling rules

1. An autostart process schedules required dependencies automatically.
2. Manually starting a process recursively schedules stopped dependencies.
3. Disabled dependencies do not auto-enable.
4. Dependency cycles are configuration errors detected before startup.
5. A dependency that is already in the process of satisfying the condition is not started twice.
6. A failed dependency blocks dependents with a structured reason.
7. A completed-successfully oneshot remains satisfied for the session until rerun.
8. Restarting a service dependency does not automatically rerun dependent oneshots.
9. Starting a rerun of a oneshot temporarily invalidates its previous exit/completion satisfaction.

### 10.5 Example

```yaml
processes:
  storage-init:
    kind: oneshot
    command: [python, scripts/azurite_init.py]
    depends_on:
      storage: ready

  api:
    kind: service
    command: [dotnet, run, --launch-profile, Local]
    depends_on:
      storage: ready
      storage-init: completed_successfully
```

---
