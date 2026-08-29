# Runtime and configuration

[Back to the product specification](../product-specification.md)

## 11. Restart and availability

### 11.1 Configuration

```yaml
restart:
  policy: on_failure       # never | on_failure | always
  backoff: 2s
  max_restarts: 5
  on_unhealthy: true
```

The baseline restart model uses a fixed backoff. Exponential backoff, jitter, rolling windows, and rate-limit strategies MAY be added later through explicit configuration if real use cases justify them.

### 11.2 Exit classification

A process exit should be classified using:

- process kind;
- desired state;
- whether project shutdown is active;
- configured success exit codes;
- whether the runtime reports a terminating signal or platform-equivalent status;
- whether shutdown escalation initiated the exit.

Suggested configuration:

```yaml
success_exit_codes: [0, 130, 143]
```

Accepted success codes affect oneshot completion and failure classification. They do not cause an unexpectedly exited service to be represented as a completed oneshot.

### 11.3 Policy rules

- `never`: do not automatically restart.
- `on_failure`: restart only when the run is classified as failed.
- `always`: restart after any unintentional service exit while desired state remains `Running`.
- A oneshot may use `never` or `on_failure`; `always` is invalid for a oneshot.
- The default policy is `never`.
- The default fixed restart backoff is `2s`.
- `on_unhealthy`: when enabled, liveness failure initiates shutdown and counts as a failed run.
- Manual stop never triggers automatic restart.
- Project shutdown suppresses all automatic restarts.
- Backoff is implemented by a cancellable timer and never blocks the supervisor loop.

### 11.4 Restart budget

`max_restarts` is the Automatic Restart Budget. A value of `5` permits five automatic retries after the initial Run. Every automatic retry consumes one unit, including retries after unhealthy Runs and retries caused by `always`.

The counter resets when:

- the user manually starts a stopped/failed process;
- the user manually restarts it;
- the project is started in a new application session.

The displayed lifetime restart count may be tracked separately from the active policy budget.

When the budget is exhausted, the process becomes `Failed` with a clear reason such as:

```text
Restart limit reached after 5 automatic attempts
```

### 11.5 Stable-run reset

No automatic stable-run reset is defined. A future explicit `reset_after` option MAY reset the automatic-restart budget after a sufficiently long healthy run.

---

## 12. Shutdown and process containment

### 12.1 Semantic shutdown ladder

Public configuration uses semantic stages:

```yaml
shutdown:
  graceful_action: interrupt
  graceful_timeout: 5s
  terminate_timeout: 3s
```

Conceptually:

```text
interrupt
wait graceful_timeout
terminate
wait terminate_timeout
kill
wait for process-tree exit
```

Default Unix mapping is expected to be:

```text
interrupt → SIGINT
terminate → SIGTERM
kill      → SIGKILL
```

A platform-specific signal override may be added, but Unix signal names should not be the primary cross-platform schema.

### 12.2 PTY transport is not process ownership

The PTY library provides terminal I/O and a child handle. It does not define the product's process-tree containment or shutdown semantics.

Use separate internal abstractions:

```rust
trait ProcessIo {
    fn resize(&self, size: TerminalSize) -> Result<()>;
    fn reader(&mut self) -> Result<ProcessReader>;
    fn writer(&mut self) -> Result<ProcessWriter>;
}

trait ProcessTree {
    fn root_pid(&self) -> Option<u32>;
    async fn interrupt(&self) -> Result<()>;
    async fn terminate(&self) -> Result<()>;
    async fn kill(&self) -> Result<()>;
    async fn wait_empty(&self, timeout: Duration) -> Result<TreeExit>;
}
```

The concrete APIs may differ, but transport and ownership must not be conflated.

### 12.3 Unix process containment

On macOS and Linux, launch each supervised process in a dedicated process group or session where practical.

The runtime should:

- signal the owned group rather than only the shell PID;
- wait for the root child and attempt to observe remaining descendants;
- aggregate metrics across the owned tree;
- avoid terminating unrelated processes that later reuse a PID;
- document limitations when descendants intentionally daemonize or escape the session/group.

### 12.4 Windows process containment

The intended future design is:

- ConPTY or a proven cross-platform PTY abstraction for terminal I/O;
- a Windows Job Object for process-tree containment and cleanup;
- semantic shutdown mapping appropriate to console and non-console children.

Windows implementation is deferred until the Unix architecture is proven.

### 12.5 Best-effort boundary

Process containment is a reliability feature, not a security sandbox. A child that intentionally daemonizes, creates a new session, or otherwise escapes ownership may evade cleanup.

The product should be reliable for ordinary development commands without promising absolute containment against adversarial processes.

### 12.6 Project shutdown

When the user quits or requests stop-all:

1. set project shutdown state;
2. suppress restarts;
3. cancel pending start/restart work;
4. run eligible `before_stop` hooks under a shared overall deadline;
5. stop active dependents before their dependencies, with independent processes at the same graph level allowed to stop concurrently;
6. escalate remaining process trees;
7. report processes that could not be confirmed stopped.

The project-level deadline prevents many per-process hooks and timeouts from multiplying into an unbounded shutdown.

### 12.7 Custom send-keys shutdown

A future explicit escape hatch may send terminal input before normal signaling for applications that only shut down cleanly through an interactive command. It should remain opt-in and must not replace process-tree signaling as the default.

---

## 13. Hooks

Hooks prevent core feature creep while preserving lifecycle customization.

### 13.1 Core hook points

```text
before_start
after_start
after_ready
before_stop
after_stop
on_failure
```

Do not add a large hook vocabulary until specific use cases justify it.

### 13.2 Hook command model

Hooks use the same `CommandSpec`, cwd, environment, output-limit, and timeout semantics as processes and exec probes.

Each invocation is identified by:

- process ID;
- `RunId`;
- hook kind;
- invocation attempt.

Hook stdout/stderr is always drained and retained within a bounded hook-output budget.

### 13.3 `before_start`

- Blocking.
- Runs after allocation of the new `RunId` and before process spawn.
- A failure or timeout fails the run.
- The process becomes `Failed`, not `Blocked`.
- Dependents remain blocked on the process's unsatisfied lifecycle condition.

### 13.4 `after_start`

- Runs after successful spawn.
- Best effort by default.
- Failure is surfaced as a diagnostic but does not normally stop a healthy process.
- Readiness evaluation starts immediately after spawn and does not wait for this hook.

### 13.5 `after_ready`

- Runs once after the first readiness success for the current run.
- Best effort by default.
- Waiting dependents are satisfied before this hook starts and do not wait for it.
- If an action must succeed before dependents start, model it as a oneshot dependency rather than `after_ready`.

### 13.6 `before_stop`

- Invoked for intentional stop/restart/project shutdown when the process exists.
- Blocking only within a configured timeout and the project-wide shutdown deadline.
- Shutdown proceeds after failure or timeout.

### 13.7 `after_stop`

- Best effort.
- Runs after the process tree is confirmed stopped or the shutdown attempt is concluded.
- Does not delay application exit indefinitely.

### 13.8 `on_failure`

- Runs once per failed run before automatic restart backoff completes.
- Best effort and bounded.
- Manual stop does not invoke it.
- Its own failure is diagnostic only.
- It never recursively invokes itself.

### 13.9 Hook output presentation

Hook output must be discoverable in the selected Process's Logs history, clearly delimited from process output. Hook output is not fed into the Run's terminal state:

```text
── before_start · run 3 ─────────────────────
checking local prerequisites...
ok
hook completed successfully

── process · run 3 ──────────────────────────
Starting API...
```

The Logs view should retain hook kind and run metadata when possible.

---

## 14. Configuration files and precedence

### 14.1 Canonical format

Use YAML with an explicit schema version:

```yaml
version: 1

env_files: []
processes: {}
profiles: {}
settings: {}
```

Version 1 accepts this canonical shape only. It does not translate older
spellings into it. `processes` and `depends_on` are name-keyed mappings.
Direct commands use a sequence, and shell commands use a sibling `shell`
field. Use `cwd`, `env_files`, `environment`, and a terminal mapping with
`mode` and optional `input`. Project-level `env_files` is a list at the root;
Process-level `env_files` is a list inside that Process.

The temporary list collections, nested command objects, `working_dir`, `env`,
top-level `input`, and scalar terminal values are rejected. The validation
message names the canonical replacement to use.

A Project may define named profiles. Select profiles explicitly with one or
more `--profile NAME` options on the normal run command, `config validate`, or
`config show`. Profiles apply in the order of the options. No profile is
selected by default. A profile may use `enable`, `disable`, `settings`, and
name-keyed `overrides`; an override can replace Process fields or add a complete
Process. The selected result is validated before any Process starts.

The final filename remains open. Examples in this document use:

```text
processes.yaml
processes.local.yaml
```

### 14.2 File discovery

When the user supplies an explicit configuration path, the CLI uses only that base file. Otherwise, it searches the current directory and then each parent directory, stopping at the first directory that contains the base filename. It then loads `stackhand.local.yaml`, when present, only from that same directory. It does not search child directories, parent directories, the home directory, or the current working directory for another override. Validation reports the selected source paths in precedence order.

Relative paths are resolved against the directory containing the base configuration file, not the process's current shell directory.

### 14.3 Merge precedence

Effective configuration is produced in this order:

```text
base configuration
  < selected profiles in CLI order
  < local override
  < explicit CLI overrides
```

Validation and dependency graph compilation occur only after the full merge.

### 14.4 Effective configuration diagnostics

Provide these non-interactive commands:

```text
<tool> config show
<tool> config validate
```

`config show` reports the base Project, selected profiles, and local override in
precedence order. It then prints the normalized canonical YAML for the effective
Project. The output includes resolved paths and effective defaults. Loaded
environment files are represented by their effective keys, but every value uses
a redaction marker. Removed environment keys are shown as YAML `null`.
Profile definitions and environment-file paths are not copied into the
effective YAML.

`config validate` reports the same selected sources without printing the
configuration. Both commands use the shared resolver and finish before any
Process starts. Resolution failures use the same diagnostic path.

This is important because profile and local-overlay behavior otherwise becomes difficult to debug.

Configuration failures identify the contributing layer and effective field. YAML
errors include the source path and line and column when available. Profile,
local-override, environment-file, path, and Dependency graph errors identify
the selected profile, source file, Process, or affected Processes. Diagnostics
are concise and do not print complete files or environment values. Resolution
finishes before the Supervisor starts, so an invalid layered Project starts no
Process.

### 14.5 Local override

A local override is intended for gitignored machine-specific changes:

- enable or disable processes;
- add a helper service;
- change ports, paths, or commands;
- add bounded hooks;
- adjust environment variables.

The shared base config should not need to know the machine-specific domain concept.

A local override is a partial YAML mapping. It may omit `version`; if present,
`version` must be `1`. It cannot define `profiles`. It uses the same deep-map,
scalar-replacement, list-replacement, and `null` clearing rules as profiles.

---

## 15. Command, working-directory, and environment model

### 15.1 Direct execution

Use `command` for a program and explicit arguments:

```yaml
command: [dotnet, run, --launch-profile, Local]
```

This invokes the executable directly. No shell parsing, expansion, pipelines, redirection, or platform-dependent word splitting occurs. A program name with a path separator is resolved from the base Project directory and checked before startup. A bare program name is left for the process launcher to find through `PATH`.

### 15.2 Shell execution

Use `shell` for a shell expression:

```yaml
shell: "source .venv/bin/activate && exec python scripts/run_local_worker.py"
```

The project has an explicit shell setting. The macOS prototype fallback is `/bin/sh -c`; commands requiring Bash behavior should configure Bash rather than relying on the user's login shell.

For example:

```yaml
settings:
  shell:
    program: /bin/bash
    args: [-lc]
```

`command` and `shell` are mutually exclusive.

### 15.3 Reusable command specification

Processes, hooks, and exec probes share one internal `CommandSpec`:

```text
Direct { program, args }
Shell { script, shell_program, shell_args }
```

Common fields include:

- cwd;
- environment;
- env files;
- stdin policy;
- terminal mode;
- timeout where applicable.

### 15.4 Working directory

- A relative `cwd` resolves from the base config directory.
- A missing or non-directory cwd is a validation error where it can be checked before start.
- Hooks and probes inherit the process cwd unless they override it.
- An explicit relative probe `cwd` resolves from the base Project directory.
- A relative direct program in a probe uses the same base Project directory and path check as a Process command.

### 15.5 Environment precedence

For one Process Run, environment changes are applied in this order:

```text
parent process environment
  < Project env files, in listed order
  < Process env files, in listed order
  < base Process environment
  < each selected profile's inline environment, in CLI order
  < local override inline environment
  < future CLI inline environment
```

A later value replaces an earlier value for the same key. A YAML `null` value
removes the key from the inherited environment and from every earlier layer.
It does not print the removed or replacement value in diagnostics.

```yaml
environment:
  SOME_INHERITED_SECRET: null
```

The resolver passes explicit values to the child and passes removals to the
process launcher. Other parent variables remain inherited.

### 15.6 Environment files

Support one or more files at the Project or Process level:

```yaml
env_files:
  - .env
  - .env.local
```

Relative paths use the base Project directory. Missing files and invalid UTF-8
are errors before startup. A future object form may mark a file optional.

Each non-blank, non-comment line is one literal assignment:

```text
KEY=value
export OTHER=value
EMPTY=
SINGLE_QUOTED='spaces, $dollars, and shell characters stay literal'
DOUBLE_QUOTED="line\nwith\tshort escapes"
```

Keys use `[A-Za-z_][A-Za-z0-9_]*`; spaces and tabs around a key are
ignored. Unquoted values trim spaces and tabs at both ends. Single quotes
preserve every character until the closing quote. NUL characters are rejected
because the operating system cannot pass them in an environment value.
Double quotes support only `\\`, `\\"`, `\\n`, `\\r`, and `\\t`. Quotes must
surround the complete value. There are no inline comments, interpolation,
command substitution, glob expansion, or shell evaluation. Later entries and
later files replace earlier values at the same level.

Environment values must not be printed indiscriminately in normal diagnostics.

### 15.7 Variable interpolation

Avoid a large templating language. If `${VAR}` interpolation is supported, define exactly:

- which fields are interpolated;
- whether interpolation uses the launcher environment or merged project environment;
- how missing variables behave;
- how literal dollar signs are escaped.

Interpolation is optional; relying on shell commands or explicit environment values is acceptable when it keeps the schema clearer.

---

## 16. Profiles and overlay merge semantics

### 16.1 Profile purpose

Profiles represent named configuration overlays such as:

- `local`;
- `devcloud`;
- `localProd`;
- `docs`;
- `minimal`.

They may enable/disable processes and override ordinary process fields. They are not a class inheritance system.

### 16.2 Merge rules

The merge contract is normative:

- maps deep-merge;
- scalar values replace;
- lists replace in full;
- `environment.NAME: null` removes that variable from the child environment;
- `depends_on.NAME: null` removes that Dependency from the effective graph;
- `null` clears optional probes, hooks, and other optional objects;
- required scalar values and complete Process definitions cannot be `null`;
- no implicit list concatenation;
- no implicit process cloning;
- no implicit profile inheritance;
- selected profiles apply in CLI order;
- local override applies after profiles;
- CLI overrides apply last.

If profiles enable and disable the same Process, the last selected profile that mentions it wins. The local override and CLI overrides can replace that result in their normal precedence order. Enablement does not change `autostart`.

If list append semantics are later required, add an explicit operation rather than changing ordinary list behavior.

### 16.3 Profile example

```yaml
profiles:
  local:
    enable:
      - servicebus
      - storage
      - cosmos
      - api
      - web

  devcloud:
    disable:
      - servicebus
      - storage
      - cosmos
    overrides:
      api:
        command: [dotnet, run, --launch-profile, DevCloud]
      worker-python:
        environment:
          QUADRANT_ENVIRONMENT: dev
```

The exact surface syntax MAY evolve, but the semantics above SHOULD remain stable.

### 16.4 Enable and disable behavior

- A disabled process remains present in the effective graph with `enabled == false`; the UI projects this as `DISABLED`.
- Dependencies do not auto-enable it.
- A dependent is visibly blocked by the disabled process.
- A local override/profile may enable it before validation/start.

### 16.5 Process removal

Complete deletion of a base process through an overlay is not required by this specification. Disabling is safer because dependency references remain diagnosable.

---

## 17. Process runtime and I/O modes

### 17.1 Pipe versus PTY mode

Terminal allocation and interactive input are separate concerns.

Recommended schema:

```yaml
terminal:
  mode: pipe          # pipe | pty
  input: disabled     # disabled | focused
```

Examples:

```yaml
# Ordinary server with separate stdout/stderr
terminal:
  mode: pipe
  input: disabled
```

```yaml
# Program wants a PTY for colors/progress but should not receive user input
terminal:
  mode: pty
  input: disabled
```

```yaml
# Interactive shell/editor
terminal:
  mode: pty
  input: focused
```

### 17.2 Default behavior

Recommended defaults:

- `service`: `pipe` + input disabled;
- `oneshot`: `pipe` + input disabled;
- explicit interactive process: `pty` + focused input.

A shorthand such as `tty: true` may be supported for migration or convenience, but the internal model must preserve the two independent decisions.

### 17.3 Pipe mode

Use separate stdout and stderr pipes.

Benefits:

- stream distinction;
- simpler line-oriented logs;
- no terminal-mode side effects;
- cleaner automation behavior.

Stdin is closed/null by default. An explicit writable pipe mode MAY be added later; it is not required by this specification.

### 17.4 PTY mode

Use a PTY for:

- shells;
- editors and terminal UIs;
- REPLs;
- tools requiring terminal capabilities;
- tools whose useful output depends on a TTY.

PTY output is one combined byte stream. The PTY size must track the actual terminal-pane cell geometry, including zoom and layout changes.

### 17.5 PTY abstraction requirements

PTY behavior MUST be hidden behind an internal transport abstraction. The selected transport must support:

- a readable output stream and writable input stream;
- terminal resize;
- child PID or equivalent process identity;
- clean detection of slave closure;
- integration with Unix process groups/sessions or Windows Job Objects where applicable;
- independent process-tree shutdown semantics;
- bounded and observable input backpressure.

A generic PTY child-kill operation MUST NOT define the product's interrupt, terminate, kill, containment, or aggregate-metrics behavior.

### 17.6 Terminal environment

Use conservative defaults for PTY processes:

```text
TERM=xterm-256color
COLORTERM=truecolor
TERM_PROGRAM=<project-name>
```

Do not invent a custom `TERM` value without shipping matching terminfo.

The embedded terminal should respond conservatively to negotiated queries through `libghostty-vt` effects.

---
