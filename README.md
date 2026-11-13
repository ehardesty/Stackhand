# Stackhand

Stackhand is a terminal UI for running and supervising the commands in a local
development Project.

Define your Services, One-shots, Dependencies, environment, and health checks
in YAML. Stackhand starts the right commands, shows their output, and keeps
control of the whole Project in one terminal.

> **Early prototype:** Stackhand is not ready for production use. Its
> configuration and interface can change.

## What Stackhand does

- Runs Services that stay active and One-shots that finish.
- Starts Processes in Dependency order.
- Waits for `started`, `ready`, `exited`, or
  `completed_successfully` conditions.
- Checks Service readiness and liveness with TCP, HTTP, `exec`, or log checks.
- Restarts failed Services when their restart policy allows it.
- Shows PTY terminal output or separate pipe output.
- Can find and link to listening TCP ports that development servers select at runtime.
- Shows a scrollbar for retained PTY history when the output is longer than the pane.
- Keeps bounded Logs that you can scroll, search, filter, and copy.
- Uses Project Profiles to select Project-wide configuration, such as `local`
  and `cloud`.
- Supports a machine-local `stackhand.local.yaml` override.
- Validates configuration before starting a Process.

## Quick start

### Requirements

- Rust installed with [rustup](https://rustup.rs).
- macOS with Xcode or the Xcode Command Line Tools.
- Network access for the first build.

The current prototype is developed and validated on macOS arm64. Linux builds
may work, but Linux interactive behavior is not yet validated. Windows is not
supported.

Homebrew is not required for local builds. The repository pins Rust `1.96.0` in
[`rust-toolchain.toml`](./rust-toolchain.toml). The build helper uses Zig
`0.17.0` from its cache, `STACKHAND_ZIG`, or `PATH`. If none is available, it
downloads the release, checks its SHA-256 checksum, and caches it in your user
cache.

Clone the repository and run the example Project:

```sh
git clone https://github.com/ehardesty/Stackhand.git
cd Stackhand
./scripts/cargo.sh run --locked -- examples/basic.yaml
```

The first build may download the Rust dependencies, the pinned Ghostty source,
and Zig. Later builds reuse the cached tools and build artifacts.

To provide another exact Zig binary, set `STACKHAND_ZIG`:

```sh
STACKHAND_ZIG=/path/to/zig ./scripts/cargo.sh build --locked
```

The Zig binary must report version `0.17.0`. The native Ghostty parser is also
optimized in development builds; Rust application code retains debug settings.

## Build and install

Build a debug binary:

```sh
./scripts/cargo.sh build --locked
```

Build an optimized binary:

```sh
./scripts/cargo.sh build --locked --release
```

The optimized binary is `target/release/stackhand`.

Install the binary in Cargo's user bin directory, normally `~/.cargo/bin`:

```sh
./scripts/cargo.sh install --path . --locked --force
```

Then run `stackhand` from any Project directory. Make sure `~/.cargo/bin` is
on your `PATH` if your Rust installation did not add it automatically.

Zig and Ghostty are build-time dependencies. The resulting Stackhand binary
does not need Zig or a separate Ghostty installation at runtime.

## Create a Project

Create a `stackhand.yaml` file in the directory for your Project. This example
starts a database first, waits for its TCP port, and then allows the API to
start:

```yaml
version: 1

processes:
  database:
    command: [postgres]
    ready:
      tcp:
        host: 127.0.0.1
        port: 5432

  api:
    command: [pnpm, dev]
    depends_on:
      database: ready
```

Each Process uses exactly one of `command` or `shell`:

```yaml
processes:
  api:
    shell: ". .env && exec pnpm dev"
```

A Process is a `service` by default. Use `kind: one-shot` for a command that
should finish. Set `terminal.mode` to `pipe` for separate output streams, or
leave the default `pty` when the command needs terminal behavior. A PTY accepts
keyboard input while its console has focus. Set `terminal.input: disabled` when
the Process must not receive keyboard input.

Dependencies control startup only. They do not couple Process lifetimes after
a Process starts.

To show listening TCP ports that development servers select at runtime, enable
port discovery for the Project:

```yaml
settings:
  port_discovery: true
```

This setting applies to all Processes in that `stackhand.yaml` Project. It is
off by default. Click a port in the **Ports** column to open its
`http://localhost:PORT/` address. See the configuration reference for polling
and platform limits.

See the [configuration reference](./docs/configuration.md) for all fields,
defaults, Dependency conditions, readiness and liveness checks, restart
policies, environment files, Profiles, and local overrides.

## Run a Project

From the Project directory, let Stackhand discover the nearest
`stackhand.yaml`:

```sh
stackhand
```

You can also pass a Project path:

```sh
stackhand path/to/stackhand.yaml
```

Select a Project Profile for the first Run:

```sh
stackhand --profile local path/to/stackhand.yaml
```

Open the TUI with all Processes stopped:

```sh
stackhand --no-autostart
stackhand --no-autostart --profile local path/to/stackhand.yaml
```

`--no-autostart` skips startup for this session only. Configured `autostart`
values and manual start actions (including Dependencies) still work normally.

When Stackhand discovers a Project, it also loads `stackhand.local.yaml` from
the same directory if that file exists. An explicit Project path does not load
the local override.

Check configuration without starting Processes:

```sh
stackhand config validate
stackhand config validate path/to/stackhand.yaml --profile local
```

View the effective configuration with environment values redacted:

```sh
stackhand config show path/to/stackhand.yaml
```

## Keyboard controls

Focus changes which keys Stackhand receives. Stackhand starts with the Process
list focused. When an interactive PTY has focus, ordinary keys go to the
selected command.

The footer starts with a badge that names where keys go: `PROCESSES`, `INPUT`
(keys reach the command), `LOGS`, `COPY`, and so on. Its hints change with the
focus, and narrow terminals drop optional hints before the ways out.

Visible underlined actions can also be clicked. A click runs a single action
directly. The lifecycle and Project Profile controls open a menu because they
contain more than one choice. The console's top border shows `Terminal` and
`Logs` tabs; click the other tab to switch views.

### Process list

Use these keys while the Process list has focus:

| Key | Action |
| --- | --- |
| `j` / `k`, or arrow keys | Select a Process. |
| `PageUp` / `PageDown` | Scroll the selected Process's output. |
| `f` | Return the selected output to the live tail. |
| `s` | Start the selected Process. |
| `S` | Start a Waiting Process once without checking its Dependencies. |
| `x` | Stop the selected Process. |
| `r` | Restart a Service or rerun a One-shot. |
| `p` | Select the next Project Profile. |
| `R` | Apply a pending Profile change when Stackhand shows this action. |
| `l` | Switch the selected PTY between Terminal and Logs. |
| `/` or `Ctrl-F` | Search the selected Logs. From a Terminal, this shows Logs; `Esc` returns. |
| `&`, `\`, `F4`, or `Ctrl-G` | Filter the selected Logs. |
| `v` | Enter Copy mode for the selected PTY Terminal. |
| `Tab` or `Ctrl-A` | Focus the selected console. |
| `q` | Stop the Project and quit. |
| `Ctrl-Q` | Stop the Project and quit from any focus. |

`S` bypasses Dependencies for one Run only. It does not change the Project
configuration or later Runs. Selecting a new Project Profile does not change
active Processes until you apply the change with `R`.

### Logs

Pipe Processes always show Logs. Press `l` from the Process list to switch a
PTY between its Terminal and Logs. When Logs are visible, use:

| Key | Action |
| --- | --- |
| `j` / `k`, or arrow keys | Move through Logs when the Logs pane has focus. |
| `PageUp` / `PageDown` | Move one page through Logs. |
| `Home` / `End` | Move to the start or end of Logs. |
| `f` | Return to the live output tail. |
| `/` or `Ctrl-F` | Start a case-sensitive search. Type text and press `Enter`. |
| `n` / `N`, or `F3` / `Shift-F3` | Move to the next (older) or previous (newer) search match. |
| `&`, `\`, `F4`, or `Ctrl-G` | Edit the Logs filter. |
| `l` | Switch a PTY back to its Terminal. |
| `c` / `y` | Copy selected Logs, or the visible Logs when nothing is selected. |
| `Esc` | Clear a Logs selection or return to the Process list. |
| `Tab` or `q` | Return to the Process list. |

Search and filter prompts open in the footer, so they do not cover the output.
In a prompt, `Enter` commits, `Esc` cancels, `Ctrl-U` clears the text, and
`Tab` commits and switches between search and filter. A search prompt opens
empty and a filter prompt opens with the current filter. Commit an empty prompt
to clear a search or filter.

A filter hides lines that do not match, like the VS Code output filter. Separate
terms with commas, start a term with `!` to exclude matching lines, and start it
with `\` to match a literal `!`. Matching ignores case. For example,
`error,warn,!retry` shows errors and warnings except retries. Run markers stay
visible. An empty filter shows every line, and the Logs tab reads
`Logs (filtered)` while lines are hidden.

A mouse drag selects Logs text. Press `c` or `y` to copy the selection. A plain
click focuses Logs without selecting text.

History is configurable in
[`settings.output`](docs/configuration.md#output-history). Defaults are 16 MiB
of terminal scrollback per PTY and 32 MiB of Logs per Process, with separate
Project budgets. Logs search checks recent output first and keeps at most 1,000
matches; it shows progress while checking older output. The bottom border of
Logs shows the search, the filter, and the Run range, and marks expired earlier
history. If the page you paused on expires, that visible page stays readable
until you scroll again or press `f` to follow. History stays in memory and ends
when Stackhand exits; use your application's log files for durable retention.


### PTY input and Copy mode

Press `Ctrl-A` to focus the selected console. A PTY accepts keyboard input by
default while its console has focus. This includes `Ctrl-C`, which is sent to
the child command, as is `Ctrl-F`. Press `Ctrl-A` again to return to the
Process list; follow it with `/` to search Logs or `l` to show them. A Process
with `terminal.input: disabled` does not accept keyboard input.

When PTY output is longer than the pane, Stackhand shows a scrollbar on the
right. Click the track or drag the thumb to move through the retained terminal
history.

Press `v` from the Process list, or drag in a PTY Terminal, to enter Copy mode.
Use `h`/`j`/`k`/`l` or the arrow keys to move. Press `v` to toggle the selection
endpoint, `a` to select all retained terminal text, and `c` or `y` to copy.
`PageUp` and `PageDown` move through terminal history. Press `Esc` or `q` to
leave Copy mode.

When you select a new Project Profile, active Processes continue to run until
you apply the change with `R`. Stackhand then stops Processes that the new
Profile disables and restarts affected enabled Processes that use autostart.
It does not start a newly enabled Process automatically.

## Terminal colors

Interactive sessions query the host terminal's default foreground, background,
and all 256 indexed colors with read-only OSC 10, OSC 11, and OSC 4 requests.
Inside tmux older than 3.7, or with unrecognized tmux version metadata, only
foreground/background are queried. Those versions can turn fragmented palette
replies into keystrokes (tmux #4749). Indexed colors retain Ghostty defaults;
upgrade tmux to 3.7 or newer for host palette inheritance.
The color requests share one 750 ms startup deadline. Keyboard negotiation
continues to use its existing timeout. Input typed during negotiation stays
queued for the application.

Each new PTY terminal inherits the captured colors through Ghostty's default
color APIs. Child color queries, OSC color changes, and resets use that pane's
Ghostty state. Changes in one pane do not alter another pane or the host palette.
Named ANSI colors use the inherited palette. Bold text keeps the same palette
entry, matching Ghostty's default. Indexed colors, truecolor, and underline colors
retain their RGB values.
An unchanged inherited default background uses the host's default background,
so transparent host backgrounds remain transparent. Explicit child backgrounds
and erased backgrounds retain their resolved colors.

Hosts that answer only some queries supply a partial profile. Missing entries
retain Ghostty's built-in defaults. A host that does not answer uses those
defaults throughout. The profile is fixed for the session, so host theme changes
require restarting Stackhand. SSH and outer tmux sessions depend on the terminal
or multiplexer answering these standard queries and supporting RGB output.
Stackhand does not modify tmux options or the host palette.

Color replies with the prefixes `ESC ] 4 ;`, `ESC ] 10 ;`, and `ESC ] 11 ;` are
consumed before key decoding. Late replies do not update the captured profile.
Other Alt-] input remains keyboard input. Reply framing accepts BEL and
ESC-backslash terminators and
buffers at most 16 KiB per frame. Oversized or malformed frames are discarded
through their terminator. An unterminated frame ends the session after 30 seconds
to prevent its payload from becoming child input. Bracketed paste takes priority,
including literal OSC text in a paste. A standalone Escape key has a 25 ms
sequence disambiguation window.
If a reply prefix pauses for more than 25 ms and the reader observes idle input,
the remaining prefix can be interpreted as keyboard input. This is a legacy terminal
sequence ambiguity. Kitty keyboard enhancement avoids the Alt-key ambiguity
when the outer terminal or multiplexer supports it. Missing query answers use
fallback colors; a stalled query write that exhausts its budget aborts startup.

## Examples

The [`examples`](./examples/) directory contains ready-to-run Projects:

- [`basic.yaml`](./examples/basic.yaml): Services, an interactive shell, a
  One-shot, and a disabled Process.
- [`dependencies.yaml`](./examples/dependencies.yaml): Dependency conditions
  and startup order.
- [`readiness.yaml`](./examples/readiness.yaml): TCP and HTTP readiness checks.
- [`failures.yaml`](./examples/failures.yaml): failed Runs and blocked
  Dependents.
- [`output-pressure.yaml`](./examples/output-pressure.yaml): bounded output
  history while control commands remain responsive.

Run an example from the repository root:

```sh
./scripts/cargo.sh run --locked -- examples/dependencies.yaml
```

See the [examples guide](./examples/README.md) for expected behavior and a
longer list of manual checks.

## Development

Use `scripts/cargo.sh` for Cargo commands that compile Stackhand. It selects
the pinned Zig toolchain and keeps Zig's native cache separate from other Zig
versions.

```sh
./scripts/cargo.sh fmt --all -- --check
./scripts/cargo.sh check --locked --all-targets
./scripts/cargo.sh test --locked --all-targets
./scripts/cargo.sh clippy --locked --all-targets -- -D warnings
```

The Rust dependency graph is pinned by [`Cargo.lock`](./Cargo.lock). The native
build inputs, including the Ghostty revision and Zig release checksums, are
recorded in [`packaging/build-metadata.toml`](./packaging/build-metadata.toml).
Stackhand uses the third-party `libghostty-rs` bridge at revision
`cd4d497e8c698b27054a677ce19d1bd1a1a4d8bf`, which builds Ghostty `vt-1.0.1`.
Both wrapper crates report version `0.2.1`; the exact Git revision selects the
native engine.

## Documentation

- [Configuration reference](./docs/configuration.md): accepted YAML and
  defaults.
- [Example Projects](./examples/README.md): guided examples and manual checks.
- [Product and technical specification](./docs/product-specification.md):
  proposed long-term behavior and architecture.
- [Implementation plan](./docs/implementation-plan.md): prototype milestones,
  risks, and validation work.

## Clipboard over SSH

User copies from Terminal and Logs share one host-session clipboard service.
`STACKHAND_CLIPBOARD=auto|native|osc52|off` selects its backend. The default
`auto` uses OSC52 when SSH environment variables are present, even with a remote
display. It also uses OSC52 on headless Linux. Local macOS uses the native
clipboard. Use an explicit backend when inherited SSH variables or outer tmux
make automatic detection ambiguous.

OSC52 submits the copy to your local terminal. Stackhand cannot confirm delivery
because OSC52 has no success acknowledgement. Your terminal must permit clipboard
writes. In Ghostty, this is controlled by
[`clipboard-write`](https://ghostty.org/docs/config/reference#clipboard-write).
Outer tmux needs `set-clipboard on` and an `Ms` capability, as described in the
[tmux clipboard guide](https://github.com/tmux/tmux/wiki/Clipboard). Stackhand does
not change tmux settings or wrap commands in DCS passthrough. Copies above 64 KiB
of UTF-8 are rejected whole with a warning. Terminal limits vary and may be lower.
Native mode retains clipboard ownership for the session. Child clipboard reads
and writes remain denied, including from an inner tmux. Host paste uses bracketed
paste and does not read the clipboard.

## License

Stackhand is available under the [MIT License](./LICENSE).
