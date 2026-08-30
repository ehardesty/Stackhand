# Interface and operations

[Back to the product specification](../product-specification.md)

## 24. TUI design

### 24.1 Default layout

Keep the visual hierarchy close to mprocs:

```text
┌ Processes ─────────────────────┬──────────────────────────────────────┐
│ ● web              READY       │                                      │
│ ● api              READY       │                                      │
│ ● servicebus       READY       │       selected process console      │
│ ● storage          READY       │                                      │
│ ✓ storage-init     DONE        │                                      │
│ ● cosmos           READY       │                                      │
│ ✓ cosmos-init      DONE        │                                      │
│ ◌ worker-python    WAITING     │                                      │
│ ○ func-dotnet      STOPPED     │                                      │
└────────────────────────────────┴──────────────────────────────────────┘
 status/help footer
```

The console should receive most of the screen width and height. The Process list
MUST show a Profile column when a current Run's applied profile differs from its
Next Profile, or when any Process's Next Profile differs from the global Process
Profile. It SHOULD hide the column at other times.

### 24.2 Process row data

Minimum visible fields:

- name;
- primary status;
- optional compact CPU;
- optional compact memory;
- conditional current and Next Profile.

When the Profile column is visible, a running Process with different profiles
shows `current → next`. Other rows show the Next Profile.

Example:

```text
api           READY      local → devcloud   3.2%   184M
worker        WAITING    devcloud               -       -
init-storage  DONE       base                   -       -
```

Metrics columns may be toggled when space is limited.

### 24.3 Status projection

The UI composes desired, lifecycle, readiness, and health into concise labels:

```text
DISABLED
STOPPED
WAITING
STARTING
READY
NOT READY
UNHEALTHY
STOPPING
DONE
FAILED
RESTARTING
```

The underlying state remains structured; labels are presentation only.

### 24.4 Selected-process header

A compact header may show:

```text
api · run 3 · local → devcloud · PID 48122 · READY · 2m14s · 184 MiB · 3.2% CPU · restarts 1 · TERMINAL
```

For a blocked process:

```text
worker · WAITING · storage-init must complete successfully
```

Do not consume large vertical space with dashboards.

### 24.5 Focus and modes

Primary focus scopes:

- process list;
- console.

Console modes:

- child input;
- app command;
- scroll;
- selection;
- search/logs.

The footer should make the current focus/mode clear enough that users understand whether keys will reach the child.

### 24.6 Suggested default keybindings

When the process list is focused:

```text
↑/↓ or j/k     select process
s              start selected
x              stop selected
r              restart/rerun selected
p              cycle the global Process Profile for future Runs
R              apply a pending profile change; shown only while changes are pending
z              zoom console
/              search selected output
l              toggle Terminal/Logs view
q              quit with controlled shutdown
Ctrl-A         focus console / enter console context
```

When the console is in child input mode:

```text
ordinary keys  forwarded to child when input is focused
a leader key   enter application command context
```

When in application command context:

```text
Esc            return/cancel
PageUp/Down    enter or move in scroll mode
/              search in Logs view
v              selection mode
y              copy selection
f              follow/live tail
l              toggle Terminal/Logs
z              zoom
```

The exact leader key remains open, but key routing rules are not open.

### 24.7 Zoom

Zoom/full-console mode is required. Layout changes must:

- compute new terminal cell geometry;
- resize Ghostty state;
- resize the child PTY;
- avoid sending transient zero sizes;
- coalesce rapid resize events;
- preserve terminal viewport/selection behavior where possible.

### 24.8 Small terminals

Define graceful degradation for narrow/small outer terminals:

- hide optional metrics columns;
- reduce borders/padding;
- allow process-list-only or console-only layouts;
- keep errors and mode state visible;
- never panic on zero-width/height intermediate regions.

### 24.9 Help and command discovery

A compact help overlay may document modes and current keybindings. It should not replace contextual footer hints.

---

## 25. Metrics

### 25.1 Core metrics

For active services:

- root PID;
- aggregate resident memory;
- aggregate CPU percentage;
- age/uptime;
- automatic restart count;
- optional descendant count.

For completed/failed processes, preserve final exit status and optionally final sampled memory/CPU for the current run.

### 25.2 Process-tree aggregation

Aggregate descendants where feasible. This matters for wrapper commands such as:

```text
pnpm dev
bash -lc ...
dotnet run
```

Use process-group/session or job membership where reliable. When the platform cannot determine ownership exactly, expose metrics as best effort rather than silently claiming precision.

### 25.3 Sampling

A one-second interval is sufficient by default.

Metrics collection:

- runs outside the supervisor loop;
- carries `RunId`;
- does not block process I/O;
- sends one bounded snapshot per interval;
- ignores stale samples;
- tolerates processes exiting during enumeration.

The implementation MUST prioritize reliable process-tree semantics over minimizing dependencies.

### 25.4 CPU definition

Document whether CPU percentage is normalized to one logical CPU or the whole machine. The UI and tests must use one consistent definition.

---

## 26. Health, dependency, and failure UX

Lifecycle problems must be understandable without opening every log.

Examples:

```text
api            WAITING · cosmos-init
cosmos         STARTING · readiness 34s
storage-init   FAILED · exit 1
worker         WAITING · storage-init must succeed
web            STOPPED
```

Selecting a blocked or failed process should show a compact explanation before the console or in a detail strip.

### 26.1 Probe diagnostic

Retain bounded information such as:

```text
Readiness: HTTP GET http://127.0.0.1:5300/health
State: failing (3 consecutive failures)
Last error: connection refused
Attempts: 12
Elapsed: 24s / startup timeout 10m
```

For composites, identify each child probe state.

### 26.2 Failure diagnostic

Examples:

```text
Spawn failed: executable not found: dotnet
before_start failed: exit 2
Startup timeout after 10m
Process exited 137
Process terminated by signal 9
Restart limit reached after 5 attempts
PTY reader failed: input/output error
Output session failed: terminal parser initialization error
```

### 26.3 Run history

At minimum, preserve bounded recent run summaries:

- run ID;
- start/end time;
- exit disposition;
- failure reason;
- restart trigger.

A full historical database is not required.

---

## 27. Project-level actions

The product supports:

- Start Default: start enabled autostart Processes and their Dependencies;
- Start All: start every enabled Process;
- stop all;
- restart all currently running services;
- cycle the global Process Profile for future Runs;
- apply pending profile changes to affected active Processes;
- start selected;
- stop selected;
- restart selected service;
- rerun selected oneshot;
- open effective configuration diagnostics;
- quit with controlled shutdown.

### 27.1 Start Default and Start All

Both actions recursively schedule required enabled Dependencies. Processes remain visibly blocked until gates are satisfied. Starting a Process sets it and each scheduled prerequisite to desired running. Disabled Dependencies remain disabled and block the request.

### 27.2 Stop all

Stop-all suppresses automatic restarts for the action and drives desired state to stopped. The user may later start processes again within the same session.

### 27.3 Restart all

Restart-all should operate on Processes that are currently running or in
startup/restart state. It should not unexpectedly start disabled or manually
stopped optional Processes.

`p` cycles the global Process Profile. This selection has no immediate
lifecycle effect. It changes Next Profiles only. It MUST NOT change Desired
State or modify, stop, restart, or start a Process. Active Processes continue.

When at least one current Run differs from its Next Profile, the footer MUST
show `R: apply profile`. It MUST hide this control when no profile change is
pending. An affected Process has an active Run whose applied profile differs
from its Next Profile.

`R` MUST stop each affected active Process whose Next Profile disables it. It
MUST restart each affected active Process that remains enabled and uses
autostart. The restart MUST start each newly enabled Dependency required by the
Process's Next Profile. It MUST NOT start other inactive Processes that the Next
Profile newly enables. The user can start those Processes manually. An affected
active Process that remains enabled but does not use autostart continues without
a restart.

### 27.4 Quit

Default quit behavior stops supervised processes. Daemon/detach behavior is outside this specification.

If shutdown cannot confirm cleanup, the final error must identify remaining processes/PIDs where available.

---

## 28. Configuration validation

Validation occurs before any project process starts.

### 28.1 Structural validation

- supported schema version;
- unique process names;
- exactly one of `command` or `shell`;
- valid kind;
- valid terminal mode/input combination;
- valid duration and byte-size values;
- valid ports and URLs;
- valid hook/probe structures;
- valid success exit codes;
- no unknown fields unless forward-compatibility policy explicitly permits them.

### 28.2 Graph validation

- every referenced dependency exists;
- no dependency cycles in base or any selectable global Process Profile;
- disabled dependencies remain representable and diagnosable;
- conditions are valid for referenced processes;
- each Process Profile uses only allowed fields;
- `base` is not defined as a Process Profile;
- each per-Process `profile` override names `base` or a profile defined by that Process.

### 28.3 Path validation

Where possible before start:

- base config directory exists;
- cwd exists;
- required env files exist;
- explicitly relative executable paths exist;
- local override syntax is valid.

Do not require PATH-resolved executables to exist at config-parse time if the environment may change; report clear spawn errors instead.

### 28.4 Semantic warnings

Warnings may identify suspicious but valid configuration:

- `ready` dependency on a service with no explicit readiness probe;
- `completed_successfully` dependency on a long-running service;
- an autostart process depending on a disabled process;
- extremely large output limits;
- a shell expression using Bash syntax while configured shell is `/bin/sh`.

Warnings must not silently change semantics.

### 28.5 Error locations

Configuration errors should include:

- file path;
- YAML path;
- line/column when available;
- concise explanation;
- relevant Process or Process Profile name.

The TUI must not half-start a project with an invalid effective graph.

---

## 29. Concurrency and performance requirements

### 29.1 Supervisor responsiveness

The supervisor loop must remain responsive during:

- sustained multi-process output;
- a blocked PTY writer;
- slow probes;
- slow hooks;
- process-tree enumeration;
- terminal rendering;
- configuration reload attempts if added later.

No blocking process I/O, network request, wait, or hook execution occurs on the supervisor task.

### 29.2 Output-session scheduling

Each Process output owner should batch bytes and bound work per scheduling turn. A noisy Process must not starve other output owners or control work.

The scheduling model is not normative, but it MUST preserve fairness, bounded queues, continuous draining, and responsive control-plane work.

### 29.3 UI snapshots

The UI renders only the selected terminal, but every Process output owner continues ingesting.

Render snapshots should be requested only when:

- selected output revision changes;
- viewport/selection changes;
- cursor blink requires redraw;
- layout changes;
- a modest periodic tick updates metrics/status.

### 29.4 Frame rate

Use event-driven redraw plus a modest maximum frame rate. Avoid a busy loop.

The terminal renderer should coalesce output bursts and use dirty-row information where practical.

### 29.5 Cancellation

Every run-scoped task must be cancellable or safely ignorable by `RunId`:

- probes;
- hook invocations;
- restart timers;
- startup timers;
- metrics samplers;
- output notifications;
- delayed shutdown stages.

Cancellation plus generation checks is preferred; generation checks are mandatory even when cancellation exists.

### 29.6 Stress requirements

Automated or repeatable stress scenarios must cover:

- one process writing continuous high-volume output;
- several processes writing concurrently;
- user navigating and stopping another process during the flood;
- repeated rapid restarts;
- readiness results arriving after restart;
- selection while output advances scrollback;
- large paste into a temporarily blocked PTY;
- project shutdown during probe/hook activity.

---

## 30. Cross-platform requirements

### 30.1 Platform-neutral product model

The public configuration and state model MUST remain platform-neutral for:

- desired, lifecycle, readiness, and health state;
- semantic process-tree actions;
- pipe and PTY modes;
- terminal input and output;
- metrics snapshots;
- configuration and dependency semantics.

### 30.2 Platform-specific runtime boundaries

Platform behavior belongs behind narrow runtime modules.

- Unix implementations use the appropriate PTY, process-group/session, signal, and process-enumeration mechanisms.
- Windows implementations use ConPTY, Job Objects, console-control or termination mechanisms, and Windows path/shell conventions.
- Unsupported behavior must fail clearly rather than silently degrading into incorrect lifecycle semantics.

### 30.3 Portability standard

A platform is not considered supported merely because the project compiles. Supported platforms must satisfy the terminal-input, PTY resize, process-tree cleanup, shutdown, metrics, packaging, and integration behavior defined by this specification.

Platform-validation order belongs in the companion implementation plan.

---
