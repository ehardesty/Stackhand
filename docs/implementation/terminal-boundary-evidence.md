# Terminal boundary evidence

This note records evidence from the first narrow terminal slice. It does not decide the full terminal prototype.

## User-visible result

Stackhand can open one real shell in a PTY-backed Ratatui pane. Basic text and Enter go to the shell through Ghostty key encoding. Shell output returns through Ghostty terminal parsing. A pane resize updates both terminal state and the PTY. `Ctrl-Q` stops the prototype and restores the outer terminal.

## Current boundary

- Ratatui owns the outer application pane.
- Crossterm supplies basic host events and outer terminal control.
- `portable-pty` owns PTY transport for this slice.
- `ratatui-ghostty` owns the serialized Ghostty terminal thread and converts its render state to a Ratatui buffer.
- Stackhand's terminal adapter is the only module that uses `ratatui-ghostty` session types during normal terminal operation.
- Render data returned by the adapter is an owned buffer with owned cursor data. No borrowed Ghostty value leaves the adapter.
- Stackhand owns the shell handle separately from PTY transport. On exit, it stops and waits for the shell before it stops the terminal owner.

The current `Cargo.lock` pins `ratatui-ghostty` 0.2.0, `libghostty-vt` 0.1.1, and the Ghostty source revision selected by that binding. That Ghostty revision requires Zig 0.15.2. Zig 0.16.0 does not build it because Ghostty rejects that version and uses an API removed from Zig 0.16.

## Automated evidence

The executable has a deterministic fixture mode for integration tests. It starts `/bin/sh` in a real PTY, sends fixture text and Enter through the same terminal adapter used by the application, and reads the expected echo from the owned rendered snapshot. This mode does not change the normal interactive path.

## Limits that remain

- Crossterm is provisional. Later input work must test modified keys, focus, paste, mouse input, and nested terminal protocols before it becomes a final choice.
- This slice supports basic text and Enter only as a verified input set.
- The current shutdown operation stops one shell root. Process Tree containment and the interrupt, terminate, and kill ladder belong to the separate process-ownership spike.
- The upstream session wrapper does not expose a reader-thread join handle. Stackhand first stops and waits for the shell, which closes the PTY slave and lets the reader reach end-of-file. Stackhand then requests terminal-owner shutdown and waits for its completion flag. Later runtime work must replace this with explicit task joins if tests show a thread can remain active.
- The Ghostty build fetches its pinned source during the first build. Offline and packaged builds remain part of the later reproducible-build ticket.
- Scrollback, selection, mouse ownership, paste, full key encoding, sustained output, and packaging are intentionally outside this ticket.

No evidence from this slice conflicts with the accepted separation between Ratatui application UI and Ghostty terminal semantics.
