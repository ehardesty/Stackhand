# Terminal boundary evidence

This note records evidence from the first narrow terminal slice. It does not decide the full terminal prototype.

## User-visible result

Stackhand can open one real shell in a PTY-backed Ratatui pane. Basic text and Enter go to the shell through Ghostty key encoding. Shell output returns through Ghostty terminal parsing. A successful pane resize updates both terminal state and the PTY. A PTY resize failure stops the prototype with a clear error. `Ctrl-Q` stops the prototype and restores the outer terminal.

## Current boundary

- Ratatui owns the outer application pane.
- Crossterm supplies basic host events and outer terminal control.
- `portable-pty` owns PTY transport for this slice.
- Stackhand owns the serialized Ghostty terminal thread and the small Crossterm input and Ratatui render adapters.
- Stackhand uses `libghostty-vt` directly only inside `terminal/`. No borrowed Ghostty value leaves the terminal owner.
- Render data returned by the adapter is an owned buffer with owned cursor data. No borrowed Ghostty value leaves the adapter.
- Stackhand owns the shell handle separately from PTY transport. On exit, it stops and waits for the shell before it stops the terminal owner.
- Stackhand owns the terminal session loop, input adapter, and Ratatui render adapter. A bounded command gate, effect collector, PTY output queue, and PTY writer are all inside the Stackhand-owned boundary.

The current `Cargo.lock` pins `libghostty-vt` 0.2.1 and Ghostty commit `a887df42c56f6de86c0fe6da9c4eeca37931e083`. This binding version is required because 0.1.1 did not expose public selection gestures, tracked grid references, selection formatting, or clipboard policy callbacks. The pinned Ghostty revision requires Zig 0.15.2. Zig 0.16.0 does not build it.

## Automated evidence

The executable has a deterministic fixture mode for integration tests. It starts `/bin/sh` in a real PTY, sends fixture text and Enter through the same terminal adapter used by the application, and reads the expected echo from the owned rendered snapshot. This mode does not change the normal interactive path.

A second staged executable fixture proves the current rendering path. It checks:

- different 16-color, 256-color, and truecolor cells;
- bold, dim, italic, underline, and inverse styles;
- a wide character and a combining character without shifted cells;
- a visible steady bar cursor at the child-selected position;
- a hidden cursor on the alternate screen;
- restoration of primary-screen content after the alternate screen closes;
- soft-wrapped line reflow from 16 columns to 8 columns;
- valid final geometry after a burst of very small and large resize requests.

The interactive event loop keeps only the last pane geometry in a rapid resize burst. It applies that geometry after a 16 ms quiet period. A deterministic unit test proves this coalescing rule without wall-clock sleeps.

The owned render snapshot now contains cursor position, shape, and blink state. A missing cursor value means that the child cursor is hidden or outside the visible viewport. Ratatui owns cursor position and visibility during each draw. Stackhand maps the owned Ghostty cursor shape to the matching Crossterm shape after the draw.

Stackhand clears its dirty signal before it copies the owned render snapshot. The serialized terminal owner updates the buffer and cursor under one lock, then sets the signal. Output that arrives after the copy sets the signal again and requests a later snapshot.

The Stackhand owner checks both the Ghostty resize result and the PTY resize result. Either error becomes a terminal failure event. A deterministic test proves that a failed 42-by-12 PTY resize is visible to the application.

A third executable fixture records bytes inside a real PTY child. It proves:

- Enter, Tab, Backspace, Escape, navigation keys, function keys, Shift-Tab, Ctrl-Up, and `Ctrl-C` reach the child;
- normal cursor mode encodes Up as `CSI A`, while application cursor mode encodes it as `SS3 A`;
- a device-status query reply enters the queue before later user input;
- focus gained and focus lost reports keep their order after the user input;
- the child, and not an internal encoder call, observes all fixture bytes.

A fourth executable fixture fills terminal history, moves the viewport away from the live tail, and marks the terminal as unfocused. It then lets a real PTY child write 4,000 more lines while the fixture does not request render snapshots. One live-tail action returns to the final `producer-complete` marker. This proves that scroll state, focus state, and skipped UI redraws do not stop the terminal owner from draining the PTY.

The interactive prototype uses `Ctrl-A` to enter application command context. `PageUp` enters scroll mode. `PageUp` and `PageDown` then move by one visible page without matching or searching for text. Any scroll action disables follow mode. `f` sends one live-tail request and returns to child input mode. The footer shows `LIVE` or `NOT FOLLOWING` so key ownership and follow state are visible.

Stackhand requests a 64 KiB Ghostty scrollback memory target. Ghostty rounds this target to its page allocation size and to the minimum active-screen allocation. Its own PageList note says that allocation can slightly exceed the requested target. The current Stackhand adapter does not yet expose retained-row count, viewport position, or a Ghostty truncation event. The scroll-mode footer therefore reports the 64 KiB target, Ghostty page rounding, and the missing Ghostty truncation signal. It does not claim a strict line count or exact current use.

The current Stackhand adapter exposes line-delta scrolling but not Ghostty's direct top and bottom requests. Stackhand uses a large bounded delta for the live-tail action. Extreme `isize` deltas can overflow the pinned Ghostty revision, so the adapter limits deltas before they reach Ghostty. The fixture proves that the bounded live-tail request reaches the end of retained history.

A fifth executable fixture (`--fixture-paste`) sends normal and bracketed paste through a real PTY child. Stackhand validates each paste with Ghostty's `paste::is_safe` check before it queues the paste. The prototype accepts at most 64 KiB of paste text. Ghostty input encoding adds `ESC [200~` and `ESC [201~` only when the child has enabled bracketed-paste mode. A paste larger than the limit is rejected before any bytes are queued. A partial-write unit fixture limits each underlying writer call to two bytes and proves that the bounded writer retries until normal and bracketed paste bytes remain complete and ordered. A blocked-child phase fills the bounded path. All admitted paste requests remain owned for retry. The first saturated call rejects the complete paste before admission and produces a visible warning.

The 0.2.1 binding exposes caller-driven scrollback compression. Stackhand does not yet schedule its incremental operation. This remains separate prototype work.

The interactive outer terminal requests Crossterm focus events. When the outer terminal reports Kitty keyboard enhancement support, Stackhand requests unambiguous Escape codes plus press, repeat, and release event types. Stackhand's owned input adapter maps those host event types and common modifiers to Ghostty input. Ghostty then applies the child-selected Kitty keyboard flags during key encoding.

The Stackhand command gate has a 256 KiB byte limit and 256 message slots. The downstream PTY writer has the same byte limit and 1,024 message slots. A command keeps its gate byte reservation until its complete encoded item enters the writer. If the writer is full, the terminal owner keeps the one complete encoded item and retries it. `send_paste` returns a request token after bounded command admission. This is not a synchronous delivery acknowledgement. The application polls the token for a request-specific `Delivered` or `Failed` completion without blocking the UI. A saturated gate rejects a complete paste before admission. Queue saturation and PTY writer failures remain visible. The writer thread retries partial operating-system writes until the admitted item is complete, a terminal failure is reported, or explicit Process shutdown cancels remaining input because the child is stopped first.

## Limits that remain

- Crossterm is provisional. It does not expose associated-text reporting, and its alternate-key support replaces the base key instead of preserving both values. Non-US physical-key identity and composed or IME input remain unproved.
- The outer terminal decides whether Kitty keyboard enhancement is available. Stackhand cannot provide key release or repeat data when the outer terminal does not report it.
- Stackhand sends a focus report whenever it forwards a host focus event. The current adapter does not filter this by the child focus-reporting mode, so mode 1004 filtering remains open.
- Alt-character input is not yet verified as supported on macOS. The current adapter passes the Alt modifier to Ghostty, but it does not configure Ghostty's macOS Option-as-Alt policy. The fixture records that Alt-X currently arrives as `x`, without an Escape prefix. Control and Shift modifier cases are verified.
- Scrollback has a requested 64 KiB memory target, not a strict line limit. Ghostty can round above the target for terminal pages and the active screen. The adapter does not yet expose exact use, current viewport position, direct top/bottom commands, or Ghostty truncation events. The footer reports these limits. A later binding decision must expose viewport and truncation data before Stackhand can show an accurate scrollbar or exact retained-history use.
- The current wrapper resolves indexed colors to RGB values before Stackhand receives the buffer. This proves that distinct ANSI, 256-color, and truecolor values survive the adapter, but Stackhand does not yet query the outer terminal's custom 16-color palette.
- Cursor shape is best effort because an outer terminal can ignore the Crossterm shape command. The automated fixture proves the Ghostty-to-Stackhand cursor state and the mapping code, not each outer terminal's display.
- The current shutdown operation stops one shell root. Process Tree containment and the interrupt, terminate, and kill ladder belong to the separate process-ownership spike.
- Stackhand owns and joins the PTY reader, terminal owner, and PTY writer. Shutdown still requires the Process to stop first so the blocking PTY read reaches end-of-file.
- The Ghostty build fetches its pinned source during the first build. The current binding links the vendored static library into the application binary. An offline contributor build still needs a local checkout supplied through `GHOSTTY_SOURCE_DIR`. See [packaging evidence](./packaging-evidence.md).
- Selection uses Ghostty press, drag, release, repeat-click, and autoscroll gesture APIs. Ghostty owns the active selection and its tracked endpoints across output and resize/reflow. The Ratatui adapter reads Ghostty's per-cell selected state for the visible highlight.
- User copy uses Ghostty's plain selection formatter with soft-wrap unwrapping and trailing-padding trimming. Tests assert copied logical text for Unicode, hard and soft line breaks, scrollback autoscroll, live output, and reflow. The UI writes owned text to the system clipboard after a user action. A failure produces a warning and does not stop the terminal session.
- The 0.2.1 callback boundary ignores child OSC 52 reads. Stackhand registers a callback that denies all child clipboard writes. Clipboard contents are not logged.
- Mouse arbitration now uses Ghostty's current tracking state, with Shift as the Stackhand selection override. See [mouse ownership evidence](./mouse-ownership-evidence.md).

No evidence from this slice conflicts with the accepted separation between Ratatui application UI and Ghostty terminal semantics.

The completed real-program matrix and final **go** recommendation are in
[terminal prototype validation](./terminal-prototype-validation.md).
