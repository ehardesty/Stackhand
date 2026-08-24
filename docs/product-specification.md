# Stackhand — Product and Technical Specification

**Revision:** 4
**Status:** Proposed north-star product and architecture specification
**Last revised:** 2026-08-24
**Audience:** Maintainers, contributors, designers, and reviewers
**Product name:** Stackhand
**Primary implementation language:** Rust
**Primary UI stack:** Ratatui
**Embedded terminal engine:** `libghostty-vt`, isolated behind a small internal adapter
**Companion working document:** [`implementation-plan.md`](./implementation-plan.md)

This is the canonical north-star specification. It defines durable product behavior and architecture contracts. The linked sections are one specification and use the normative terms defined in the document conventions.

## Sections

1. [Product direction](./specification/product-direction.md): purpose, goals, non-goals, principles, the Quadrant reference case, and product experience.
2. [Architecture and lifecycle](./specification/architecture-and-lifecycle.md): ownership, Process and Run models, lifecycle behavior, probes, and Dependencies.
3. [Runtime and configuration](./specification/runtime-and-configuration.md): restarts, shutdown, hooks, configuration, profiles, commands, and process I/O.
4. [Terminal and output](./specification/terminal-and-output.md): `libghostty-vt`, host input, selection, clipboard, output history, Logs, and memory limits.
5. [Interface and operations](./specification/interface-and-operations.md): TUI behavior, metrics, diagnostics, Project actions, validation, concurrency, and platforms.
6. [Safety and reference](./specification/safety-and-reference.md): errors, trust boundaries, illustrative types, example configuration, success criteria, and change history.

## Related decisions

- [ADR 0001: Separate application UI from terminal semantics](./adr/0001-separate-application-ui-from-terminal-semantics.md)
- [ADR 0002: Use current-state startup dependencies](./adr/0002-use-current-state-startup-dependencies.md)
