# Mouse ownership evidence

[Back to the prototype implementation plan](../implementation-plan.md)

## Implemented rule

The serialized terminal owner decides where each mouse event goes. It reads the current Ghostty mouse-tracking state when it handles the event.

| Console state | Owner |
|---|---|
| Child input, child tracking off | Stackhand |
| Child input, child tracking on | Child |
| Shift held | Stackhand |
| App command, scroll, or selection mode | Stackhand |

One press-drag-release gesture keeps its first owner. A child mode change or a modifier change cannot split one gesture between Stackhand and the child. The UI uses the same latched owner for follow state and history changes.

When Stackhand owns the event, a left drag uses Ghostty selection gestures. The vertical wheel moves Ghostty terminal history by three lines for each event. When the child owns the event, Ghostty encodes press, release, motion, drag, and all four wheel directions. Stackhand refreshes the Ghostty tracking mode and output format before every child-owned event. This lets a child change its protocol during a latched gesture without changing the gesture owner.

The footer starts with one of these messages:

```text
MOUSE: STACKHAND
MOUSE: STACKHAND · active gesture
MOUSE: CHILD · Shift+mouse: Stackhand
```

The ownership text stays before lower-priority control hints so it remains visible on an 80-column terminal.

## Automated evidence

The `--fixture-mouse` executable fixture starts a real PTY child. The child enables any-event tracking and SGR mouse output. It then records the bytes that it receives.

The fixture verifies these child-visible SGR events:

- left press and release;
- unpressed motion;
- left-button drag;
- vertical wheel in both directions;
- horizontal wheel in both directions.

Before those events, the fixture sends a complete Shift selection gesture. The recorded child bytes contain only the later child-owned events. This proves that override bytes do not reach the child.

Focused unit tests also verify:

- child tracking off uses Stackhand selection and sends no bytes;
- child tracking off sends a vertical wheel event to Stackhand history;
- Shift suppresses child bytes while SGR tracking is active;
- app command, scroll, and selection modes retain ownership;
- a release outside the pane still reaches the active owner;
- adding or releasing Shift during a gesture does not change UI ownership or follow state;
- one gesture keeps its initial owner across a modifier or child-mode change;
- a child-owned gesture uses a new mouse output format when the child changes it before release;
- the footer shows both the current owner and the Shift override.

Run the fixture with:

```bash
cargo run --locked -- --fixture-mouse
```

## Manual program checks

These checks ran on macOS arm64 in an 80-by-24 outer terminal. The installed programs were Vim 9.2, less 668, and tmux 3.7c.

### Editor

Vim ran with `set mouse=a`. Stackhand changed its footer from `MOUSE: STACKHAND` to `MOUSE: CHILD`. An outer SGR left click moved the Vim cursor to line 2, column 2. Inserting `X` and saving produced `tXwo` on line 2. This proves that the click reached Vim with the expected cell position.

### Pager

less ran with `--mouse --wheel-lines=3` over a 100-line file. Stackhand showed child mouse ownership. One outer SGR wheel-down event changed the first visible line from 1 to 4. This proves that the wheel event reached less and retained its configured three-line step.

### Terminal multiplexer

tmux ran with `set -g mouse on`. Stackhand showed child mouse ownership. One outer SGR wheel-up event entered tmux copy mode. A separate Shift drag selected Stackhand terminal text, changed Stackhand to a history view, and did not cause another tmux mouse action.

Stackhand then exited with controlled terminal restoration. The outer mouse and focus modes were disabled, the alternate screen closed, the cursor became visible, and the cursor shape returned to the user's default.

## Host limits that remain

- Crossterm reports cell coordinates, not pixel coordinates. SGR-pixel fidelity is not proved.
- Crossterm exposes left, middle, and right buttons. It maps wheel directions separately. Extra physical mouse buttons are not available through this host boundary.
- Some outer terminals do not report every modifier on every mouse event. On macOS, an outer terminal can report Control-left-click as right-click.
- Motion, horizontal wheel, and modifier events can reach the child only when the outer terminal reports them.
- The current prototype has one console. Process-list and search-mode mouse ownership remain specification rules until those views exist.
- The footer uses the last owned snapshot for display. The serialized terminal owner still makes the authoritative decision from current Ghostty state. A tracking-mode change can appear in the footer one redraw after the child emits it.
- Stackhand ignores horizontal wheel events when it owns the mouse. Child-owned horizontal wheel events are encoded.

No evidence from this slice changes the accepted Ratatui and Ghostty responsibility boundary.
