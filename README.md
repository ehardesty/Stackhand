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

## Canonical sources

The documents in this repository are the maintained sources. Earlier drafts outside the repository are temporary inputs and can be deleted.
