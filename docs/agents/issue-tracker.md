# Issue tracker: GitHub

Issues and specifications for this repository live in GitHub Issues. Use the `gh` CLI for all operations.

## Conventions

- Create: `gh issue create --title "..." --body "..."`
- Read: `gh issue view <number> --comments`
- List: `gh issue list` with suitable state and label filters
- Comment: `gh issue comment <number> --body "..."`
- Add or remove labels: `gh issue edit <number> --add-label "..."` or `--remove-label "..."`
- Close: `gh issue close <number> --comment "..."`

Infer the repository from the Git remote.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Publishing

When a skill says “publish to the issue tracker,” create a GitHub issue.

## Fetching

When a skill says “fetch the relevant ticket,” run:

`gh issue view <number> --comments`

## Wayfinding

- A map is one issue with the `wayfinder:map` label.
- Child tickets use GitHub sub-issues when available.
- Child labels use `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, or `wayfinder:task`.
- Use native issue dependencies for blocking relationships when available.
- Assign a ticket before work starts.
- Add the answer as a comment, then close the ticket when work is complete.
