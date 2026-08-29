# Milestone 3 macOS validation evidence

## Scope and recommendation

This record covers issue #74: the complete Milestone 3 acceptance run for the
Stackhand Project and the real Quadrant workflow. It follows the shutdown and
fallback evidence in [Milestone 3 shutdown validation](./milestone-3-shutdown-validation.md).
It does not close parent issue #54 or change the parent issue state.

**Recommendation: GO to maintainer review.**

This is prototype evidence. It is not a release decision. It does not claim
Linux implementation, Linux validation, product support, or a release
boundary.

## Revisions and host

The validation used:

- Stackhand after issue #73: `9c1a60a`.
- Quadrant after issue #73, with the issue #74 changes applied:
  `d4c56ad44`.
- Validation date: 2026-08-29 UTC.
- Controller host: `MacBook-Pro.local`, macOS 26.6.2, Darwin 25.6.0, arm64.
- Rust: `rustc 1.93.0 (254b59607 2026-01-19)`.
- Cargo: `cargo 1.93.0 (083ac5135 2025-12-15)`.
- Zig used for the Stackhand checks: `0.15.2` from Homebrew
  `zig@0.15`.
- .NET SDK: `10.0.400`.
- Node: `v24.12.0`; pnpm: `10.28.0`.
- Azure Functions Core Tools: `4.12.0`.
- Docker Compose: `v5.1.2`.
- Docker client context: `dev-vm`, through the checked-in remote-port
  forwarding script.

The Docker Engine was remote for this run. The evidence therefore describes
the macOS controller workflow and the observed Compose result. It is not a
claim about a supported Linux host.

## Automated Stackhand evidence

Commands run from the Stackhand checkout:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo fmt --all -- --check
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo clippy --locked --all-targets -- -D warnings
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo build --locked --all-targets
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --all-targets -- --test-threads=1
```

All commands passed. The complete test run passed 415 library tests and 49
integration tests. No test failed, was ignored, or was filtered.

The layered compiled Project fixture also passed:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --test project_fixture -- --test-threads=1
```

The focused target passed all 6 tests. The full suite also passed the real
Project smoke, configuration error, configuration show/validate, profile,
input, output, path, interaction, convergence, and sustained-output targets.

## Configuration evidence

Commands run from the Quadrant checkout with the Stackhand binary built above:

```sh
stackhand config validate
stackhand config show
stackhand config validate "$PWD/stackhand.yaml"
stackhand config show "$PWD/stackhand.yaml"
stackhand config validate --profile local --profile docs
stackhand config show --profile devcloud --profile worker-python
```

All commands passed. They covered:

- discovery of the nearest Project;
- an explicit Project path;
- ordered profile selection;
- an optional Process selection; and
- a temporary same-directory `stackhand.local.yaml` override.

The local override moved the web URL to port `3101`. `config validate` accepted
it and `config show` reported the override source and the effective URL. The
temporary file was removed after the check.

The Quadrant Project also passed these checks:

```sh
bash -n scripts/local-dev/emulators/stackhand-compose.sh
scripts/local-dev/emulators/stackhand-compose.sh --help
docker compose -f docker-compose.local.yml config
```

## Real default workflow

The controller used the remote Docker port bridge and ran:

```sh
scripts/local-dev/emulators/forward-remote.sh dev-vm
stackhand --profile local
```

An `expect` PTY waited for the API and web health checks, then sent `q`. The
workflow accepted the input and exited with status 0. The API and web became
ready in 47 seconds for the first current run and 45 seconds for the second
current run. The API depends on the visible storage and Cosmos One-shots, so
this readiness path also proves that both required One-shots completed.

Two complete current cycles passed:

| Cycle | API and web ready | Stackhand | First cleanup poll |
| --- | ---: | ---: | ---: |
| 1 | 47 seconds | exit 0 | 0 running containers |
| 2 | 45 seconds | exit 0 | 0 running containers |

After each cycle, `servicebus-emulator`, `servicebus-sql`, `azurite`, and
`cosmosdb-emulator` were stopped. The local ports were closed and no Stackhand,
Compose, or Functions process remained. The remote-port bridge was stopped
after the run.

A stop during startup was also covered by the earlier shutdown record. It
returned status 0 and stopped the owned services. The generic input and
terminal tests cover focused PTY input, and the real cycles above sent `q` over
an interactive PTY.

## Optional Quadrant profiles

The maintained optional profiles were exercised with the local mode:

| Profile selection | Evidence | Result |
| --- | --- | --- |
| `local` + `docs` | API, web, and the documentation route `/docs` became ready; `q` stopped the Project. | Pass |
| `local` + `worker-python` | API and web became ready; the Python worker process remained running after 10 seconds; `q` stopped the Project. | Pass |
| `local` + `emulators-smoke` | The existing smoke command completed successfully before the success marker was written; API and web became ready; `q` stopped the Project. | Pass |
| `local` + `func-dotnet` | The Functions host reached TCP port `7071` after 47 seconds; a direct host run with the emulators also acquired its host lock without listener errors; `q` stopped the Project. | Pass |

The `func-dotnet` Process now uses `dotnet run`. Core Tools warned that
running `func host start` directly against this .NET Isolated project may not
load extensions correctly. The documentation server readiness URL now uses
`/docs`, which is the route served by Astro.

The `devcloud` and `localProd` selections were validated through the
configuration commands. A real cloud run was not included because it requires
external Azure identity, network, Key Vault, database, and data-plane access.

## Observed limits

- This evidence is for the selected macOS arm64 controller host.
- The Docker context was remote. Local Docker Desktop was not used for this
  run.
- The validation does not establish a Linux implementation, Linux validation,
  supported product platform list, or release boundary.
- Cloud-backed profiles were configuration-tested but not run as a release or
  production test.
- The real terminal check used an `expect` PTY. It did not cover every physical
  keyboard, IME, outer-terminal, or browser interaction case.
- Process Tree containment and the Compose adapter's named-service ownership
  remain the limits documented in the shutdown record.
- The temporary ignored Functions and Python local settings files were created
  from checked-in templates for local profile validation. They were not staged.

## Result

The selected macOS host passed the complete automated suite, layered fixture,
configuration resolution checks, real local startup, required One-shot path,
API and web readiness, optional local profiles, interactive shutdown, and
repeated cleanup. The evidence supports maintainer review of Milestone 3. The
parent issue #54 remains open.
