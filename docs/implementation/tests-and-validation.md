# Tests and validation

[Back to the prototype implementation plan](../implementation-plan.md)

## 4. Test plan

### 4.1 Unit tests — configuration

- YAML schema parsing;
- unknown/invalid field diagnostics;
- direct command versus shell command validation;
- relative path resolution;
- environment precedence and null unsetting;
- profile order;
- local overlay precedence;
- map/scalar/list/null merge semantics;
- enable/disable/autostart interaction;
- enabled and autostart default values;
- ordered profile enable/disable conflicts;
- field-specific `null` behavior;
- explicit and parent-directory configuration discovery;
- dependency removal through overlay;
- duration and byte-size parsing;
- output budget validation.

### 4.2 Unit tests — graph and lifecycle

- dependency lookup;
- cycle detection after overlays;
- dependency condition satisfaction;
- disabled dependency behavior;
- desired-state transitions;
- start eligibility;
- immediate readiness without probe;
- successful oneshot session satisfaction;
- rerun invalidation behavior;
- service unexpected clean exit;
- `started` is not satisfied while the prerequisite is stopping;
- manual stop versus restart policy;
- one-shot `on_failure` and rejected one-shot `always` policy;
- automatic restart budget usage and reset;
- startup timeout transition;
- restart limits and counter reset;
- liveness failure;
- hook blocking/failure semantics;
- `after_start` and `after_ready` do not gate readiness or dependents;
- dependency recovery;
- project shutdown state;
- stale `RunId` event rejection.

### 4.3 Unit tests — probes

- threshold progression;
- initial delay;
- no overlapping attempts;
- timeout;
- cancellation;
- HTTP status classification;
- redirect policy;
- response body cap;
- TCP success/failure;
- exec success exit codes;
- exec timeout and cleanup;
- log match across chunks;
- log ANSI stripping;
- log current-run isolation;
- composite `all` state.

### 4.4 Unit tests — output

- raw ring-buffer truncation;
- logical-line truncation;
- global and per-process budget enforcement;
- invalid UTF-8 normalization;
- long unterminated line cap;
- stdout/stderr observation sequencing;
- run boundary markers;
- search next/previous;
- search cancellation;
- follow-mode transitions;
- coalesced dirty notification;
- queue saturation behavior.

### 4.5 Integration tests — runtime

- pipe-mode service starts and exits;
- PTY-mode shell starts and resizes;
- child/grandchild cleanup;
- interrupt/terminate/kill escalation;
- process ignores interrupt;
- process exits during timeout race;
- output continuously drains while unselected;
- writer partial-write handling;
- process-tree aggregate metrics;
- manual stop does not restart;
- project quit cleans up all owned trees.

### 4.6 Integration tests — supervisor

- autostart schedules dependencies;
- manual start schedules dependencies;
- service becomes ready through TCP;
- service becomes ready through HTTP;
- service becomes ready through log match;
- oneshot waits for ready service;
- dependent waits for successful oneshot;
- failed oneshot blocks dependent;
- rerunning failed oneshot unblocks dependent on success;
- disabled dependency remains blocked;
- startup timeout stops process tree;
- stale readiness success from prior run is ignored;
- stale process exit from prior run is ignored;
- restart backoff is cancellable by manual stop;
- `on_failure` runs once per failed attempt;
- shutdown hooks obey shared project deadline.

### 4.7 Terminal integration tests

Where practical, automate or provide deterministic fixtures for:

- ANSI colors/styles;
- alternate screen;
- resize/reflow;
- wide and combining Unicode;
- cursor state;
- application cursor mode;
- key modifiers;
- bracketed paste;
- focus reporting;
- mouse reporting;
- scrollback;
- selection across wrapped lines;
- selection formatting;
- selection under mutation;
- terminal effect write-back;
- scrollback compression scheduling.

Some nested-terminal behavior will require manual/platform matrix testing in addition to automated tests.

### 4.8 TUI tests

Use Ratatui `TestBackend` where useful for:

- process row statuses;
- waiting/blocked/failure diagnostics;
- layout and zoom;
- focus transitions;
- leader command routing;
- logs/search view;
- help/modal rendering;
- metrics column collapse;
- narrow terminal behavior.

Avoid brittle snapshots of dynamic terminal cell content when focused behavioral tests are clearer.

### 4.9 Race and model tests

Use deterministic or property-based testing for event ordering such as:

- readiness succeeds as process exits;
- manual restart while probe is in flight;
- stop during spawn;
- stop during before-start hook;
- stop while waiting does not stop already scheduled dependencies;
- project shutdown during restart backoff;
- old metrics after new run starts;
- output-session failure after process exit;
- dependency success and dependent manual stop in either order.

The supervisor should be testable with a fake clock and fake runtime so these races do not depend on wall-clock sleeps.

### 4.10 Stress and soak tests

- sustained high-output producer for an extended run;
- many low-output services;
- repeated restarts;
- continuous resize events;
- selection and copy under output;
- global history eviction across many processes;
- memory remains bounded;
- no task/channel count grows over repeated runs;
- responsive controlled shutdown.

### 4.11 Human daily-use validation

Run Milestone 4B only after the Milestone 4A automated suite passes. Have a
human use the complete workflow with representative quiet, noisy, failing, and
interactive Processes. Record:

- whether modes and actions are easy to discover;
- whether search, selection, and copy match the user's intent;
- whether warnings and metrics are clear without excessive noise;
- whether Terminal and Logs views support real diagnostic work;
- friction that requires a fix before continued daily use; and
- deliberate deferrals with their reasons.

Automated tests can prove correctness, bounds, and deterministic interaction
behavior. They cannot prove that the UX is clear or comfortable for a human
user. The human validation record must give a go or no-go recommendation.

---

## 5. Prototype validation criteria

The prototype should satisfy all of the following before a later release decision is considered:

1. A user can define multiple services and oneshots in YAML.
2. Direct commands and shell commands have unambiguous behavior.
3. A service can wait for another service to become ready.
4. A service can wait for a oneshot to complete successfully.
5. HTTP and TCP readiness checks work reliably.
6. A service without readiness configuration becomes ready immediately after spawn.
7. Failed prerequisites produce clear blocked reasons.
8. Rerunning a failed prerequisite can automatically unblock a still-desired dependent.
9. Start, stop, restart, and oneshot rerun work from the TUI.
10. Manual stop never triggers automatic restart.
11. Stale asynchronous results from prior runs cannot change current state.
12. The selected process has a large readable terminal/console.
13. Pipe-mode and PTY-mode processes are both supported.
14. An interactive shell, pager, and editor can receive useful keyboard input.
15. Resizing/zooming updates the child PTY and terminal state correctly.
16. Unfocused processes continue draining output and do not deadlock.
17. Output flood does not freeze lifecycle controls or interactive input.
18. Terminal output can be scrolled without stopping ingestion.
19. Retained output can be searched.
20. Text can be selected with the mouse.
21. Selected terminal text copies without soft-wrap corruption.
22. Child mouse reporting and outer selection override behave predictably.
23. Process-tree CPU and memory are visible.
24. Output memory is bounded per process and project-wide.
25. History truncation is visible rather than silent.
26. Project shutdown reliably attempts to clean up supervised process trees.
27. A gitignored local override can add machine-specific commands or helper processes.
28. Configuration errors are detected before any process starts.
29. Packaged prototype binaries do not require Zig at runtime.
30. The core contains no hand-written terminal emulator or custom general TUI framework.

---

## 6. Implementation guardrails

Implementation work should follow these constraints unless concrete evidence justifies a documented specification or plan change:

1. **Do not reimplement terminal emulation.** Use `libghostty-vt`.
2. **Do not reimplement a general TUI framework.** Use Ratatui.
3. **Do not route every output chunk through the supervisor lifecycle queue.**
4. **Do not let PTY-library kill behavior define product shutdown semantics.**
5. **Do not add Docker, SSH, cloud, VPN, or emulator-specific process kinds.**
6. **Do not hide required initialization in hooks when a visible oneshot dependency is appropriate.**
7. **Do not allow hooks to become indefinitely running hidden services.**
8. **Do not use unbounded output, input, effect, probe, or hook buffers.**
9. **Do not tie output draining to TUI focus or rendering.**
10. **Do not allow Ratatui widgets to own authoritative process state.**
11. **Do not mutate one Ghostty terminal concurrently from UI and output tasks.**
12. **Do not return long-lived borrowed FFI render/selection data to the UI.**
13. **Do not silently drop interactive input.**
14. **Do not blindly grant terminal OSC clipboard access.**
15. **Do not build a daemon, API, web UI, or plugin system before the local TUI is excellent.**
16. **Do not over-design profile inheritance or list merge operators.**
17. **Move generic Process coordination into configuration when the existing model represents it cleanly. Keep domain-specific work in Project commands or scripts.**
18. **Do not steal Ctrl-C from a focused interactive child.**
19. **Do not treat process spawn as equivalent to readiness when a readiness probe exists.**
20. **Do not treat a service exit as successful oneshot completion.**
21. **Do not automatically cascade dependency failures through already-running services in MVP.**
22. **Do not accept asynchronous state changes without validating `RunId`.**
23. **Do not claim process containment is a security boundary.**
24. **Do not advertise Windows until ConPTY, Job Object, input, and shutdown behavior are validated.**

---
