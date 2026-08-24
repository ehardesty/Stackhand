# Safety and reference

[Back to the product specification](../product-specification.md)

## 31. Error handling and diagnostics

### 31.1 User-facing errors

Surface errors at the level users care about, with process and run context.

Examples:

```text
api · run 4 · spawn failed: executable `dotnet` not found
storage · run 2 · readiness timeout after 10m
api · run 5 · before_start hook exited 1
worker · blocked: storage-init has failed
web · run 3 · exited 137
shell · run 1 · PTY reader closed unexpectedly
```

### 31.2 Internal diagnostics

Internal logs may be written to a separate file or enabled through an environment variable/CLI flag.

They should include:

- task and channel failures;
- stale-event drops at debug/trace level;
- process-tree escalation details;
- FFI errors;
- output truncation and queue saturation;
- configuration merge trace when requested.

Do not dump full effective environments or other likely secrets by default.

### 31.3 Panic policy

A malformed child output stream, probe response, clipboard failure, or one process runtime failure must not crash the entire application.

Unexpected invariant violations should be logged with enough context to reproduce them. Where possible, fail the affected Process output owner and keep Project control available.

---

## 32. Security and trust boundaries

This is a local developer tool that intentionally executes arbitrary project commands. It is not a sandbox.

### 32.1 Configuration trust

Opening a project and starting processes may execute:

- process commands;
- hooks;
- exec probes;
- shell expressions;
- local overlays.

The application should not imply these are safe merely because they are YAML.

A future trust prompt for untrusted repositories may be valuable, but it is not required for the first vertical slice.

### 32.2 Environment and logs

- Do not log effective environment values by default.
- Do not expose secrets in merge diagnostics unless explicitly requested.
- Cap hook and probe output.
- Treat titles, working directories, hyperlinks, and terminal-generated strings as untrusted display content.

### 32.3 Terminal OSC policy

Recommended defaults:

- clipboard read request from child: deny;
- clipboard write request from child: deny or prompt;
- user-initiated copy: allow;
- desktop notification request: disabled or opt-in;
- title and working-directory updates: accept for display only after sanitization;
- file-loading graphics protocols: disabled by default;
- unknown sequences: ignore or trace, never execute.

### 32.4 Clipboard

Never send system clipboard contents to a child merely because it requests them.

User paste is an explicit action and follows the paste-safety policy.

### 32.5 Resource exhaustion

Bound:

- output and scrollback;
- OSC payloads as supported by the terminal library;
- graphics data if ever enabled;
- paste size;
- probe response bodies;
- search work;
- task/channel counts.

### 32.6 Process containment disclaimer

Process groups and Job Objects improve cleanup but do not create a security boundary against malicious child processes.

---

## 33. Suggested internal types

These are illustrative contracts, not mandatory exact APIs.

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct ProcessId(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RunId(u64);

enum ProcessKind {
    Service,
    Oneshot,
}

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

enum CheckState {
    NotConfigured,
    Inactive,
    Pending,
    Passing,
    Failing,
}

enum DependencyCondition {
    Started,
    Ready,
    Exited,
    CompletedSuccessfully,
}

enum TerminalMode {
    Pipe,
    Pty,
}

enum InputPolicy {
    Disabled,
    Focused,
}
```

Structured failure state:

```rust
enum FailureReason {
    SpawnFailed { message: String },
    HookFailed { hook: HookKind, result: HookResult },
    StartupTimeout { timeout: Duration },
    LivenessFailure { diagnostic: ProbeDiagnostic },
    UnexpectedExit { status: ExitStatus },
    ProcessIoFailure { message: String },
    ProcessOutputFailure { message: String },
    RestartLimitExceeded,
    Internal { message: String },
}
```

Process state:

```rust
struct ProcessState {
    id: ProcessId,
    enabled: bool,
    current_run: Option<RunId>,
    desired: DesiredState,
    lifecycle: LifecycleState,
    readiness: CheckState,
    health: CheckState,
    blocked_reasons: Vec<BlockedReason>,
    failure: Option<FailureReason>,
    exit_status: Option<ExitStatus>,
    metrics: RuntimeMetrics,
    total_restart_count: u64,
    automatic_restart_attempts: u32,
    output: ProcessOutputHandle,
}
```

Control-plane events:

```rust
enum SupervisorEvent {
    Spawned {
        id: ProcessId,
        run_id: RunId,
        pid: u32,
        tree: ProcessTreeIdentity,
    },
    SpawnFailed {
        id: ProcessId,
        run_id: RunId,
        error: RuntimeError,
    },
    ReadinessChanged {
        id: ProcessId,
        run_id: RunId,
        state: CheckState,
        diagnostic: Option<ProbeDiagnostic>,
    },
    HealthChanged {
        id: ProcessId,
        run_id: RunId,
        state: CheckState,
        diagnostic: Option<ProbeDiagnostic>,
    },
    Exited {
        id: ProcessId,
        run_id: RunId,
        status: ExitStatus,
    },
    HookFinished {
        id: ProcessId,
        run_id: RunId,
        hook: HookKind,
        result: HookResult,
    },
    StartupTimedOut {
        id: ProcessId,
        run_id: RunId,
    },
    RestartBackoffElapsed {
        id: ProcessId,
        failed_run_id: RunId,
    },
    MetricsUpdated {
        id: ProcessId,
        run_id: RunId,
        metrics: RuntimeMetrics,
    },
    ProcessOutputFailed {
        id: ProcessId,
        run_id: RunId,
        error: OutputError,
    },
}
```

Output bytes are intentionally absent from `SupervisorEvent`.

UI commands:

```rust
enum SupervisorCommand {
    Start(ProcessId),
    Stop(ProcessId),
    Restart(ProcessId),
    Rerun(ProcessId),
    StartDefault,
    StartAllEnabled,
    StopAll,
    RestartRunningServices,
    ShutdownProject,
}
```

### 33.1 Stale-event invariant

For every event containing `run_id`:

```text
if event.run_id != process.current_run:
    ignore event
```

Exceptions must be explicit and tested, such as Process history finalizing an older Run marker without mutating current lifecycle state.

---

## 34. Illustrative Quadrant-like configuration

This example uses the revised direct-command and shell-command distinction. It remains illustrative and does not require Quadrant to rewrite existing wrapper scripts immediately.

```yaml
version: 1

settings:
  output:
    global_history_bytes: 256MiB
    per_process_history_bytes: 16MiB
    terminal_scrollback_bytes: 16MiB

processes:
  servicebus:
    kind: service
    command: [./scripts/dev/run-servicebus.sh]
    autostart: true
    ready:
      http:
        url: http://127.0.0.1:5300/health
      interval: 2s
      startup_timeout: 10m

  storage:
    kind: service
    command: [./scripts/dev/run-storage.sh]
    autostart: true
    ready:
      all:
        - tcp: { host: 127.0.0.1, port: 10000 }
        - tcp: { host: 127.0.0.1, port: 10001 }
        - tcp: { host: 127.0.0.1, port: 10002 }
      startup_timeout: 10m

  cosmos:
    kind: service
    command: [./scripts/dev/run-cosmos.sh]
    autostart: true
    ready:
      http:
        url: http://127.0.0.1:18080/ready
      startup_timeout: 10m

  storage-init:
    kind: oneshot
    shell: "source .venv/bin/activate && exec python scripts/azurite_init.py"
    cwd: app/functions-python
    autostart: true
    depends_on:
      storage: ready

  cosmos-init:
    kind: oneshot
    shell: "source app/functions-python/.venv/bin/activate && exec python scripts/local-dev/emulators/cosmos-init.py --skip-wait --timeout-seconds 600"
    autostart: true
    depends_on:
      cosmos: ready

  api:
    kind: service
    command: [dotnet, run, --launch-profile, Local]
    cwd: app/api
    autostart: true
    depends_on:
      servicebus: ready
      storage: ready
      cosmos: ready
      storage-init: completed_successfully
      cosmos-init: completed_successfully
    ready:
      http:
        url: http://127.0.0.1:5000/health
      interval: 2s
      startup_timeout: 2m
    restart:
      policy: on_failure
      backoff: 2s
      max_restarts: 3

  web:
    kind: service
    command: [pnpm, dev]
    cwd: app/frontend
    autostart: true
    depends_on:
      api: ready

  worker-python:
    kind: service
    shell: "source .venv/bin/activate && exec python scripts/run_local_worker.py"
    cwd: app/functions-python
    autostart: false
    depends_on:
      servicebus: ready
      api: ready
      storage-init: completed_successfully
      cosmos-init: completed_successfully

  func-dotnet:
    kind: service
    command: [func, host, start]
    cwd: app/functions
    autostart: false
    depends_on:
      servicebus: ready
      api: ready
      storage-init: completed_successfully
      cosmos-init: completed_successfully
```

Existing `run.sh` and `prepare.sh` scripts remain valid processes. The tool should improve orchestration incrementally rather than requiring immediate declarative decomposition.

---

## 35. Definition of success

The finished product should feel like this:

- launching it is as direct as mprocs;
- the selected process's output dominates the UI;
- interactive console behavior feels like a real modern terminal rather than a crude log box;
- output can be inspected, searched, selected, and copied reliably;
- a noisy background process does not degrade control of the project;
- complex startup ordering does not require long chained shell expressions;
- one-shot initialization work is visible and understandable;
- readiness is distinct from process existence;
- health is distinct from readiness;
- stale probe or exit races cannot corrupt state;
- process CPU and memory are immediately visible;
- project shutdown cleans up ordinary process trees reliably;
- machine-specific setup can be added locally without contaminating the shared model;
- the codebase remains much smaller than a custom terminal emulator plus custom TUI framework.

The core differentiator is:

> **A developer-focused process supervisor with excellent terminal/output ergonomics and a small but capable lifecycle model.**

---

## 36. Change history

### Revision 4 — 2026-08-24

Made the repository copy canonical and named the product Stackhand. Standardized Process, Run, Process Tree, Project, and Dependency language. Reconciled direct commands, output ownership, startup conditions, hooks, restart budgets, configuration discovery and overlays, Project actions, and prototype status with the documentation interview and ADRs.
