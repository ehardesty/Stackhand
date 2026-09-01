# Terminal prototype validation

[Back to the prototype implementation plan](../implementation-plan.md)

This note records the final evidence for the terminal-boundary prototype. It
does not validate Process Tree ownership or the broader supervisor.

## Recommendation

**Go.** The evidence supports ADR 0001. Ratatui can own the application UI
while Ghostty owns terminal semantics. The current boundary supports the real
terminal programs in this matrix without a second terminal emulator, a custom
general TUI framework, or shared borrowed Ghostty state.

Accept the ADR for this boundary. Keep the host event backend provisional. Do
not start broad supervisor work until the separate Process Tree ownership and
shutdown spike also passes.

## Validation host

The manual matrix ran on 2026-08-24 with this host:

| Item | Value |
| --- | --- |
| Operating system | macOS 26.6.2, build 25G83 |
| Architecture | arm64 |
| Outer terminal harness | Codex PTY, 80 by 24 cells |
| Outer environment | `TERM=xterm-256color`, `TERM_PROGRAM=codex`, `COLORTERM=truecolor` |
| Child environment | `TERM=xterm-256color`, `COLORTERM=truecolor` |
| Host event backend | Crossterm 0.29.0 |
| Host keyboard enhancement | Not available in this PTY harness |
| Host focus input | Available and observed by Vim |
| Host mouse input | SGR cell events injected through the outer PTY |
| System clipboard | Available through the macOS pasteboard |

The harness answered the outer keyboard query with no enhancement flags. It
then sent text, focus, paste, and SGR mouse sequences to the actual Stackhand
interactive input path. The mouse checks prove the Crossterm-to-Stackhand-to-
child cell-event path. They do not prove a physical mouse driver or pixel
coordinates.

## Real-program matrix

| Program | Version | Result | Manual evidence |
| --- | --- | --- | --- |
| zsh shell | 5.9 | Pass | Text and Enter produced `shell-keyboard-ok`. A safe bracketed paste produced `paste-safe-ok`. A 5,000-line producer reached `flood-4999`. Stackhand PageUp entered history, and `f` returned to the live tail. Select-all and copy produced logical text without terminal padding. |
| Vim editor | 9.2.950 | Pass | The alternate screen opened and restored. Keyboard insert and save worked. With `mouse=a`, an SGR click moved the cursor to line 2, column 2. Inserting `X` saved `tXwo`. Vim `FocusLost` and `FocusGained` autocmds each wrote their marker after outer focus events. |
| less pager | 668 | Pass | `less --mouse --wheel-lines=3` showed lines 1 through 20. One child-owned wheel-down event changed the first line from 1 to 4. PageDown also reached less. |
| fzf fuzzy finder | 0.74.3 | Pass | Filtering four values with `ga` and pressing Enter selected `gamma`. The full-screen view opened and restored. |
| Python REPL | 3.11.13 | Pass | `sum(range(10))` returned `45`. A safe bracketed paste ran `print('repl-paste-ok')`. `Ctrl-D` returned to the shell. |
| tmux | 3.7c | Pass | A nested session accepted keyboard input. With mouse mode on, one child-owned wheel-up event entered tmux copy mode. A Shift drag stayed with Stackhand, selected `tmux-keyboard-ok`, and copied that exact text. The tmux screen restored after the session ended. |

No tested program exposed a defect that requires a terminal-boundary change.

### Reproducible replay matrix

This matrix is a replay recipe, not a durable terminal transcript. The manual
screen output was captured in the Codex PTY task output and was not added to
the repository. Files below `/tmp` were temporary observations.

Build `target/debug/stackhand`, open an 80-by-24 PTY, and start it with:

```sh
env TERM=xterm-256color TERM_PROGRAM=codex COLORTERM=truecolor \
  SHELL=/bin/zsh target/debug/stackhand
```

The PTY harness must answer Stackhand's outer capability queries with
`\x1b[?0u` and `\x1b[?1;2c`. This records that Kitty keyboard
enhancement is unavailable. After the application starts, write the listed
bytes to the same outer PTY. `ESC` below means byte `0x1b`.

| Program | Launch and setup | Input or harness action | Expected observable and capture |
| --- | --- | --- | --- |
| zsh 5.9 | Start Stackhand with `SHELL=/bin/zsh`. Run `printf 'shell-keyboard-ok\n'`. Then run `python3 -c 'for i in range(5000): print(f"flood-{i:04d}")'`. | Send outer paste `ESC[200~printf 'paste-safe-ok\n'ESC[201~`, then Enter. After the flood, send `Ctrl-A`, PageUp as `ESC[5~`, then `f`. Send `Ctrl-A`, `s`, `a`, and `y`. | The screen shows both `shell-keyboard-ok` and `paste-safe-ok`, reaches `flood-4999`, moves into history, and returns to `LIVE`. `pbpaste` contains logical shell text. Screen and clipboard output were captured in the PTY task; no transcript file was kept. |
| Vim 9.2.950 | Run `vim -Nu NONE /tmp/stackhand-vim-validation.txt`. Insert `one`, `two`, and `three` on separate lines. Send Escape as a separate PTY write. Run `:set mouse=a`. | Send click press and release as `ESC[<0;3;3M` and `ESC[<0;3;3m`. Insert `X`, send Escape separately, and run `:wq`. For focus, open `vim -Nu NONE`. Run `:autocmd FocusLost * call writefile(['lost'], '/tmp/stackhand-focus-lost')` and `:autocmd FocusGained * call writefile(['gained'], '/tmp/stackhand-focus-gained')`. Then send outer `ESC[O` and `ESC[I`. | Vim reports line 2, column 2 after the click. The saved file contains `tXwo` on line 2. The two temporary focus files contain `lost` and `gained`. The screen, file bytes, and marker contents were captured in the task; the `/tmp` files are not repository artifacts. |
| less 668 | Run `seq 1 100 \| less --mouse --wheel-lines=3`. | Send wheel down as `ESC[<65;10;10M`. Send PageDown as `ESC[6~`. Send `q`. | The first visible line changes from 1 to 4 after the wheel event. PageDown moves by a page. The screen result was captured in the PTY task only. |
| fzf 0.74.3 | Run `printf 'alpha\nbeta\ngamma\ndelta\n' \| fzf`. | Send `ga`, then Enter. | The full-screen view closes and the shell shows `gamma`. The screen result was captured in the PTY task only. |
| Python 3.11.13 | Run `python3 -q`. | Send `sum(range(10))`, then Enter. Send outer paste `ESC[200~print('repl-paste-ok')ESC[201~`, then Enter. Send `Ctrl-D`. | The REPL shows `45` and `repl-paste-ok`, then returns to zsh. The screen result was captured in the PTY task only. |
| tmux 3.7c | Run `tmux -L stackhand-validation new -s check`. Inside it, run `tmux set -g mouse on` and `printf 'tmux-keyboard-ok\n'`. | Send wheel up as `ESC[<64;10;10M`. Send a Shift drag as `ESC[<4;2;5M`, `ESC[<36;20;5M`, and `ESC[<4;20;5m`. Send `Ctrl-A`, `s`, and `y`. Check `pbpaste`. Leave app selection mode, leave tmux copy mode, and run `tmux kill-session`. | tmux shows copy mode after the wheel event. The Stackhand footer shows the Shift gesture owner and `NOT FOLLOWING`. `pbpaste` contains exactly `tmux-keyboard-ok`. The screen and clipboard output were captured in the PTY task only. |

For rapid resize, first resolve the session-specific outer TTY from the
Stackhand PID. Apply these sizes to that TTY without delay:

```sh
stty -f /dev/<outer-tty> rows 12 cols 60
stty -f /dev/<outer-tty> rows 40 cols 120
stty -f /dev/<outer-tty> rows 8 cols 32
stty -f /dev/<outer-tty> rows 24 cols 80
```

Run `stty size` inside zsh. It must report the final bordered pane as `21 78`.
The outer TTY name is local to one PTY session and must not be copied from a
prior run.

## Capability coverage

| Capability | Result | Evidence boundary |
| --- | --- | --- |
| Keyboard input | Pass | Shell, Vim, less, fzf, Python, and tmux accepted useful keyboard input. `Ctrl-C` and special-key byte coverage also pass in the real-PTY input fixture. |
| Focus | Pass | Vim observed both focus changes through real autocmd results. The automated input fixture also proves byte order. |
| Mouse | Pass for cell events | Vim click, less wheel, and tmux wheel behavior matched the target cell event. Physical-device and pixel-coordinate behavior were not tested. |
| Selection | Pass | Shell select-all and a tmux Shift drag used Stackhand selection. The tmux gesture did not add a child mouse action. |
| Copy | Pass | The macOS clipboard contained logical shell text and the exact tmux selection. Automated tests cover Unicode, hard and soft wraps, output mutation, and reflow. |
| Paste | Pass with current policy | Safe bracketed paste worked in zsh and Python. Newlines, a bracketed-paste terminator, and data above 64 KiB remain intentionally rejected before admission. |
| Scrollback | Pass with known limits | PageUp stopped follow mode after 5,000 lines, and `f` returned to the live tail. The target is 128 KiB with Ghostty page rounding and no exact truncation signal. |
| Rapid resize | Pass | The outer PTY changed through 60x12, 120x40, 32x8, and 80x24. The child later reported the final pane as 21 rows by 78 columns. No zero geometry or panic occurred. Automated tests prove final-size coalescing and reflow. |
| Alternate screens | Pass | Vim, fzf, and tmux opened full-screen terminal state and restored the shell state on exit. |
| High output | Pass for the prototype | The manual 5,000-line producer completed. Automated sustained-output fixtures prove continued parsing, bounded history, redraw coalescing, input-effect order, blocked-writer fairness, and controlled shutdown. |

## Automated results

The pinned Rust 1.93.0 and Zig 0.15.2 suite passed after this note was added:

```text
cargo fmt --all -- --check                         pass
cargo check --locked --all-targets                 pass
cargo test --locked --all-targets                  57 unit, 6 terminal integration, 2 sustained-output tests pass
cargo clippy --locked --all-targets -- -D warnings pass
```

The tests cover terminal rendering, primary and alternate screens, resize and
reflow, input and terminal responses, paste, scrollback, selection and copy,
mouse ownership, sustained output, queue bounds, and shutdown ordering. A
passing automated fixture does not replace the manual program results above.

## Packaging measurements

The pinned macOS arm64 package path passed after the real-program matrix:

| Measurement | Value |
| --- | ---: |
| Release binary | 2,297,408 bytes |
| Static Ghostty archive | 8,857,784 bytes |
| Package archive | 953,727 bytes |
| Package SHA-256 | `de7ccb78dda1895641de867947546ff3575d1a0c38d0db63daba36deca8d9257` |

The packaged launcher opened a real `/bin/sh` PTY with a clean runtime `PATH`
that did not contain Zig. It printed `package-post-doc-ok` and completed
controlled `Ctrl-Q` restoration. `otool -L` reported system frameworks and
libraries only. It did not report a Ghostty shared library.

Linux x86-64 and the other listed target architectures are still unverified.
This result does not define a supported-product platform or a release boundary.

## Assumptions

- The Codex PTY harness injected outer text, paste, focus, mouse, and resize
  events. Crossterm parsed those events through the normal Stackhand
  interactive path.
- The complete manual matrix ran on one macOS 26.6.2 arm64 host. Results from
  this host do not establish another platform.
- `TERM=xterm-256color` and `COLORTERM=truecolor` describe the tested nested
  terminal behavior. A different outer terminal or terminfo entry can expose
  different capabilities.
- The harness reported that Kitty keyboard enhancement was unavailable. The
  matrix therefore assumes the ordinary escape-sequence fallback.
- Stackhand, each real program, the clipboard, and the test harness shared one
  host and user process context. This was not a remote or isolated system test.
- Copy checks assume that the macOS system clipboard was available and that
  `pbpaste` read the same user's pasteboard.
- The matrix used the installed program versions listed above. A different
  version or user configuration can change program behavior.
- SGR mouse bytes were injected. No physical mouse, pixel mouse mode, extra
  mouse buttons, IME input, or non-US physical-key layout was tested.
- No Linux runtime or Linux package was available for this matrix.
- This terminal matrix makes no claim about Process Tree ownership,
  descendant cleanup, shutdown escalation, or aggregate metrics.

## Partial and unsupported behavior

- The outer PTY did not report Kitty keyboard enhancement support. Key repeat,
  key release, and enhanced key identity were not manually validated.
- Without Kitty enhancement, Escape has the normal nested-terminal timing
  ambiguity. A zero-delay injected `Escape` plus `:` was read as one
  Alt-modified sequence in Vim. A separately delivered Escape worked. Human
  timing was not measured. This is a host event limit, not evidence of a
  Ghostty ownership failure.
- Crossterm does not preserve associated text or both base and alternate key
  identities. Non-US physical keys, composed input, and IME input remain
  unproved.
- Alt-character input on macOS remains partial. The automated fixture records
  Alt-X as `x` without an Escape prefix.
- Cursor shape is best effort because the outer terminal can ignore a shape
  request.
- Focus forwarding is not filtered by the child's mode 1004 state.
- Mouse evidence covers cell coordinates and common buttons. It does not cover
  pixels, extra buttons, or every physical modifier combination.
- The adapter does not expose exact Ghostty scrollback use, direct top and
  bottom requests, or a Ghostty truncation signal. Caller-driven scrollback
  compression is not scheduled yet.
- Linux runtime input, resize, mouse, and package behavior remain unverified.
- Process Tree containment, the interrupt/terminate/kill ladder, descendant
  cleanup, and aggregate metrics belong to the next separate spike.

## Architecture interpretation

The evidence matches ADR 0001:

- Ratatui owns borders, the footer, application modes, and cursor placement.
- Ghostty owns screen state, terminal modes, input and mouse encoding,
  scrollback, selection, copy formatting, and terminal responses.
- Stackhand keeps Ghostty behind one serialized owner and returns owned render
  data to Ratatui.
- Real nested terminal programs changed terminal modes without requiring
  program-specific rendering or input code.

The remaining limits are host capability, adapter coverage, platform proof,
or Process Tree ownership work. None requires Stackhand to own terminal
semantics or replace Ratatui.
