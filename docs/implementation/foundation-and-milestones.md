# Foundation and milestones

[Back to the prototype implementation plan](../implementation-plan.md)

## 1. Purpose and change policy

This document contains the implementation-specific material that is expected to change as evidence accumulates:

- technical spikes and go/no-go gates;
- library and backend evaluations;
- package/module organization;
- platform and prototype sequencing;
- milestones and vertical slices;
- test execution strategy;
- prototype validation criteria;
- active risks and unresolved implementation choices;
- source-reading assignments;
- coding-agent workflow.

The companion specification is authoritative for product behavior and architectural contracts. This plan may change without a specification revision when the change does not alter those contracts.

When the plan conflicts with the specification, the specification wins.

---

## 2. Current implementation baseline

### 2.1 Starting technical choices

The current working baseline is:

- Rust application;
- Ratatui application UI;
- `libghostty-vt` for terminal semantics;
- a project-owned safe terminal adapter;
- an event-driven supervisor with immutable UI snapshots;
- per-process output histories and per-Run terminal sessions outside the lifecycle control queue;
- macOS as the current real-world prototype target;
- Linux in a dedicated portability and validation milestone;
- Windows after explicit ConPTY, Job Object, input, shutdown, and packaging validation.

These are implementation choices for the current plan. Any change that alters a normative contract in the companion specification requires a specification revision.

### 2.2 Initial package and module shape

Begin with one Rust package and clear modules rather than a broad workspace:

```text
src/
  main.rs
  app.rs
  config/
  model/
  supervisor/
  runtime/
  probes/
  metrics/
  hooks/
  output/
  terminal/
  tui/
  platform/
```

Introduce a separate native/FFI crate only when it materially isolates unsafe Ghostty bindings, improves packaging, or creates a clear reusable boundary.

### 2.3 PTY transport evaluation

Evaluate `portable-pty` first because it offers a cross-platform PTY API. Its generic child-kill behavior must not define the product's shutdown ladder.

Verify:

- reader/writer behavior;
- nonblocking integration or dedicated blocking-thread behavior;
- resize correctness;
- child PID availability;
- process-group/session interaction on Unix;
- behavior when the slave closes;
- build and packaging impact.

Use the smallest viable alternative if it obstructs process ownership, shutdown semantics, or event-loop integration.

### 2.4 Ghostty binding evaluation

`libghostty-rs` is the first Rust-wrapper candidate. Adoption depends on:

- compatibility with the pinned Ghostty revision;
- API coverage for terminal, render, key, mouse, selection, paste, and effects;
- maintenance activity;
- ability to contain unsafe lifetime rules;
- build-script behavior;
- static and dynamic linking behavior;
- macOS and Linux CI reliability.

Direct FFI behind a small safe wrapper remains acceptable when it gives a more controlled dependency and build story.

### 2.5 Host event backend evaluation

Compare Crossterm, Termwiz, or another justified Ratatui-compatible event backend for:

- modified keys;
- application cursor keys;
- alternate key identities;
- Unicode text input;
- key repeat/release where available;
- bracketed paste;
- focus events;
- mouse press, drag, release, motion, and wheel events;
- Kitty keyboard enhancement reporting;
- macOS and Linux behavior in common outer terminals.

Do not choose a backend solely because it is Ratatui's most common example.

### 2.6 Native build and packaging validation

Prove clean contributor builds and packaged application binaries for:

- macOS arm64;
- macOS x86-64 when retained in the packaging strategy;
- Linux x86-64;
- Linux arm64 when practical.

Contributor builds may require Zig. Distributed binaries must not require end users to install Zig or a matching Ghostty library.

### 2.7 Output-session scheduling choice

Candidate approaches include:

- dedicated async tasks with blocking-reader adapters;
- one reader thread per stream feeding bounded channels into a per-run actor;
- platform evented I/O where it materially simplifies behavior.

Choose the smallest reliable model that satisfies the specification's fairness, backpressure, continuous-drain, and control-plane responsiveness requirements.

---

## 3. Milestones and risk-gated sequencing

The milestones prioritize risk reduction and usable vertical slices. Do not build the full supervisor before proving the terminal and process-runtime boundaries.

Every milestone is prototype work. The plan does not set a release boundary, supported-product platform list, or release date. Those decisions require evidence from the completed prototype and real-world validation.

### Milestone 0A — terminal boundary spike

Build a focused prototype that embeds one real PTY-backed shell through `libghostty-vt` inside a Ratatui region.

#### Required capabilities

1. Open a Ratatui application.
2. Spawn a shell in a PTY.
3. Continuously read PTY output.
4. Feed bytes into `libghostty-vt`.
5. Render visible terminal cells into a Ratatui region.
6. Render cursor position, visibility, and supported shape.
7. Forward keyboard input through Ghostty encoding.
8. Handle terminal effect write-back to the PTY.
9. Forward focus events where supported.
10. Paste using Ghostty-safe encoding.
11. Resize both Ghostty and the PTY.
12. Scroll through Ghostty scrollback.
13. Select with mouse gestures.
14. Copy with soft-wrap unwrapping.
15. Exercise child mouse reporting and the selection override.
16. Continue draining output while the pane is unfocused.
17. Bound output/input queues.
18. Perform idle scrollback compression if required by the API.
19. Produce a clean packaged build on macOS and Linux development environments.

#### Rendering tests

- 16/256/truecolor;
- bold, dim, italic, underline, inverse where supported;
- primary and alternate screens;
- wide characters;
- combining characters;
- wrapping and reflow;
- cursor hide/show;
- rapid resize.

#### Input tests

- text;
- Enter, Tab, Backspace, Escape;
- arrows and application cursor mode;
- Home/End/PageUp/PageDown;
- function keys;
- Ctrl/Alt/Shift/Super combinations where meaningful;
- key repeat;
- Kitty keyboard negotiation where available;
- bracketed paste;
- focus in/out;
- SGR mouse press/release/motion/wheel.

#### Selection tests

- single/double/triple click;
- drag across wrapped lines;
- drag into scrollback;
- autoscroll during drag;
- selection while output continues;
- selection after resize/reflow;
- alternate-screen entry/exit;
- clipboard failure handling.

#### Backpressure tests

- PTY emits sustained high output;
- user input remains responsive;
- terminal query responses are not dropped;
- queue saturation behavior is visible and bounded;
- no output deadlock occurs when the view is unfocused.

#### Packaging tests

- pinned Ghostty revision;
- documented Zig/build prerequisites for contributors;
- packaged artifact does not require Zig at runtime;
- clean build from a fresh checkout;
- license inventory;
- measured binary size and startup impact.

#### Deliverables

- minimal `TerminalSession` adapter;
- minimal Ratatui terminal widget/view;
- selected event backend and rationale;
- selected Ghostty binding strategy and rationale;
- selected PTY transport candidate;
- documented unsupported/partial capabilities;
- manual test checklist and results;
- build/distribution note;
- go/no-go recommendation.

#### Exit criterion

Proceed only if the prototype demonstrates that a modern interactive terminal experience can be embedded without building a second terminal emulator, a large custom renderer framework, or unsafe cross-thread state sharing.

### Milestone 0B — process ownership and shutdown spike

Build a headless process-runtime prototype independent of the full TUI.

#### Ownership seam to prove

Give each Run one owner for its Process Tree, process I/O, output drains,
sampler, and optional terminal session. The owner must control the complete
shutdown order. Callers use semantic interrupt, terminate, kill, resize, and
wait operations. They do not coordinate raw operating-system handles or stop
the terminal and Process Tree as separate peer objects.

Keep terminal semantics inside `TerminalSession`. Do not move Process Tree
containment or shutdown policy into it. Milestone 0B must prove the interface
between these two modules before Milestone 1 uses it.

#### Required capabilities

1. Spawn pipe-mode and PTY-mode processes.
2. Establish owned process group/session semantics on macOS and Linux.
3. Return root PID and process-tree identity.
4. Continuously drain output.
5. Resize PTY.
6. Interrupt, terminate, and kill through semantic operations.
7. Escalate after configured timeouts.
8. Clean up common child/grandchild trees.
9. Sample aggregate CPU and memory.
10. Distinguish intentional stop from unexpected exit.
11. Cancel readers, writers, and samplers without leaks.

#### Test commands

- direct executable;
- shell wrapper that `exec`s child;
- shell wrapper that does not `exec` child;
- process spawning grandchildren;
- process ignoring interrupt;
- process ignoring terminate;
- process exiting during escalation;
- high-output process;
- PTY interactive shell.

#### Deliverables

- one Run ownership module with a small semantic interface;
- `ProcessIo` abstraction;
- `ProcessTree` abstraction;
- macOS/Linux implementation notes;
- PTY-library assessment;
- containment limitations;
- verified Run shutdown ordering for pipe and PTY modes;
- cleanup and metrics test results;
- go/no-go recommendation.

Evidence recorded in [run ownership evidence](run-ownership-evidence.md).

### Milestone 1 — first integrated vertical slice

Combine the proven terminal and runtime boundaries into a small usable supervisor.

Implement:

- YAML version 1 parsing;
- direct `command` and `shell` command specs;
- services and oneshots;
- enabled/autostart/desired-state model;
- dependency graph and cycle detection;
- dependency conditions `started`, `ready`, and `completed_successfully`;
- HTTP and TCP readiness;
- immediate readiness when no probe is configured;
- process start/stop/restart/rerun;
- `RunId` on asynchronous events;
- bounded per-process output history and per-Run terminal sessions;
- process list plus selected terminal view;
- pipe and PTY mode;
- focus/leader behavior;
- scroll/follow;
- mouse selection and copy;
- controlled project shutdown;
- basic structured failure and blocked diagnostics.

At the end of Milestone 1, the prototype should manage the synthetic Quadrant-like fixture and be usable enough to test with a small real project.

Evidence and the macOS-scoped go recommendation are recorded in [Milestone 1 validation](milestone-1-validation.md).

### Milestone 2 — lifecycle hardening

Add and test:

- full normative lifecycle transition table;
- `exited` dependency condition;
- exec and log readiness probes;
- composite `all` probes;
- startup timeout with process termination;
- liveness;
- restart policy and restart exhaustion;
- success exit codes;
- stale event/race tests;
- dependency recovery after failed prerequisite rerun;
- richer probe diagnostics;
- cancellation of all run-scoped tasks.

Evidence and the macOS-scoped go recommendation are recorded in
[Milestone 2 validation](milestone-2-validation.md).

### Milestone 3 — declarative Projects and real-world macOS use

Make Stackhand useful for daily development with one complex real Project.
Use Quadrant to validate the generic product model. Do not add
Quadrant-specific behavior to Stackhand.

Add:

- base Project discovery from the current directory and its parents;
- explicit Project paths that disable base-file discovery;
- local override discovery;
- deterministic merge rules;
- environment files and inline environment;
- path-resolution rules;
- effective graph validation after merge;
- source-aware configuration diagnostics.

Create and exercise a representative Project configuration in
`alyzenmed/QUADRANT`. Use generic Stackhand features to replace startup scripts
that only coordinate Processes. The normal workflow should use:

- direct commands for Services and One-shots;
- HTTP, TCP, exec, log, and composite probes;
- Dependencies for startup ordering;
- One-shots for visible initialization and smoke checks;
- restart rules and controlled Project shutdown;
- environment files and inline environment values;
- explicit shell configuration only when a command requires a shell.

Keep Quadrant-specific work in Quadrant commands or scripts. Stackhand must not
gain built-in knowledge of Docker Compose, Azure Functions, Service Bus,
Azurite, Cosmos, or other parts of the validation Project. Running one of those
tools as an ordinary command does not make it part of the Stackhand model.

Prototype bounded lifecycle hooks only if the real Project shows a generic
lifecycle need that a Service, One-shot, Dependency, probe, restart rule,
Process Profile, or local override cannot represent cleanly. If hooks are necessary,
bound their runtime and output, and add visible Run markers.

Milestone 3 is complete when:

- a developer can start the normal local Quadrant workflow with Stackhand on
  the validated macOS host;
- that workflow does not call `emulators/run.sh` or `emulators/prepare.sh` for
  generic startup coordination;
- required initialization remains visible as One-shots;
- optional Processes can start without editing the base Project file;
- failed probes and blocked Dependencies identify the failed condition;
- stopping Stackhand cleans up its Process Trees, and supervised commands stop
  the external services that they started;
- direct commands do not depend on the user's login shell; and
- Quadrant-specific initialization remains outside the Stackhand core.

Keep the old helper scripts during initial validation. Simplify or remove them
only after the declarative workflow gives equivalent behavior.

### Milestone 4A — automated UX and observability foundation

Build and verify the behavior that does not require human judgment.

Add or improve:

- dedicated Logs/Search view;
- literal search and navigation;
- timestamps and stream distinction;
- project-wide memory budget;
- long-line and invalid-UTF-8 behavior;
- aggregate process-tree CPU and memory;
- output-flood tuning;
- history truncation UX;
- terminal scrollback compression tuning;
- keyboard selection mode;
- Process Profile controls and conditional Profile column;
- terminal/log copy polish.

Use deterministic fixtures, scripted input, terminal snapshots, synthetic
output pressure, resource measurements, and cleanup checks. Verify:

- search results and next/previous navigation;
- timestamps and stream labels;
- memory and line-length limits;
- invalid UTF-8 normalization;
- aggregate Process Tree metrics;
- control responsiveness during output flood;
- history truncation state and warnings;
- terminal scrollback compression;
- keyboard selection state transitions;
- future-Run Process Profile changes and affected-Process restarts;
- one-Run Start Anyway Dependency bypasses;
- conditional Profile column visibility;
- semantic state colors through terminal palette roles; and
- copied text across wrapped lines and continued output.

Milestone 4A is complete when the automated acceptance suite passes and no
known correctness or resource-bound defect remains in scope. It does not claim
that the UX is clear or comfortable for a human user.

### Milestone 4B — human daily-use UX validation

Have a human use Stackhand during representative Quadrant development
sessions. Validate:

- whether Terminal and Logs views are easy to understand and switch;
- whether search is easy to discover and operate;
- whether the current match and navigation direction are clear;
- whether keyboard selection feels natural;
- whether copied text matches the user's intent;
- whether truncation and failure warnings are useful without excessive noise;
- whether metrics help without reducing console readability;
- whether control remains comfortable during noisy output; and
- whether the UI remains usable in narrow and small terminals.

Fix high-frequency friction found during these sessions when the fix stays
inside the accepted product model. A new configuration or lifecycle concept
requires separate review before implementation.

Milestone 4B is complete when the human validation record lists the scenarios,
observed friction, completed fixes, deliberate deferrals, and a human go or
no-go recommendation for continued daily use.

Do not add configuration or lifecycle concepts only to fix a Quadrant-specific
problem. Keep domain-specific work in Project commands and scripts.

### Milestone 5 — Linux portability and validation

Run the same lifecycle, terminal, configuration, and cleanup acceptance suite
on the selected Linux targets.

Confirm:

- pipe and PTY Process Tree ownership;
- signal and shutdown behavior;
- keyboard, mouse, paste, focus, and resize behavior;
- readiness, liveness, restart, and Dependency behavior;
- Process Profile selection, local override, environment, and path behavior;
- direct-command behavior with non-POSIX login shells;
- aggregate Process Tree metrics;
- clean contributor builds and packaged artifacts; and
- real Project startup and cleanup where the required services are available.

Fix Linux-specific defects found by this work. Do not describe Linux as
validated until the selected targets pass the applicable acceptance suite.

### Milestone 6 — Windows validation spike and implementation

Only after macOS/Linux behavior is stable:

- validate ConPTY integration;
- implement Job Object containment;
- map semantic shutdown actions;
- validate keyboard/mouse/paste/focus behavior;
- implement process-tree metrics;
- package native dependencies;
- run the same terminal and lifecycle acceptance suite.

Do not describe Windows as validated until it meets the same core user experience. A nominal spawn-only port is insufficient.

---
