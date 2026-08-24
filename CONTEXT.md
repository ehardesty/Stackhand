# Stackhand

Stackhand supervises the commands that make up a local development project. This glossary defines the language used in product documents and source code.

## Language

**Process**:
A configured item that Stackhand supervises as a service or one-shot command.
_Avoid_: Resource, task

**Service**:
A Process that is expected to stay active until it is stopped.
_Avoid_: Daemon, long-running task

**One-shot**:
A Process that is expected to complete and exit.
_Avoid_: Job, init task

**Run**:
One execution attempt of a Process.
_Avoid_: Instance, execution instance

**Process Tree**:
The root operating-system process and the owned descendant processes for one Run.
_Avoid_: Process, child list

**Project**:
The effective set of Processes and dependencies loaded from configuration for one Stackhand session.
_Avoid_: Workspace, environment

**Desired State**:
The user's current intent for a Process to be running or stopped.
_Avoid_: Status, lifecycle state

**Automatic Restart Budget**:
The maximum number of automatic retries allowed after an initial Run.
_Avoid_: Consecutive failure count, retry count

**Dependency**:
A startup relationship that prevents one Process from starting until another Process meets a specified condition. It does not couple their lifetimes after startup.
_Avoid_: Parent, prerequisite process
