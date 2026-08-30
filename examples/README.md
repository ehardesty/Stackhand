# Example Projects

These Projects show the behavior that the current macOS prototype supports.
They omit fields when the default gives the intended behavior. See the
[current configuration reference](../docs/configuration.md) for all fields and
defaults.

The Projects use standard shell tools. The readiness example also uses `nc`,
which is included with macOS.

The examples run POSIX command text through explicit command forms. They do not
depend on your login shell. A `shell` expression uses the Project's configured
launcher, or the macOS fallback `/bin/sh -c` when no launcher is configured.

Run an example from the repository root:

```sh
cargo run -- examples/basic.yaml
```

Stackhand starts enabled autostart Processes with the Process list focused:

```text
j/k or Up/Down   select a Process
s                start the selected Process
S                start a Waiting Process now; skips Dependencies for one Run
x                stop the selected Process
r                restart a Service or rerun a One-shot
p                cycle the global Process Profile for future Runs
R                apply pending profile changes when the footer shows this key
                 (stops disabled active Processes and restarts affected active
                 enabled autostart Processes; does not start inactive Processes)
PageUp/PageDown  inspect retained output
f                return to live output
v                enter Copy mode for a PTY console
Ctrl-A           toggle Process-list and console focus
q                stop the Project and quit from the Process list
Ctrl-Q           stop the Project and quit from any focus
```

In Copy mode, use `h`/`j`/`k`/`l` or the arrow keys to move. Press `v` to
start the selection endpoint, `c` or `y` to copy, and `Esc` to return to the
Process list. A mouse drag enters Copy mode directly.

## Examples

### `basic.yaml`

Start here. This Project has a pipe Service, a manual interactive PTY shell, a
manual One-shot, and a disabled Service.

Manual checks:

1. Confirm that `clock` starts and prints once per second.
2. Start `shell`, press `Ctrl-A`, and run `echo hello` or `stty size`.
3. Start or rerun `manual-task` and look for a new Run marker.
4. Confirm that `disabled-example` cannot start.

### `dependencies.yaml`

This Project shows all three Dependency conditions:

- `started` waits for a Service to start;
- `ready` waits for a Service readiness probe;
- `completed_successfully` waits for a successful One-shot.

Watch the Process rows and selected headers. Waiting Processes name the
Dependency condition that is not yet satisfied.

### `readiness.yaml`

This Project shows TCP and HTTP readiness probes. The listeners delay startup
so that you can see readiness attempts and Waiting states.

It uses ports `43123` and `43124`. Change both the command and probe when one of
these ports is already in use.

### `failures.yaml`

This Project shows failures without stopping the TUI:

- a One-shot exits with status 7;
- its dependent remains Waiting;
- a Service exits unexpectedly;
- a manual Process has a missing executable and fails when you start it.

### `output-pressure.yaml`

This Project produces enough pipe output to cross the one-MiB retained-output
bound. Select `noisy` to see the truncation warning. Then stop or restart
`steady` to confirm that lifecycle commands still work while output is busy.

The example is intentionally noisy. Press `Ctrl-Q` when the check is complete.

## Current limits

These examples are for the current macOS prototype. Windows is not supported.
Linux interactive PTY behavior is not yet current validation evidence. These
examples do not yet show hooks, Process Profiles, local overrides, or
configuration discovery.
