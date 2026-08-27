# Stackhand

Stackhand is a prototype local-development process supervisor. It combines a compact process list with a large interactive terminal and a small lifecycle model for services, one-shot commands, dependencies, probes, hooks, and restarts.

The project is an idea under validation. All current milestones are prototype work. No release boundary, supported-product platform list, or release date has been selected.

## Documentation

- [Product and technical specification](./docs/product-specification.md): the proposed north-star product behavior and architecture contract.
- [Prototype implementation plan](./docs/implementation-plan.md): flexible milestones, spikes, tests, risks, and open questions.
- [Domain language](./CONTEXT.md): the canonical terms used by the project.
- [Architecture decisions](./docs/adr/): decisions that are difficult to reverse and need their reasons preserved.

The specification controls when it conflicts with the implementation plan. Prototype evidence can change the plan. A change to product behavior or a normative architecture contract requires a specification update.

## Current prototype

Milestone 1 provides an integrated macOS prototype. It loads one YAML Project,
starts and supervises Services and One-shots, applies Dependency and readiness
conditions, renders pipe or PTY output, accepts focused terminal input, reports
basic metrics, and performs controlled Project shutdown. It is validation
software, not a supported release. See the example Projects below for the
current behavior.

## Build and run the Project prototype

The current Ghostty revision requires Rust 1.93 or newer and Zig 0.15.2. On macOS with Homebrew:

```sh
brew install zig@0.15
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo build
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo run -- examples/basic.yaml
```

Stackhand loads one explicit YAML Project file. It starts enabled autostart
Processes with the Process list focused. Use `j` or `k` to select a Process,
`s` to start, `x` to stop, and `r` to restart a Service or rerun a One-shot.
Press `Ctrl-A` to focus the selected console and send keys to an input-enabled
PTY. Press `Ctrl-A` again to return to the Process list. Press `q` from the
Process list, or `Ctrl-Q` from anywhere, to stop the Project and restore the
outer terminal.

For ready-to-run Projects and manual checks, see
[Example Projects](./examples/README.md).

Press `v` from the Process list to enter Copy mode for a PTY console. Use
`h`/`j`/`k`/`l` or the arrow keys to move, and press `v` again to begin the
selection endpoint. You can also click and drag to enter Copy mode directly.
A double click selects a word. A triple click selects a logical line. Press
`a` to select all retained terminal text, `c` or `y` to copy, or `Esc` to
return to the Process list. Child clipboard reads and writes are denied.

Run the automated checks with the pinned Zig version on `PATH`:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo test --all-targets
```

## Canonical sources

The documents in this repository are the maintained sources. Earlier drafts outside the repository are temporary inputs and can be deleted.
