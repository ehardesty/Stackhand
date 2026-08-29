# Milestone 3 shutdown validation evidence

## Scope and recommendation

This record covers issue #73: controlled shutdown and fallback equivalence for
Quadrant's declarative Stackhand Project. It validates the macOS controller
workflow and the Quadrant Compose cleanup adapter. It does not close parent
issue #54 or the Milestone 3 issue.

**Recommendation: GO to issue #74 validation on the selected macOS host.**

This is prototype evidence. It is not a release decision. It does not claim
Linux implementation, Linux validation, product support, or a release
boundary.

## Revisions and host

The validation used:

- Stackhand controller after issue #69: `7c04940`.
- Quadrant Project after issue #72: `1693f2062`.
- The issue #73 wrapper and documentation changes were present in the working
  tree.
- Validation time: 2026-08-29T04:24Z to 2026-08-29T05:09Z.
- Controller host: macOS 26.6.2, Darwin 25.6.0, arm64.
- Rust: `rustc 1.93.0 (254b59607 2026-01-19)`.
- Cargo: `cargo 1.93.0 (083ac5135 2025-12-15)`.
- Docker client context: `dev-vm`, through the checked-in remote-port
  forwarding script.

The Docker Engine was remote for this run. The evidence therefore describes
the macOS controller workflow and the observed Compose result. It is not a
claim about a supported Linux host.

## Automated Stackhand evidence

Commands run from the Stackhand checkout:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --all-targets -- --test-threads=1
```

The complete suite passed:

- 415 library tests.
- All integration targets passed, including the 7 `real_project_smoke`
  tests, 6 `project_fixture` tests, 3 fixture round-trip tests, 2
  `run_convergence` tests, and 2 `sustained_output` tests.
- No test failed, was ignored, or was filtered in the complete run.

The focused shutdown and failure checks also passed:

```sh
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --lib supervisor::tests::project_shutdown -- --test-threads=1
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --test project_fixture -- --test-threads=1
PATH="$(brew --prefix zig@0.15)/bin:$PATH" \
  cargo test --locked --test real_project_smoke -- --test-threads=1
```

These checks cover shared-deadline Project shutdown, cleanup failures with
remaining PIDs, failed One-shots, failed Processes, startup cancellation,
readers, readiness probes, output ownership, real readiness children, and real
Process Tree cleanup.

## Quadrant configuration evidence

The wrapper passed shell syntax and help checks:

```sh
bash -n scripts/local-dev/emulators/stackhand-compose.sh
scripts/local-dev/emulators/stackhand-compose.sh --help
```

The local Compose file passed:

```sh
docker compose -f docker-compose.local.yml config
```

`stackhand config validate` passed for the base Project, `local`, `devcloud`,
and `localProd` profiles, and for these optional selections:

- `local` + `worker-python`;
- `devcloud` + `worker-python`;
- `local` + `func-dotnet`;
- `local` + `docs`;
- `local` + `emulators-smoke`.

## Real default workflow

The controller ran the default local Project with:

```sh
scripts/local-dev/emulators/forward-remote.sh dev-vm
stackhand --profile local
```

An `expect` PTY sent `q` after both application endpoints passed their health
checks. Observed results:

- API and web became ready after 53.810 seconds.
- Stackhand accepted the PTY input and exited with status 0 after the
  controlled shutdown.
- The first cleanup poll reported zero running Compose containers.
- `servicebus-emulator`, `servicebus-sql`, `azurite`, and
  `cosmosdb-emulator` were all stopped.
- No Stackhand, wrapper, Compose, or remote Docker SSH process remained in the
  local process check. An unrelated Vite process from another checkout was
  excluded from the result.

Two complete real default cycles were then repeated with the same PTY and
readiness procedure:

| Cycle | API and web ready | Stackhand | First cleanup poll |
| --- | ---: | ---: | ---: |
| 1 | 49.631 seconds | exit 0 | 0 running containers |
| 2 | 49.636 seconds | exit 0 | 0 running containers |

A startup stop sent `q` after 5 seconds. Stackhand exited with status 0 in 7
seconds, and the first cleanup poll reported zero running containers.

The optional smoke workflow was also run with:

```sh
stackhand --profile local --profile emulators-smoke
```

For this check only, an ignored local override wrapped the existing
`scripts/local-dev/emulators/prepare.sh --smoke` command and wrote a marker
only after that command succeeded. The API and web became ready, the marker
was present, Stackhand accepted `q`, exited with status 0, and the first cleanup
poll reported zero running containers. The override was removed after the run.

The remaining shutdown windows were exercised with the real Project and
temporary ignored local overrides. The overrides changed only the command being
observed and were removed after each run:

| Window | Setup and observation | Result |
| --- | --- | --- |
| Initialization | `storage-init` wrote a marker and slept for 120 seconds after its real emulator dependency became ready. | Marker was present at 30 seconds; `q` returned status 0 and cleanup stopped the owned services. |
| API startup | The unmodified Project received `q` at 40 seconds. API and web health checks were both still false. | Stackhand returned status 0 and cleanup stopped the owned services. |
| Failed initialization | `storage-init` wrote a marker and exited with status 1. | The failure occurred before `q`; controlled shutdown returned status 0 and cleanup stopped the owned services. |
| Failed Service | The `storage` Service command wrote a marker and exited with status 1. | The failure occurred before `q`; controlled shutdown returned status 0 and cleanup stopped the owned services. |

The injected failures test Project shutdown after a real Process failure. They
do not change the maintained Quadrant commands or claim that a failed
emulator can serve application traffic.

## Old helper comparison

The old helpers remain in the repository and were not removed:

- `scripts/local-dev/emulators/run.sh` starts selected Compose services,
  waits for readiness, runs initialization, streams logs, and stops the
  services from its exit trap.
- `scripts/local-dev/emulators/prepare.sh` waits for already-running
  emulators, runs initialization, and can run the smoke checks.

The fallback was exercised with `scripts/local-dev/emulators/run.sh all`. It
reached the ready message, received Ctrl-C, returned its expected status 130,
and left zero running Compose containers. The helper comparison therefore
covers both the old fallback cleanup and the Stackhand cleanup path.

The Stackhand path now preserves the required behavior while moving generic
coordination to Stackhand and keeping Quadrant work in Quadrant:

| Generic coordination moved to Stackhand | Quadrant-specific work kept in Quadrant |
| --- | --- |
| Process startup and dependency order | Compose service names and the SQL backing service |
| Readiness probes and startup bounds | Emulator health endpoints and port values |
| Visible One-shot ordering and completion | Azurite and Cosmos initialization scripts |
| Project shutdown ladder and restart suppression | Service Bus, Azurite, and Cosmos smoke commands |
| Pipe readers, probe work, and output ownership | Local environment values and app launch commands |
| Ordered profiles and local overrides | The old `run.sh` and `prepare.sh` fallback scripts |

`stackhand-compose.sh` is the narrow Quadrant adapter at this seam. It starts
only the Compose services named by its Process command. It keeps Compose in a
separate job-control Process Group and runs cleanup in its own group, so later
Stackhand shutdown signals cannot leave an owned container running. Its final
cleanup action uses `docker compose kill` after the Compose client stops. This
is intentionally stronger than the old helper's graceful `stop`, because the
Stackhand Project has one bounded shutdown deadline; it does not remove
volumes or initialize Quadrant data.

## Observed limits

- This evidence is for the selected macOS arm64 controller host.
- The Docker context was remote. Local Docker Desktop was not used for this
  run.
- The old helpers were retained as fallbacks. Their removal was not attempted.
- The real workflow used an `expect` PTY for input. It did not cover every
  physical keyboard, IME, outer-terminal, or browser interaction case.
- Process Tree containment remains best effort as documented by Stackhand. A
  child that creates a new session can escape the owned group.
- The Compose adapter stops only the services named by its Process command. A
  future Compose dependency must be added to that command when it must stop
  with its parent service.
- The adapter has no independent network timeout for a Docker command that never
  returns. The Stackhand shutdown deadline remains the outer bound; a hard kill
  can prevent adapter cleanup. This failure mode was not simulated.
- This record does not establish a release platform list or infer completeness
  on Linux or Windows.
