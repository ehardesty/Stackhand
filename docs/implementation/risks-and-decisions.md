# Risks and decisions

[Back to the prototype implementation plan](../implementation-plan.md)

## 7. Highest-risk technical questions

Answer these with evidence before committing deeply to the affected implementation boundary.

### 7.1 Ratatui + Ghostty rendering

- Is render-state traversal straightforward and fast enough?
- Can owned row/cell snapshots be produced without spreading FFI types?
- How are grapheme clusters, wide cells, combining characters, and cursor overlays represented?
- Can dirty-row updates materially reduce work without complicating correctness?
- What visual features cannot be represented by the outer terminal/Ratatui backend?

### 7.2 Nested-terminal input fidelity

- Which Ratatui-compatible event backend preserves the best keyboard data?
- Can Kitty keyboard enhancements be observed and translated correctly?
- What is lost for non-US layouts, composed input, key release, or physical key identity?
- Do bracketed paste, focus, and mouse protocols survive the outer-to-inner path?
- What fallback behavior is acceptable and documented?

### 7.3 Ghostty selection

- Can gesture APIs map cleanly from host mouse coordinates?
- Can tracked references preserve selection while output mutates or scrollback compresses?
- How should alternate-screen transitions affect selection?
- Does copy formatting exactly satisfy soft-wrap behavior?
- Can keyboard selection reuse Ghostty adjustment APIs cleanly?

### 7.4 Ghostty effects and PTY write-back

- Which effects are required for common terminal applications?
- How should synchronous callbacks enqueue writer work without blocking?
- What queue size and saturation behavior are appropriate?
- Which clipboard, title, PWD, progress, and notification effects should be surfaced?

### 7.5 Build and distribution

- How disruptive is Zig for contributors?
- Can Ghostty source and package dependencies be pinned for offline/reproducible builds?
- Is static or dynamic linking preferable for packaged artifacts?
- Can macOS universal or separate architecture builds be produced reliably?
- What upgrade cadence and compatibility tests are required?

### 7.6 PTY and process ownership

- Does the selected PTY crate expose enough information without dictating shutdown?
- How reliably can process groups/sessions contain common wrappers?
- How are child trees discovered for metrics?
- What edge cases leave descendants behind?
- Can the runtime cleanly support ConPTY later?

### 7.7 Output duplication and memory

- What minimum duplicate representation is required for terminal state, raw history, and searchable logs?
- How much memory does Ghostty scrollback use under representative output?
- How effective is incremental compression?
- Should raw history or normalized history be optional for some processes?
- What global defaults remain safe for 10–30 processes?

### 7.8 Supervisor race safety

- Can every task/event be associated with a `RunId`?
- How are spawn cancellation and stop-during-spawn represented?
- How are hooks and process cleanup sequenced under shutdown?
- Can a fake runtime and fake clock exercise all important interleavings?

The preferred answer format is a small proof of concept and measured behavior, not speculative architecture.

---

## 8. Decisions fixed for the current plan

The following should be treated as current project decisions:

1. macOS and Linux are the initial prototype platforms; Windows follows dedicated validation.
2. Ratatui owns application UI.
3. `libghostty-vt` owns terminal semantics.
4. Ghostty integration is isolated behind an internal adapter.
5. Services and oneshots are the only initial process kinds.
6. Desired state is separate from observed lifecycle state.
7. Readiness and health are orthogonal.
8. A service without readiness configuration is immediately ready after spawn.
9. Dependency conditions are `started`, `ready`, `exited`, and `completed_successfully`.
10. Dependencies gate startup and do not imply lifetime cascading in the prototype baseline.
11. Every run has a `RunId`; stale asynchronous events are ignored.
12. High-volume output bypasses the supervisor lifecycle queue.
13. Each Process has bounded multi-Run output history and a serialized terminal session for its current Run.
14. Terminal and Logs/Search views are distinct.
15. PTY allocation and interactive input policy are distinct.
16. PTY transport and process-tree ownership are distinct abstractions.
17. Shutdown is configured through semantic actions and escalation.
18. Direct command arrays and shell strings have separate schema fields.
19. Overlay merge rules are deterministic: deep map merge, scalar/list replacement, `null` clearing.
20. Base < selected profiles in order < local override < CLI overrides.
21. Per-process and project-wide output memory are bounded.
22. Mouse ownership changes when a child enables tracking; an explicit override restores outer selection.
23. OSC clipboard access is denied or consent-based by default.
24. The terminal and process-ownership spikes are hard gates before broad implementation.

---

## 9. Decisions intentionally deferred

These remain open and should be decided from spike evidence, testing, or early use:

- exact default config filename;
- exact leader key and command keymap;
- exact clipboard crate;
- Crossterm versus Termwiz or another Ratatui-compatible backend;
- `portable-pty` versus another PTY transport;
- direct Ghostty FFI versus `libghostty-rs`;
- exact history and scrollback defaults;
- whether raw byte history is always retained or configurable;
- exact HTTP redirect/TLS override schema;
- regex search;
- `any` composite probes;
- optional environment interpolation syntax;
- send-keys shutdown escape hatch;
- daemon/attach mode;
- richer lifetime dependency policies;
- plugin model;
- graphics protocol support;
- timing for possible Windows validation.

Deferred decisions must not be answered accidentally through undocumented implementation behavior.

---

## 10. Recommended first implementation task

Create a focused terminal-boundary spike in a temporary branch or isolated module with this goal:

> Render and interact with a real PTY-backed shell through `libghostty-vt` inside a Ratatui pane, including resize, terminal write-back effects, scrollback, child mouse mode, outer selection override, live-output selection, paste, and correct copy formatting.

The spike should contain:

- a minimal `TerminalSession` owner;
- a minimal Ratatui terminal view;
- one chosen host input backend;
- one chosen PTY transport;
- one Ghostty binding strategy;
- bounded input/effect queues;
- a simple output-flood fixture;
- a manual matrix for shell, ANSI output, editor, pager, `tmux`, resize, selection, mouse, paste, and high output;
- measured build/package friction;
- a concise architecture note identifying any conflict with the companion specification.

If the spike succeeds, execute the process-ownership spike before constructing the complete supervisor.

If either spike fails materially, reassess the relevant boundary before building a large codebase around it.

---
