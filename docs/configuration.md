# Current configuration reference

This page describes the YAML that the current Stackhand prototype accepts.
The north-star [runtime and configuration specification](./specification/runtime-and-configuration.md) can describe later behavior that is not available yet.

Use `stackhand config validate` to check a Project without starting a Process.
Use `stackhand config show` to see the selected effective Project with environment values redacted.

## Small Project

```yaml
version: 1

processes:
  api:
    command: [pnpm, dev]
    depends_on:
      database: ready

  database:
    command: [postgres]
    ready:
      tcp: {host: 127.0.0.1, port: 5432}
```

`version` must be `1`. `processes` is a mapping from each Process name to its configuration. Unknown fields are errors.

## Project fields

```yaml
version: 1

env_files:
  - .env
  - .env.local

settings:
  shell:
    program: /bin/bash
    args: [-lc]

processes: {}
```

- `env_files` lists environment files in load order. Relative paths start from the directory that contains `stackhand.yaml`.
- `settings.shell` selects the program that runs every `shell` expression. The default is `/bin/sh` with `[-c]`.
- `processes` can be empty.

## Process fields

```yaml
processes:
  web:
    kind: service
    enabled: true
    autostart: true
    cwd: app/web
    env_files: [.env.web]
    environment:
      PORT: "3000"
      REMOVE_THIS_VALUE: null
    terminal:
      mode: pty
      input: disabled
    success_exit_codes: [0]
    restart:
      policy: never
      backoff: 2s
      max_restarts: 5
      on_unhealthy: false
    command: [pnpm, dev]
```

Each Process must set exactly one of `command` or `shell`. The other fields are optional.

| Field | Accepted values | Default |
| --- | --- | --- |
| `kind` | `service`, `one-shot` | `service` |
| `enabled` | Boolean | `true` |
| `autostart` | Boolean | `true` |
| `cwd` | Directory path | Project directory |
| `env_files` | List of paths | Empty |
| `environment` | String values or `null` | Empty |
| `terminal.mode` | `pty`, `pipe` | `pty` |
| `terminal.input` | `disabled`, `focused` | `disabled` |
| `success_exit_codes` | Unique codes from 0 through 255 | `[0]` |
| `restart.policy` | `never`, `on_failure`, `always` | `never` |
| `restart.backoff` | Positive duration | `2s` |
| `restart.max_restarts` | Nonnegative whole number | `5` |
| `restart.on_unhealthy` | Boolean | `false` |

A direct command is a sequence. Every item must be a string:

```yaml
command: [dotnet, run, --launch-profile, Local]
```

A shell command is a sibling field:

```yaml
shell: "source .venv/bin/activate && exec python worker.py"
```

Do not set both forms on one Process. A One-shot cannot use the `always` restart policy.

## Dependencies

`depends_on` maps a Dependency Process name to one condition:

```yaml
processes:
  api:
    command: [api]
    depends_on:
      database: ready
      prepare: completed_successfully
```

The accepted conditions are:

- `started` for a Process that has started;
- `ready` for a Service with a readiness check;
- `exited` for a One-shot that has exited;
- `completed_successfully` for a successful One-shot.

Stackhand validates every Dependency name, condition, and cycle before it starts a Process.

## Readiness and liveness

A `ready` or `liveness` block contains one `tcp`, `http`, `exec`, or `log` check:

```yaml
ready:
  http:
    url: http://127.0.0.1:8080/health
  initial_delay: 0s
  interval: 1s
  timeout: 2s
  success_threshold: 1
  failure_threshold: 1
  startup_timeout: 1m
```

`startup_timeout` is available only on `ready`. Readiness and liveness checks are available only for Services.

Use `all` for two or more checks. Scheduling fields on `all` apply to every child by default. A scheduling field on a child overrides the parent value for that child:

```yaml
liveness:
  all:
    - tcp:
        host: 127.0.0.1
        port: 8080
    - log:
        contains: heartbeat
      interval: 5s
  interval: 2s
  timeout: 500ms
```

For each scheduling field, Stackhand uses the child value, then the parent value, and then the built-in default. `startup_timeout` remains a parent-only readiness setting.

A TCP check requires a nonempty `host` and a `port` from 1 through 65535. An HTTP check accepts a plain `http://` URL. HTTPS is not supported. A log check requires a nonempty `contains` value.

An `exec` check accepts `command` or `shell`, `cwd`, `environment`, and `success_exit_codes`.

Set `restart.on_unhealthy` to `true` when a liveness failure must stop the current Run and use the restart policy.

Durations use a nonnegative whole number followed by `ms`, `s`, `m`, or `h`. Fields that require a positive duration do not accept zero.

## Process Profiles

A Process Profile is a named partial patch inside one Process:

```yaml
processes:
  api:
    command: [api, --local]
    profiles:
      cloud:
        command: [api, --cloud]
        environment:
          MODE: cloud
        depends_on:
          login: completed_successfully
```

A Process Profile can change only:

- `command` or `shell`;
- `cwd`;
- `env_files`;
- `environment`;
- `enabled`;
- `depends_on`.

One `--profile NAME` option selects the initial global Process Profile. A Process without that name keeps its base configuration. The name `base` is reserved.

A Process can use `profile` to select a fixed Process Profile:

```yaml
processes:
  worker:
    profile: cloud
    command: [worker, --local]
    profiles:
      cloud:
        command: [worker, --cloud]
```

A profile patch replaces `env_files` and the complete `depends_on` mapping. It merges `environment` by variable name. A `null` environment value removes that variable.

## Local override

When Stackhand discovers `stackhand.yaml`, it also loads `stackhand.local.yaml` from the same directory when that file exists. An explicit Project path does not load the local override.

The local file is a partial Project mapping:

```yaml
processes:
  api:
    environment:
      PORT: "3001"
```

The local file can omit `version`. If it includes `version`, the value must be `1`. Maps merge, scalar values replace, lists replace, and `null` removes a field. Environment `null` values remove individual variables.
