# Domain docs

Stackhand uses one domain context.

## Before exploration

- Read `CONTEXT.md`.
- Read ADRs in `docs/adr/` that affect the work.
- If a file does not exist, continue without an error.

## Domain language

Use the terms defined in `CONTEXT.md`. Do not use synonyms that the glossary marks with `_Avoid_`.

If a required concept is missing, reconsider the new term or record the gap for domain-modeling work.

## ADR conflicts

Report any conflict with an existing ADR. Do not silently override an accepted decision.

## Layout

```text
/
├── CONTEXT.md
├── docs/
│   ├── adr/
│   └── agents/
└── src/
```
