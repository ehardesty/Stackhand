# Terminal boundary evidence

This note records evidence from the first narrow terminal slice. It does not decide the full terminal prototype.

## User-visible result

Stackhand can open one real shell in a PTY-backed Ratatui pane. Basic text and Enter go to the shell through Ghostty key encoding. Shell output returns through Ghostty terminal parsing. A successful pane resize updates both terminal state and the PTY. A PTY resize failure stops the prototype with a clear error. `Ctrl-Q` stops the prototype and restores the outer terminal.

## Current boundary

- Ratatui owns the outer application pane.
- Crossterm supplies basic host events and outer terminal control.
- `portable-pty` owns PTY transport for this slice.
- `ratatui-ghostty` owns the serialized Ghostty terminal thread and converts its render state to a Ratatui buffer.
- Stackhand's terminal adapter is the only module that uses `ratatui-ghostty` session types during normal terminal operation.
- Render data returned by the adapter is an owned buffer with owned cursor data. No borrowed Ghostty value leaves the adapter.
- Stackhand owns the shell handle separately from PTY transport. On exit, it stops and waits for the shell before it stops the terminal owner.
- Stackhand supplies the wrapper with a nonblocking, byte-bounded FIFO writer. Both encoded user input and Ghostty PTY write-back enter this one queue. A separate writer thread does the PTY I/O after a Ghostty callback returns.

The current `Cargo.lock` pins `ratatui-ghostty` 0.2.0, `libghostty-vt` 0.1.1, and the Ghostty source revision selected by that binding. That Ghostty revision requires Zig 0.15.2. Zig 0.16.0 does not build it because Ghostty rejects that version and uses an API removed from Zig 0.16.

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

Stackhand clears the wrapper's old dirty signal before it copies a render snapshot. If output arrives during or after that copy, the wrapper sets the signal again and Stackhand requests another snapshot. A deterministic model test injects output during the copy. It proves that the new signal remains set. The wrapper does not expose one atomic call for the buffer, cursor, and dirty state, so one snapshot can still contain buffer and cursor values from adjacent revisions. The next dirty snapshot corrects that temporary difference.

The pinned wrapper discards both resize results. Stackhand wraps the PTY resize callback and records its error before the wrapper discards it. The error becomes a terminal failure event. A deterministic test proves that a failed 42-by-12 PTY resize is visible to the application. The wrapper does not expose the Ghostty terminal resize result, so Stackhand cannot report that separate failure through this binding.

A third executable fixture records bytes inside a real PTY child. It proves:

- Enter, Tab, Backspace, Escape, navigation keys, function keys, Shift-Tab, Ctrl-Up, and `Ctrl-C` reach the child;
- normal cursor mode encodes Up as `CSI A`, while application cursor mode encodes it as `SS3 A`;
- a device-status query reply enters the queue before later user input;
- focus gained and focus lost reports keep their order after the user input;
- the child, and not an internal encoder call, observes all fixture bytes.

The interactive outer terminal requests Crossterm focus events. When the outer terminal reports Kitty keyboard enhancement support, Stackhand requests unambiguous Escape codes plus press, repeat, and release event types. The wrapper maps those host event types and common modifiers to Ghostty input. Ghostty then applies the child-selected Kitty keyboard flags during key encoding.

The input queue has a 256 KiB byte limit and 1,024 message slots. It accepts each encoded sequence as one item or rejects the full item. It does not partially accept a sequence. Queue saturation and PTY writer failures become terminal events. The current prototype exits with a clear error after saturation instead of silently dropping input.

## Limits that remain

- Crossterm is provisional. It does not expose associated-text reporting, and its alternate-key support replaces the base key instead of preserving both values. Non-US physical-key identity and composed or IME input remain unproved.
- The outer terminal decides whether Kitty keyboard enhancement is available. Stackhand cannot provide key release or repeat data when the outer terminal does not report it.
- The current wrapper sends a focus report whenever Stackhand forwards a host focus event. It does not expose the child focus-reporting mode to Stackhand, so focus filtering when mode 1004 is disabled remains a wrapper limitation.
- Alt-character input is not yet verified as supported on macOS. The wrapper passes the Alt modifier to Ghostty, but its session configuration does not expose Ghostty's macOS Option-as-Alt setting. The fixture records that Alt-X currently arrives as `x`, without an Escape prefix. Control and Shift modifier cases are verified.
- The wrapper's terminal-command channel is unbounded. The Stackhand-owned PTY writer is byte-bounded, but a later binding decision must also bound commands before large paste support is accepted.
- The current wrapper resolves indexed colors to RGB values before Stackhand receives the buffer. This proves that distinct ANSI, 256-color, and truecolor values survive the adapter, but Stackhand does not yet query the outer terminal's custom 16-color palette.
- Cursor shape is best effort because an outer terminal can ignore the Crossterm shape command. The automated fixture proves the Ghostty-to-Stackhand cursor state and the mapping code, not each outer terminal's display.
- The wrapper publishes requested terminal geometry even when its internal Ghostty resize call fails. It does not expose that result or an error callback. Stackhand detects PTY resize failures, but it cannot detect an internal Ghostty resize failure with this wrapper version. The binding decision must resolve this before Stackhand claims that every accepted resize keeps both sides equal.
- The current shutdown operation stops one shell root. Process Tree containment and the interrupt, terminate, and kill ladder belong to the separate process-ownership spike.
- The upstream session wrapper does not expose a reader-thread join handle. Stackhand first stops and waits for the shell, which closes the PTY slave and lets the reader reach end-of-file. Stackhand then requests terminal-owner shutdown and waits for its completion flag. Later runtime work must replace this with explicit task joins if tests show a thread can remain active.
- The Ghostty build fetches its pinned source during the first build. `scripts/package.sh` bundles the resulting native library for runtime use; an offline contributor build still needs a local checkout supplied through `GHOSTTY_SOURCE_DIR`. See [packaging evidence](./packaging-evidence.md).
- Scrollback, selection, mouse ownership, paste, full key encoding, sustained output, and packaging are intentionally outside this ticket.

No evidence from this slice conflicts with the accepted separation between Ratatui application UI and Ghostty terminal semantics.
