# Sustained-output evidence

This note records the prototype evidence for GitHub issue #9. It proves the
current terminal slice under one deterministic PTY flood. It does not declare
the final Process history design.

## User-visible result

The terminal session continues to parse and render output while the view is
active, scrolled, and unfocused. A device-status query response reaches the
child before the later probe input. Output callbacks continue while the test
does not request snapshots. Redraw requests are coalesced at the owned dirty
gate.

The fixture ends by stopping the shell, waiting for it, stopping the terminal
owner, and joining the bounded PTY writer. A successful run returns only after
these shutdown operations complete.

A second real PTY fixture starts a child that never reads input while it keeps
writing noisy output. The fixture admits complete 64 KiB paste requests until
the bounded command path rejects the next full request. It proves that at least
one admitted request remains pending while real output history and redraw wakes
continue to advance. It then proves that every remaining request token reports
`Delivered` or `Failed` after controlled shutdown.

## Fixture

Run the focused fixture with:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo test --locked --test sustained_output -- --nocapture
```

The child emits a bounded, high-volume PTY stream, sends `CSI 6 n`, and waits
for one later `probe` line. The test changes state in this order:

1. active snapshots;
2. terminal scroll mode;
3. unfocused output draining without snapshots;
4. probe input and terminal-response acknowledgement;
5. controlled shutdown.

The test samples process RSS during every fixture loop and records the highest
sample. The stress bound is 128 MiB above the sample taken before the run. RSS
is an operational measurement and can be unavailable when `ps` is not present;
the structural bounds remain executable in that case.

## Explicit bounds

| Structure | Current prototype bound |
| --- | ---: |
| output work per fair turn | 32 chunks |
| command gate slots | 256 commands |
| command gate bytes | 256 KiB |
| input/effect bytes | 256 KiB |
| PTY output queue | 64 chunks |
| PTY read buffer | 4 KiB |
| diagnostic events | 64 events |
| terminal scrollback target | 128 KiB |
| Process output history | 16 MiB / 4,096 real PTY chunks |
| terminal effect collector | 256 KiB |

The production terminal owner puts every real PTY output chunk into the
Process output history before Ghostty parses it. The history evicts the oldest
complete chunk at either limit. It records the evicted byte count and emits one
coalesced truncation diagnostic. The interactive footer makes that diagnostic
visible. This is prototype Process history. It is not a sampled render model.

## Interpretation and limits

Stackhand does not use the pinned wrapper's session type. It uses the wrapper's
public input and Ratatui render helpers inside a Stackhand-owned session loop.
That loop enforces a 32-chunk output turn even while one accepted input item or
terminal effect waits for writer capacity. The command byte reservation stays
charged until all encoded bytes enter the bounded writer. A full writer leaves
one complete encoded item with the owner for retry. Output parsing continues,
so blocked input cannot fill and stall the output reader queue. Terminal
response effects use a bounded collector and take priority over later input.
Collector overflow is a visible terminal failure, not silent loss.

`send_paste` acknowledges bounded command admission and returns a unique
request token. It does not claim synchronous PTY delivery. The token receives a
request-specific `Delivered` completion only after the writer completes all
operating-system writes. Writer failure or owner shutdown produces a `Failed`
completion. The interactive loop polls these tokens without blocking.

The fixture proves scheduling and shutdown behavior on the current macOS
development host. It does not prove Linux timing, full Process Tree cleanup,
or total native allocator memory outside the measured run.
