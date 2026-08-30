# Product direction

[Back to the product specification](../product-specification.md)

## 0. Document purpose and conventions

### 0.1 Purpose

This document defines the durable product direction, behavioral contracts, and architectural boundaries for Stackhand.

It intentionally does **not** define implementation sequencing, milestones, technical spikes, release decisions, library-evaluation tasks, or coding-agent workflow. Those belong in the companion implementation plan and may change more frequently without changing this specification.

A change to the implementation plan does not require a specification revision unless it changes product behavior, public configuration semantics, lifecycle semantics, ownership boundaries, security policy, or another normative requirement in this document.

### 0.2 Normative language

The terms **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are used intentionally:

- **MUST / MUST NOT**: required for architectural correctness or product acceptance.
- **SHOULD / SHOULD NOT**: expected default; deviation requires concrete evidence and documentation.
- **MAY**: optional or safely deferrable.

Illustrative Rust types and YAML examples communicate behavior, not frozen public APIs. The implementation may use different names or structures when it preserves the specified semantics.

### 0.3 Stability and change control

The specification is intended to remain useful across prototype phases and any later product work.

- Durable product and architecture decisions belong here.
- Current sequencing, experiments, and dependency choices belong in the implementation plan.
- Temporary workarounds MUST NOT silently redefine normative behavior.
- Deliberate deviations from a **MUST** or **MUST NOT** requirement require an explicit specification update.
- Open details may remain unresolved when their resolution does not alter the contracts defined here.

### 0.4 Source and continuity

This repository document is the canonical specification. Earlier drafts outside the repository are not maintained sources. The companion implementation plan keeps short-lived execution details separate so that prototype evidence can change them without silently changing this specification.

---

## 1. Executive summary

Build a lightweight local-development process supervisor that combines the strongest parts of **mprocs/dekit** and **Process Compose**, without inheriting the amount of custom terminal and TUI infrastructure those projects maintain.

The product should feel much closer to **mprocs** in normal use:

- a compact process list;
- a large selected-process console;
- low visual noise;
- fast keyboard-driven control;
- excellent output inspection and interactive-terminal behavior.

The console is the primary workspace, not a secondary log widget. Scrolling, following, selecting, copying, searching, resizing, and interacting with terminal applications are essential prototype capabilities.

The lifecycle model should borrow the strongest concepts from **Process Compose** and the user's **Quadrant Aspire AppHost**:

- long-running services;
- visible one-shot initialization jobs;
- dependencies that wait for start, readiness, exit, or successful completion;
- readiness and liveness checks;
- startup timeouts;
- restart behavior;
- bounded lifecycle hooks;
- aggregate process metrics;
- clear blocked and failure diagnostics.

The implementation remains intentionally focused. It MUST NOT become a Docker Compose replacement, Aspire replacement, SSH manager, cloud environment manager, general workflow engine, or machine-provisioning tool. Machine-specific behavior belongs in ordinary Processes, bounded hooks, scripts, Process Profiles, or a gitignored local override.

The central architectural division is:

- **Ratatui** owns application layout, widgets, menus, help, status, and modal interaction.
- **libghostty-vt** owns virtual-terminal semantics: VT parsing, terminal state, scrollback, wrapping/reflow, cursor state, selection semantics, formatting, input encoding, mouse protocol encoding, and terminal query behavior.
- **The supervisor** owns desired state, lifecycle state, dependency scheduling, probes, restarts, hooks, metrics, and project shutdown.
- **The process runtime** owns spawning, pipes, PTYs, process groups/jobs, signals, exit observation, and process-tree cleanup.
- **Per-process output owners** retain bounded multi-run Logs history and own a fresh terminal session for each Run.

The project MUST NOT reimplement a terminal emulator or a general TUI framework.

---

## 2. Product goals

### 2.1 Primary goals

#### 2.1.1 mprocs-like usability

- Compact process list beside a dominant console pane.
- Low visual noise and minimal dashboard chrome.
- Keyboard-driven selection and lifecycle actions.
- Fast start, stop, restart, rerun, focus, and zoom operations.
- Predictable behavior when an interactive child owns ordinary keyboard input.

#### 2.1.2 Excellent output inspection

- Continuous output draining for every process, focused or unfocused.
- ANSI color and style preservation.
- Reliable terminal scrollback for the current run.
- Bounded multi-run line-oriented history.
- Follow and unfollow behavior.
- Literal search with next/previous navigation.
- Mouse and keyboard selection.
- Correct copying across soft-wrapped lines.
- Interactive shells, editors, pagers, fuzzy finders, and REPLs in PTY mode.

#### 2.1.3 First-class lifecycle semantics

- Long-running `service` processes.
- Short-lived `oneshot` processes.
- Dependency gating by explicit condition.
- Readiness and liveness probes.
- Startup timeout distinct from individual probe timeout.
- Restart policies with a bounded automatic restart budget.
- Graceful shutdown followed by escalation.
- Visible blocked, waiting, backoff, completed, failed, and stopped states.
- Stale asynchronous events safely ignored by `RunId`.

#### 2.1.4 Useful runtime observability

- Root PID.
- Aggregate descendant CPU usage.
- Aggregate descendant resident memory.
- Uptime or age.
- Current run identifier or attempt number where useful.
- Total restart count and automatic restart attempts used.
- Readiness and health state.
- Last exit status and structured failure reason.
- Latest useful probe diagnostic.

#### 2.1.5 Extensibility without core bloat

- Bounded generic lifecycle hooks.
- Named Process Profiles for partial Process configuration.
- One global Process Profile selection for future Runs.
- Automatically discovered gitignored local overlay.
- Ordinary supervised helper processes for long-running machine-specific work.
- Scripts remain valid commands and extension points.
- Domain-specific initialization and validation remain in Project commands or
  scripts.
- No first-class SSH, Docker, VPN, cloud-authentication, or emulator-specific process types.

#### 2.1.6 A relatively small, maintainable codebase

- Reuse Ratatui and libghostty rather than building equivalents.
- Keep authoritative supervisor state independent from Ratatui widgets.
- Keep unsafe/native terminal integration behind one adapter boundary.
- Keep the high-volume output path independent from lifecycle command processing.
- Avoid premature daemon, API, plugin, persistence, and distributed-system layers.
- Prefer a small number of clear ownership boundaries over a broad framework hierarchy.

### 2.2 Secondary goals

- Cross-platform public configuration and state concepts.
- Understandable configuration for complex monorepos.
- Manual/opt-in processes in addition to autostart processes.
- Multiple Process Profiles such as `local`, `devcloud`, and `localProd` without duplicating complete Process definitions.
- Architecture that can support a future daemon/attach protocol without redesigning the supervisor core.
- Reproducible packaging that does not require users of a packaged prototype to install Zig or compile Ghostty.

---

## 3. Non-goals

The following are outside the core product model and MUST NOT become first-class domain concepts without a deliberate specification revision:

- Docker container orchestration.
- Docker Compose parsing or lifecycle ownership beyond running Docker commands as ordinary processes.
- SSH port-forward management.
- Remote machine provisioning.
- VPN management.
- Cloud login or cloud-resource provisioning.
- Secret management.
- Kubernetes semantics.
- Full observability or OpenTelemetry dashboarding.
- Built-in web UI.
- Distributed multi-host scheduling.
- General-purpose workflow engine.
- Arbitrary plugin runtime.
- Terminal image rendering, Kitty graphics, or sixel rendering.
- Pixel-perfect equivalence with a native GPU terminal emulator.
- Built-in daemon/detach behavior as part of the local TUI product.

A machine-specific prerequisite or SSH forward should be represented by one of:

- a bounded hook;
- a script;
- a gitignored local override;
- an ordinary long-running helper service.

---

## 4. Design principles

### 4.1 Output is the primary UI

Developers spend most of their time reading and interacting with output. The process list is navigation and status; the console is the main workspace.

The product MUST NOT optimize around charts, dashboards, or dense tables at the expense of terminal readability.

### 4.2 Lifecycle, readiness, and health are separate

A process may be:

- alive but not ready;
- alive and ready;
- alive and unhealthy;
- blocked before spawn;
- stopped intentionally;
- waiting in restart backoff;
- completed successfully;
- failed before or after spawn.

These concepts MUST remain orthogonal internally even when the UI projects them into one compact label.

### 4.3 Dependencies are visible gates, not hidden shell pipelines

Avoid definitions such as:

```sh
wait-for-database && initialize-x && run-api
```

when the behavior can be represented as visible processes and dependency conditions.

A long-running service SHOULD NOT hide a failed required initialization step in its own output.

### 4.4 Hooks are bounded escape hatches, not a second process manager

Hooks are for bounded lifecycle actions. A command that must remain alive belongs in the process graph as a service.

### 4.5 Generic core, domain-specific commands

The core understands:

- process;
- service;
- oneshot;
- dependency;
- readiness;
- liveness;
- restart;
- hook;
- process tree;
- terminal session.

The core does not understand:

- Service Bus emulator;
- Azurite;
- Cosmos emulator;
- SSH tunnel;
- pnpm;
- .NET launch profile;
- a particular cloud or local stack.

### 4.6 Always drain output independently of focus

Unfocused processes MUST continue consuming stdout, stderr, or PTY output.

The system cannot promise infinite producer throughput; when a producer exceeds all bounded processing capacity, operating-system backpressure is unavoidable. However, backpressure MUST NOT occur merely because the process is not selected or because the TUI is rendering another pane.

### 4.7 Bound memory globally and locally

Every output-related structure MUST have an explicit limit, including:

- terminal scrollback;
- raw/log history;
- logical-line assembly;
- search indexes;
- pending input;
- pending terminal effects;
- hook output;
- render snapshots.

A per-process cap alone is insufficient. The project MUST also enforce a total retained-output budget.

### 4.8 Separate control and data planes

Lifecycle commands and events are low-volume, latency-sensitive control traffic. Process output is high-volume data traffic.

Raw output chunks MUST NOT pass through the authoritative supervisor event queue. Output flood from one process MUST NOT delay stop commands, process exits, probe transitions, restart timers, or project shutdown.

### 4.9 Make ownership explicit

Each mutable subsystem MUST have one clear owner:

- supervisor state: supervisor task;
- process handles and containment: process runner/runtime;
- terminal and retained output: Process output owner;
- TUI interaction state: application/UI task.

Broad shared mutable state behind `Arc<Mutex<...>>` SHOULD be avoided.

### 4.10 Prefer deterministic semantics over convenience magic

Command execution, path resolution, environment precedence, Process Profile selection, merge behavior, dependency satisfaction, restart suppression, and shutdown escalation MUST be documented and testable.

---

## 5. Reference use case: Quadrant

The user's `alyzenmed/QUADRANT` repository is the representative complex local-development environment used to validate the model.

Current mprocs usage includes processes such as:

- local emulator runner;
- .NET API;
- Vite frontend;
- documentation site;
- Python worker;
- .NET Azure Functions;
- smoke/check jobs;
- multiple environment variants.

The existing mprocs configuration is intentionally thin, while more complicated lifecycle logic lives in scripts.

The emulator workflow approximately performs:

1. Start local infrastructure containers.
2. Wait for several HTTP and TCP readiness conditions.
3. Initialize storage resources and CORS.
4. Initialize Cosmos processes.
5. Optionally run smoke tests.
6. Declare the local backend usable.
7. Continue streaming logs.
8. Stop relevant containers when the wrapper exits.

Other processes wait for the emulator environment before starting.

The Quadrant Aspire AppHost expresses two important relationships directly:

- wait for a service to become ready;
- wait for a oneshot initializer to complete successfully.

This distinction MUST be supported.

Quadrant is a validation fixture, not a reason to make the core a workflow
engine. Stackhand SHOULD replace generic coordination that starts, waits for,
restarts, or stops Processes. Quadrant-specific actions that create resources,
configure application data, or validate application behavior MUST remain in
Quadrant commands or scripts.

Running a domain tool as an ordinary command does not add that tool to the
Stackhand model. For example, Stackhand may supervise a `docker compose`
command or a Cosmos initialization script. Stackhand does not need to
understand Docker Compose or Cosmos.

When a script mixes generic coordination with domain-specific work, move only
the generic coordination into Stackhand configuration. Keep the remaining
domain-specific command small and explicit.

Quadrant remains a continuing validation case for the model. Proposed lifecycle or configuration changes SHOULD be checked against both a small synthetic Quadrant-like graph and the real repository workflow before they become durable product concepts.

---

## 6. Product experience

### 6.1 Default interaction model

The default screen is a process list beside a large selected-process console:

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

The console SHOULD receive most of the horizontal space. Metrics and details MUST remain compact. The Process list MUST show a Profile column when a current Run differs from its Next Profile or when any Process Next Profile differs from the global Process Profile. A profile selection has no immediate lifecycle effect. Active Processes continue until the user applies the pending change or another lifecycle action creates a new Run.

### 6.2 Two output representations

Each process exposes two related views:

#### Terminal view

- Current or most recent run.
- Ghostty terminal state.
- ANSI styling.
- Alternate-screen behavior.
- Cursor and interactive input.
- Terminal scrollback.
- Terminal-native selection and copy formatting.

#### Logs view

- Bounded multi-run history.
- Run and hook separators.
- Timestamps.
- stdout/stderr/PTY tags where known.
- Literal search and match navigation.
- Stable line-oriented representation.
- No claim that rows correspond one-for-one with the terminal grid.

Search MAY be entered from Terminal view, but results SHOULD be presented in Logs view rather than requiring fragile raw-history-to-terminal-coordinate mapping. A future implementation MAY add direct terminal-result navigation only when it can do so reliably.

### 6.3 Process control

The user can:

- start selected;
- start a selected Waiting Process without requiring Dependencies for one Run;
- stop selected;
- restart selected service;
- rerun selected oneshot;
- Start Default;
- Start All;
- stop all;
- restart all running services;
- cycle the global Process Profile for future Runs;
- apply pending profile changes to affected active Processes;
- zoom the console;
- switch Terminal/Logs view;
- inspect blocked and failure reasons;
- quit with controlled shutdown.

### 6.4 Interactive child behavior

When focused input forwarding is enabled:

- ordinary keyboard input goes to the child;
- `Ctrl-C` goes to the child;
- the application leader key returns to application command context;
- application scroll/search/selection commands require explicit command context;
- mouse events go to the child when the child has enabled mouse tracking, except for an explicit application selection override.

---
