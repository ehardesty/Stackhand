# Stackhand

Stackhand is a prototype local-development process supervisor. It combines a compact process list with a large interactive terminal and a small lifecycle model for services, one-shot commands, dependencies, probes, hooks, and restarts.

The project is an idea under validation. All current milestones are prototype work. No release boundary, supported-product platform list, or release date has been selected.

## Documentation

- [Product and technical specification](./docs/product-specification.md): the proposed north-star product behavior and architecture contract.
- [Prototype implementation plan](./docs/implementation-plan.md): flexible milestones, spikes, tests, risks, and open questions.
- [Domain language](./CONTEXT.md): the canonical terms used by the project.
- [Architecture decisions](./docs/adr/): decisions that are difficult to reverse and need their reasons preserved.

The specification controls when it conflicts with the implementation plan. Prototype evidence can change the plan. A change to product behavior or a normative architecture contract requires a specification update.

## Current starting point

The first work is a terminal-boundary prototype. It must test whether Ratatui and `libghostty-vt` can provide the required rendering, input, resize, selection, output, and packaging behavior. A separate process-ownership prototype follows it. Broader supervisor work starts only after both boundaries have enough evidence.

## Build and run the terminal prototype

The current Ghostty revision requires Rust 1.93 or newer and Zig 0.15.2. On macOS with Homebrew:

```sh
brew install zig@0.15
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo build
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo run
```

Stackhand opens the shell from `SHELL`, or `/bin/sh` when `SHELL` is not set. Type in the bordered pane. Press `Ctrl-Q` to leave the prototype. Stackhand then stops the shell and restores the outer terminal.

Press `Ctrl-A`, then `s`, to enter selection mode. Drag to select cells. A
double click selects a word. A triple click selects a logical line. Press `a`
to select all retained terminal text, `y` to copy, or `Esc` to return to
application commands. Child clipboard reads and writes are denied.

Run the automated checks with the pinned Zig version on `PATH`:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo test --all-targets
```

## Canonical sources

The documents in this repository are the maintained sources. Earlier drafts outside the repository are temporary inputs and can be deleted.
