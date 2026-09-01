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
- Keeps bounded Logs that you can scroll, search, and copy.
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

Homebrew is not required for local builds. The repository pins Rust in
[`rust-toolchain.toml`](./rust-toolchain.toml). The build helper uses Zig
`0.15.2` from its cache, `STACKHAND_ZIG`, or `PATH`. If none is available, it
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

On arm64 macOS, the build helper uses an older installed macOS SDK when the
current SDK is not readable by Zig `0.15.2`. If you need to provide another
exact Zig binary, set `STACKHAND_ZIG`:

```sh
STACKHAND_ZIG=/path/to/zig ./scripts/cargo.sh build --locked
```

The Zig binary must report version `0.15.2`.

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

Visible underlined actions can also be clicked. A click runs a single action
directly. The lifecycle and Project Profile controls open a menu because they
contain more than one choice.

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
| `v` | Enter Copy mode for the selected PTY Terminal. |
| `Ctrl-A` | Focus the selected console. |
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
| `/` | Start a case-sensitive search. Type text and press `Enter`. |
| `n` / `N` | Move to the next or previous search match. |
| `c` / `y` | Copy selected Logs, or the visible Logs when nothing is selected. |
| `Esc` | Cancel a search, clear a Logs selection, or return to the Process list. |
| `q` | Return to the Process list. |

A mouse drag selects Logs text. Press `c` or `y` to copy the selection.

### PTY input and Copy mode

Press `Ctrl-A` to focus the selected console. A PTY accepts keyboard input by
default while its console has focus. This includes `Ctrl-C`, which is sent to
the child command. Press `Ctrl-A` again to return to the Process list. A Process
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

## Documentation

- [Configuration reference](./docs/configuration.md): accepted YAML and
  defaults.
- [Example Projects](./examples/README.md): guided examples and manual checks.
- [Product and technical specification](./docs/product-specification.md):
  proposed long-term behavior and architecture.
- [Implementation plan](./docs/implementation-plan.md): prototype milestones,
  risks, and validation work.

## License

Stackhand is available under the [MIT License](./LICENSE).
