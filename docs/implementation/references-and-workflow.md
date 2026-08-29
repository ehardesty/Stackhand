# References and workflow

[Back to the prototype implementation plan](../implementation-plan.md)

## 11. Source references

Inspect upstream source directly rather than relying only on the companion specification or this plan.

### 11.1 dekit / mprocs

Repository:

```text
pvolok/dekit
```

Useful areas:

```text
src/console/
src/term/
src/term_driver/
src/process/
src/task/
```

Important historical point:

```text
commit 2c3a73e24a15502a1727e4b033312e6b9805d45e
"Replace ratatui with manual rendering"
```

Study this primarily to understand the terminal/UI complexity this project should avoid owning through Ratatui + Ghostty.

### 11.2 Process Compose

Repository:

```text
F1bonacc1/process-compose
```

Useful areas:

```text
src/types/process.go
src/health/
src/app/process.go
src/app/project_runner.go
src/tui/
```

Concepts worth studying:

- dependency conditions;
- readiness/liveness probes;
- restart and availability settings;
- process lifecycle and health separation;
- CPU/memory reporting;
- successful-completion dependency semantics.

Do not assume its TUI architecture should be copied.

### 11.3 Ghostty / `libghostty-vt`

Repository:

```text
ghostty-org/ghostty
```

Primary public API headers:

```text
include/ghostty/vt.h
include/ghostty/vt/terminal.h
include/ghostty/vt/render.h
include/ghostty/vt/selection.h
include/ghostty/vt/key.h
include/ghostty/vt/mouse.h
include/ghostty/vt/paste.h
include/ghostty/vt/focus.h
include/ghostty/vt/snapshot.h
```

Inspect effect callbacks, render snapshots, selection gesture lifetime rules, scrollback compression, key/mouse encoding, paste, and terminal write-back behavior.

### 11.4 Ghostling

Repository:

```text
ghostty-org/ghostling
```

Ghostling demonstrates the intended boundary:

```text
embedder owns PTY + event loop + renderer
libghostty owns virtual terminal semantics
```

Use it as a minimal integration reference, not as a production architecture to copy wholesale.

### 11.5 Rust bindings

Candidate repository:

```text
Uzaaft/libghostty-rs
```

Inspect:

- safe wrapper coverage;
- binding generation/pinning;
- build script behavior;
- Zig requirements;
- static/dynamic linking;
- selection and render lifetimes;
- maintenance status.

Direct FFI remains acceptable behind a project-owned safe adapter.

### 11.6 Ratatui and event backends

Use Ratatui for:

- layout;
- process list;
- borders;
- footer/help;
- search input and dialogs;
- display of owned Ghostty render snapshots.

Evaluate host event backends rather than assuming one automatically preserves the input fidelity needed by nested terminal applications.

### 11.7 PTY/process runtime

Evaluate mature PTY crates such as `portable-pty`, but inspect actual spawn, resize, PID, process-group, and kill behavior.

Do not infer semantic process-tree control from a generic `Child::kill` API.

### 11.8 Quadrant reference use case

Repository:

```text
alyzenmed/QUADRANT
```

Important files:

```text
mprocs.yaml
QUADRANT.AppHost/AppHost.cs
scripts/local-dev/emulators/run.sh
scripts/local-dev/emulators/prepare.sh
scripts/local-dev/emulators/forward-remote.sh
docker-compose.local.yml
docs/dev/local-development-prerequisites.md
```

Use these to validate realistic orchestration without introducing
Quadrant-specific concepts into the core. Move startup ordering, probes,
restart rules, environment loading, and shutdown coordination into generic
configuration where the model represents them cleanly. Keep resource creation,
application initialization, and application validation in Quadrant commands or
scripts.

---

## 12. Working instructions for implementation agents

Treat the companion specification as the normative product and architecture contract. Treat this document as the current execution plan, not as a request to implement every section in one change.

Preferred working pattern:

1. Inspect the referenced upstream source.
2. Write a concise architecture note listing any evidence-based conflict with this specification.
3. Execute Milestone 0A, the terminal-boundary spike.
4. Report concrete results before committing to the terminal adapter, event backend, or Ghostty binding.
5. Execute Milestone 0B, the process-ownership spike.
6. Report concrete cleanup, metrics, and PTY-runtime results.
7. Build the first vertical slice with lifecycle tests and a synthetic Quadrant-like fixture.
8. Keep changes small enough to review and validate continuously.
9. Use a fake runtime and fake clock for supervisor state-machine tests.
10. When considering a new first-class concept, first determine whether it can be represented by:
    - an ordinary service;
    - an ordinary oneshot;
    - a readiness/liveness probe;
    - a dependency condition;
    - a bounded hook;
    - a local override;
    - an existing script.
11. Prefer a generic bounded escape hatch over a domain-specific subsystem.
12. Treat terminal/output quality, race safety, process-tree cleanup, and bounded memory as prototype-validation requirements rather than deferred polish.
13. Record any deliberate deviation from the fixed decisions in this plan, and update the companion specification when the deviation changes a normative contract.

---

## 13. Change history

### Revision 4 — 2026-08-29

Split Milestone 4 into automated implementation and verification in Milestone
4A, followed by real human-use validation and evidence-based UX fixes in
Milestone 4B.

### Revision 3 — 2026-08-28

Moved real-world Quadrant use into Milestone 3. Made Quadrant a validation case
for generic declarative Projects rather than a source of product-specific
features. Moved full Linux portability and validation into Milestone 5.

### Revision 2 — 2026-08-24

Made the repository copy canonical and renamed the project to Stackhand. Marked all milestones as prototype work with no release boundary. Reconciled Process terminology, per-Process Logs history, per-Run terminal state, restart-budget semantics, hook ordering, startup Dependency behavior, configuration discovery, overlay rules, and Project actions with the north-star specification.

### Revision 1 — 2026-08-24

Created as the flexible prototype companion to the north-star product specification.

The split moved the following ephemeral material out of the north-star specification:

- macOS/Linux-first and Windows-follow-up sequencing;
- terminal-boundary and process-ownership spikes;
- package/module starting shape;
- PTY, Ghostty binding, host event backend, and packaging evaluations;
- vertical-slice milestones;
- executable test plan;
- prototype validation criteria;
- implementation guardrails;
- current technical risks;
- fixed and deferred implementation decisions;
- recommended first task;
- source-reading assignments;
- coding-agent workflow.

The companion specification retains durable product behavior, lifecycle semantics, configuration contracts, ownership boundaries, performance requirements, security policy, and the definition of success.
