# Terminal and output

[Back to the product specification](../product-specification.md)

## 18. `libghostty-vt` integration

### 18.1 Responsibility boundary

Use `libghostty-vt` for:

- VT/ANSI parsing;
- primary and alternate screens;
- cursor state and supported cursor shapes;
- colors and text styles;
- scrollback;
- wrapping and reflow;
- key encoding;
- application cursor-key behavior;
- Kitty keyboard protocol behavior supported by the host event source;
- mouse encoding and tracking modes;
- selection semantics;
- soft-wrap-aware copy formatting;
- terminal query responses;
- title and working-directory effects;
- scrollback compression support.

The application owns:

- PTY creation;
- process lifecycle;
- host event collection;
- application/child input arbitration;
- rendering Ghostty cells through Ratatui;
- clipboard policy;
- effect dispatch;
- synchronization and scheduling.

### 18.2 Internal adapter

No module outside `terminal/` should depend directly on a broad set of Ghostty FFI types.

Illustrative abstraction:

```rust
trait TerminalEngine {
    fn write(&mut self, bytes: &[u8]) -> Result<TerminalEffects>;
    fn resize(&mut self, size: TerminalGeometry) -> Result<()>;
    fn render_snapshot(&mut self) -> Result<TerminalRenderSnapshot>;
    fn encode_key(&mut self, event: HostKeyEvent) -> Result<EncodedInput>;
    fn encode_mouse(&mut self, event: HostMouseEvent) -> Result<EncodedInput>;
    fn scroll(&mut self, request: ScrollRequest) -> Result<()>;
    fn selection_command(&mut self, command: SelectionCommand) -> Result<()>;
    fn selection_text(&mut self) -> Result<Option<String>>;
}
```

The actual API MAY differ from the illustration. The durable requirement is a narrow, owned Rust boundary.

### 18.3 Single serialized terminal owner

Terminal writes, render-state updates, selection gestures, scrolling, and resize operations must be serialized for each active terminal session.

A per-run output actor/task is the preferred owner. The UI requests owned snapshots or sends commands; it does not mutate Ghostty state concurrently.

### 18.4 Effect callbacks

Ghostty effects such as PTY write-back, title changes, clipboard requests, and device queries occur during terminal writes. They must remain nonblocking and non-reentrant.

Effect handlers should enqueue small typed actions for later processing. They must not:

- block on clipboard UI;
- perform long I/O;
- call terminal write recursively;
- hold broad application locks.

PTY write-back bytes use the same bounded input writer as user input and must not be silently dropped.

### 18.5 Render snapshots

`TerminalRenderSnapshot` should contain owned or safely reference-counted Rust data sufficient for Ratatui rendering:

- visible rows/cells;
- grapheme/codepoint content;
- style attributes;
- effective colors;
- cursor position, visibility, and shape;
- selection highlighting;
- viewport/scrollbar state;
- revision number or dirty-row information.

Do not expose borrowed FFI references after releasing the terminal owner's serialization boundary.

### 18.6 Binding boundary

The Ghostty integration may use maintained Rust bindings or direct C FFI, but it MUST remain behind a project-owned safe adapter.

The adapter must:

- pin compatibility with a known Ghostty revision;
- expose the terminal, render, key, mouse, selection, paste, and effect capabilities required by this specification;
- contain unsafe lifetime and allocation rules;
- prevent borrowed FFI data from escaping the serialized terminal-owner boundary;
- allow the binding strategy to change without propagating through the application.

### 18.7 Native build and packaging contract

The project must pin:

- Ghostty source revision;
- binding/wrapper revision when one is used;
- supported Zig version;
- native build options.

Contributor builds MAY require Zig. Distributed application binaries MUST NOT require users to install Zig at runtime or install a matching Ghostty library manually.

Build and packaging details belong in the companion implementation plan; this document defines only the reproducibility and end-user requirements.

### 18.8 Scrollback compression

If Ghostty scrollback compression is caller-driven, each terminal session must schedule bounded incremental compression after terminal activity becomes idle. Compression work must not block output draining or UI input.

---

## 19. Terminal capability policy

This application embeds a terminal emulator inside another terminal. It should provide excellent behavior, but it cannot assume the outer terminal and host event backend expose every native-terminal capability.

### 19.1 Required capabilities

- ANSI colors and common text styles;
- 256-color and truecolor rendering where representable;
- primary and alternate screens;
- cursor position and visibility;
- supported cursor shapes;
- wide characters;
- combining characters;
- resize and reflow;
- application cursor-key mode;
- bracketed paste;
- focus reporting where available;
- mouse tracking and SGR mouse encoding;
- scrollback;
- selection and correct formatting/copy;
- terminal query responses needed by common applications.

### 19.2 Best effort

- uncommon underline styles;
- hyperlinks;
- cursor-shape fidelity through every outer terminal;
- non-US physical-key identity;
- composed/IME input depending on host backend;
- key release and repeat information;
- Kitty keyboard features not exposed by the outer terminal/backend.

### 19.3 Outside the core capability set

- Kitty graphics rendering;
- sixel;
- font shaping and ligatures;
- image placement inside arbitrary Ratatui regions;
- pixel-perfect native Ghostty parity;
- full IME guarantees on every platform;
- terminal audio or desktop integration beyond basic bell/status behavior.

Unsupported or filtered terminal sequences should fail safely and may be surfaced in debug diagnostics.

---

## 20. Host input, mouse ownership, selection, and clipboard

### 20.1 Host event requirements

The host event layer must preserve enough information to support nested terminal applications correctly:

- modified keys;
- application cursor keys;
- alternate key identities where available;
- Unicode text input;
- key repeat/release where available;
- bracketed paste;
- focus events;
- mouse press, drag, release, motion, and wheel events;
- Kitty keyboard enhancement reporting where exposed by the outer terminal.

The implementation MUST NOT select an event backend solely because it is the most common Ratatui example. Backend selection and platform validation belong in the companion implementation plan.

### 20.2 Focus and command arbitration

Two primary focus scopes exist:

- process list/application controls;
- console.

Within console focus, the application has explicit modes:

- child input mode;
- app command mode;
- scroll/history mode;
- selection mode;
- search/logs mode.

A centralized leader binding enters app command context from arbitrary interactive child programs. `Ctrl-C` must remain available to the child during ordinary console input mode.

### 20.3 Mouse ownership policy

| State | Mouse behavior |
|---|---|
| Child mouse tracking disabled | Drag selects terminal text; wheel scrolls terminal history. |
| Child mouse tracking enabled | Mouse events are encoded and forwarded to the child. |
| Selection override modifier held | Application selection wins even when child tracking is enabled. |
| App selection/scroll/search mode active | Application owns mouse. |
| Process list focused | Ratatui application owns mouse. |

Use a conventional override such as Shift by default. The binding SHOULD be configurable.

This policy must work with common mouse-aware applications such as `vim`/`nvim`, `tmux`, `less`, and SGR-mouse programs.

### 20.4 Scroll key ownership

When an interactive child is focused, PageUp/PageDown and ordinary keys belong to the child. To scroll outer terminal history, the user enters app command/scroll mode or uses an explicit mouse-wheel policy.

Do not silently intercept common child keys merely because they are useful for the supervisor.

### 20.5 Selection

Desired behavior:

- single drag: cell/linear selection;
- double click: word selection;
- triple click: line selection;
- autoscroll while dragging outside the viewport;
- optional rectangular selection later;
- keyboard endpoint adjustment;
- visually highlighted selection;
- selection remains coherent while output continues when supported by tracked-reference semantics.

Use Ghostty selection gesture and tracked grid-reference APIs rather than recreating terminal selection rules.

### 20.6 Keyboard selection mode

A keyboard-driven mode should support:

- enter selection mode;
- move/extend by cell;
- move by line/page;
- line beginning/end;
- select all;
- copy/yank;
- cancel.

Exact keys may evolve, but the mode must not require mouse use.

### 20.7 Copy semantics

Copied terminal text must:

- unwrap soft-wrapped rows;
- preserve real line breaks;
- trim terminal padding appropriately;
- preserve Unicode text;
- avoid artificial line breaks caused by current pane width.

Ghostty's selection formatter is the reference behavior.

### 20.8 Paste and input backpressure

Paste should:

- respect bracketed-paste mode;
- use Ghostty's paste validation/encoding where available;
- have a configurable maximum paste size or confirmation policy;
- flow through the bounded PTY writer queue;
- surface backpressure/errors instead of dropping data.

The host reader admits at most 64 KiB of raw paste and of decoded UTF-8 text.
An oversized paste is rejected atomically, with one warning. Its suffix stays
quarantined until the closing marker. A frame has a 30-second absolute lifetime
from its opener. Expiry ends host parsing permanently and starts controlled
Project cleanup. Late closing markers cannot resume child input. Ordinary escape
sequences are capped at 1 KiB; exceeding that cap also ends the session.

Parser storage holds at most 64 KiB of paste, five tentative delimiter bytes,
a 1 KiB sequence, and one 1 KiB read batch. Lossy conversion can temporarily use
three times the paste storage. The separate semantic queue admits 256 entries,
which can retain about 16 MiB of accepted paste plus event overhead.

### 20.9 Clipboard policy

- User-initiated copy to the system clipboard is allowed.
- Child OSC clipboard reads and writes are denied.
- User copies from Terminal and Logs use one session-owned clipboard service.
- `STACKHAND_CLIPBOARD=auto|native|osc52|off` selects the host backend.
- Remote auto mode submits OSC52 through the sole host writer, without native fallback.
- Copies above 64 KiB of UTF-8 are rejected before mutation. Selection survives errors.
- OSC52 submission does not confirm delivery to the physical clipboard.
- Clipboard contents are not logged.
- Terminal clipboard effects are dispatched outside Ghostty callbacks.

---

## 21. Output architecture

### 21.1 Process history and Run terminal sessions

Each Process owns bounded multi-run Logs history. Each Run owns a fresh terminal session:

```text
Process reader(s)
      │
      ▼
ProcessOutputHistory
  ├── bounded multi-run Logs history
  ├── run and hook markers
  └── bounded search data

RunTerminalSession
  ├── sequence and timestamp assignment
  ├── libghostty terminal state
  ├── live log-readiness matcher
  ├── selection state
  └── coalesced dirty notification
```

The Process history exists across Runs. A fresh terminal session is created before or atomically with process spawn so early bytes are not lost. It is finalized after readers reach EOF and the Run ends.

### 21.2 Raw output event

Illustrative internal type:

```rust
struct OutputChunk {
    process_id: ProcessId,
    run_id: RunId,
    seq: u64,
    observed_at: SystemTime,
    stream: OutputStream, // Stdout | Stderr | Pty | Hook(HookKind)
    bytes: Bytes,
}
```

This type belongs to the output data plane. It is not an ordinary supervisor event.

### 21.3 Pipe ordering

For separate stdout and stderr readers, exact originating-process interleaving cannot be reconstructed. Assign sequence numbers at ingestion so the displayed merged order is deterministic based on observation order.

The Logs view may retain stream identity. The terminal projection may merge streams according to observation order.

### 21.4 PTY ordering

PTY processes naturally provide one combined stream. Label it `Pty`; do not pretend stdout/stderr distinction remains available.

### 21.5 Terminal mutation

All incoming bytes are fed to `libghostty-vt` in observation order. Terminal effects generated during writes are enqueued for later handling.

The Run terminal session increments a render revision and coalesces UI notifications. A flood of many small chunks should normally cause one pending dirty notification, not one control event per chunk.

### 21.6 Hook output

Hook output uses the bounded Logs history and is tagged by hook kind and Run. It is not fed through the Run's terminal emulator.

### 21.7 Run boundaries

By default, a process's console history may retain several recent runs within the process-level memory budget. Run boundaries must be visibly marked.

A new run must not contaminate current-run log readiness matching or terminal state unless the product deliberately chooses to preserve terminal history across runs. The preferred behavior is:

- start a fresh Ghostty terminal state for each run;
- retain prior run output in bounded Logs history;
- optionally expose prior run terminal snapshots later if cheap.

This prevents old terminal modes, alternate-screen state, and cursor settings from leaking into a new process.

---

## 22. Terminal view and Logs view

Terminal and log history are related but not identical representations.

### 22.1 Terminal view

The Terminal view is the real `libghostty-vt` state for the active run:

- interactive input;
- cursor;
- alternate screen;
- mouse reporting;
- terminal scrollback;
- selection;
- accurate terminal copy;
- overwrite and carriage-return behavior;
- current terminal geometry.

It is the default console for PTY processes and may also render piped process output through Ghostty.

### 22.2 Logs view

The Logs view is a bounded line-oriented projection:

- literal search;
- next/previous match;
- timestamps;
- stdout/stderr/PTY/hook labels;
- run boundaries;
- a line filter (§22.6);
- invalid UTF-8 replacement policy;
- deterministic observation order.

Terminal control sequences (including colors and hyperlink metadata) are removed before retention and search. Filtering spans reader chunks and stays isolated per stream and Run. CRLF and bare carriage returns become line breaks.

It does not claim that each line corresponds to one terminal row. Carriage returns, cursor movement, alternate screens, and overwritten progress output may produce a different representation from Terminal view.

### 22.3 Search behavior

Search is a core feature and operates in Logs view.

Requirements:

- literal search;
- next and previous match;
- optional case sensitivity toggle if inexpensive;
- at most 1,000 matches, with newest matches retained first;
- bounded incremental search work with visible progress while older output is checked;
- match navigation disables follow mode;
- one action returns to live tail;
- search never blocks output ingestion;
- an index is built incrementally or on demand within the memory budget.

Regex is deferred unless it is trivial and safe to bound.

### 22.4 Navigation between views

The console's top border shows a tab for each available view, so the user can always see which view is shown and how to reach the other:

```text
┌ Terminal │ Logs  api · run 3 · Ready ─────────┐
```

The shown view is highlighted. The other view's tab is clickable; a click shows that view and focuses the console, as a click on its output does. Processes without a Run terminal show only the Logs tab. While a filter hides lines, the tab reads `Logs (filtered)`.

Entering search or filter may switch to Logs view. Returning to Terminal view restores the prior terminal viewport and interaction mode.

When a Process has no active Run terminal, show its retained Logs instead of an empty Terminal view. A later Run returns to the user's selected representation.

Do not implement fragile raw-history-to-terminal-grid coordinate mapping solely to make search results appear in the Terminal view.

### 22.5 Pipe processes

For ordinary pipe processes, the application may default to Terminal or Logs view based on user preference. The architecture should support both:

- Terminal view provides ANSI rendering and terminal-like scrolling/copying.
- Logs view provides streams, timestamps, and search.

### 22.6 Line filter

A Logs filter hides retained lines that do not match, like the VS Code Output panel and Debug Console filters:

- comma-separated terms;
- a term that starts with `!` excludes matching lines;
- a leading `\` makes the rest of a term literal, for example `\!important`;
- include terms are OR'd and any exclusion wins;
- case-insensitive literal substring matching per retained line, unlike case-sensitive search;
- Run markers and the history-limit notice always stay visible.

The filter previews while it is typed. Committing an empty filter shows every line. Filtering is bounded like search: one UI turn examines at most 512 KiB of unfiltered output, newest first, and shows progress while older output is checked. Search runs only on lines the filter shows, so hidden lines never use the 1,000-match limit. A held Logs page stays in place while its first line is still shown; otherwise it moves to the selected match or the live tail.

---

## 23. Memory limits and backpressure

### 23.1 Project and per-process budgets

The configured in-memory defaults are:

```yaml
settings:
  output:
    terminal_scrollback_bytes: 16MiB
    terminal_project_bytes: 128MiB
    per_process_history_bytes: 32MiB
    global_history_bytes: 256MiB
```

See [Output history](../configuration.md#output-history) for units, validation,
fixed terminal shares, page granularity, and memory-accounting boundaries.
Terminal shares reserve all possible PTY Processes before startup; opening a
Run does not shrink another terminal's history. Native memory measurements and
parsed output progress are separate metrics. Logs are shared in bounded
segments; pinned segments remain charged until the final reader releases them.
History is in memory; disk archiving and snapshot persistence are deferred.

### 23.2 Budget accounting

Count or conservatively account for:

- raw output bytes;
- normalized logical lines;
- search/index structures;
- terminal scrollback where the API permits measurement/configuration;
- retained prior-run history;
- hook output;
- pending writer data.

Do not assume that a 16 MiB raw history cap implies a 16 MiB total memory cost.

### 23.3 Eviction policy

- Evict oldest segments within each Process when its budget is reached.
- At the Project limit, trim the largest current Process history first; break ties by oldest output.
- Preserve small quiet histories where possible rather than allowing one noisy Process to erase them.
- Selection does not bypass the shared storage budget.
- If a paused Logs page expires, freeze only the visible page and label it expired. Explicit scrolling or following returns to retained output. Do not pin a full paused history outside its budget.
- Eviction must not stop reading from the OS.
- Search results pointing into evicted content are invalidated cleanly.
- The UI should indicate truncation, for example:

```text
Earlier Logs expired at the history limit; recent output continues
```

### 23.4 Long lines and binary output

A process may emit an arbitrarily long line or non-text bytes.

Requirements:

- enforce `max_logical_line_bytes`;
- split or truncate with an explicit marker;
- preserve raw bytes only within configured limits;
- use a documented lossy UTF-8 representation in Logs view;
- do not repeatedly allocate proportional to an attacker-controlled line length;
- terminal parsing remains bounded by Ghostty and application-level queue limits.

### 23.5 Output backpressure

The application must continuously drain OS readers. When internal history is full, evict history rather than stop reading.

Output flooding must not monopolize:

- supervisor control commands;
- keyboard input;
- process shutdown;
- probes;
- metrics;
- UI redraw.

Use batching, bounded work per scheduling turn, and coalesced notifications.

The outer terminal uses independently opened nonblocking descriptors. Inherited
stdin, stdout, and stderr flags are unchanged. A complete draw and cursor batch
is staged before its first OS write. The staging cap is 16 MiB. It accommodates
large styled hosts such as 500 columns by 200 rows. A larger encoded batch fails
the session before submission; there is no silent dimension reduction.

Ordinary host output applies backpressure without a frame deadline. SIGTERM,
SIGHUP, and fatal host-input faults interrupt output through poll intervals of
at most 10 ms, subject to scheduling. Once interrupted or failed, output cannot
resume ordinary frames. Partial transactions retain their exact suffix.

With healthy output, shutdown continues to show progress. Each shutdown frame
has a 100 ms write budget. A stalled shutdown frame ends rendering permanently
and cleanup continues without output. Host input is drained and discarded during
cleanup. Pending host input is also flushed at terminal finalization, so capture
reports and keys accumulated during cleanup are not passed to the invoking shell.
This cannot prevent input that the host sends after Stackhand has exited.

Project cleanup retains its 20-second deadline and five-second observation grace.
Raw mode remains active through cleanup. Local termios is restored before final
output waits. One shared 500 ms output budget covers a retained suffix, mode
restoration, and terminal reporting, in that order. If the suffix cannot finish,
restoration and report bytes are not injected into it. OS acceptance does not
prove that the host displays restoration. Scheduler, kernel, and existing
Supervisor joins are outside any hard realtime guarantee. On macOS, a controlling
session leader also waits for its terminal kernel queue within that same output
budget. If the queue remains nonempty at expiry, finalization discards that queue
with `TCOFLUSH` and records incomplete delivery. This happens only at session end,
never during ordinary drawing. It prevents Darwin's implicit terminal drain on
session-leader exit from adding an unbounded wait.

Interactive completion returns an explicit exit code. Regular-file stderr
reports are preserved. Terminal stderr uses private bounded output within the
same finalization budget. Interactive reports to redirected pipes and other
nonregular nonterminal sinks are omitted, because a portable independently opened
nonblocking sink is unavailable. Configuration and other preflight diagnostics
retain their existing CLI path.
Terminal reports use carriage return and line feed. Regular-file reports use
normal line feeds. Late restoration failures are appended
to regular-file reports after terminal finalization; terminal delivery remains
subject to the shared finalization budget.

### 23.6 Input backpressure

The process writer has a bounded queue. On temporary OS backpressure:

- retain queued bytes up to the limit;
- retry asynchronously;
- preserve order between user input and terminal responses;
- surface a visible input-backpressure warning when the queue remains saturated;
- reject or require confirmation for an oversized paste before partial delivery;
- never silently discard the remainder of a user paste or key sequence.

### 23.7 Scrollback compression

Incremental terminal scrollback compression is scheduled only during idle periods and subject to bounded work units. It yields immediately to new output and input.

---
