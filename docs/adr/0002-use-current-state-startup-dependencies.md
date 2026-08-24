---
status: accepted
---

# Use current-state startup dependencies without lifetime cascading

Stackhand treats `started` as satisfied only while the prerequisite Run has spawned and is starting or running. A stopping Process does not satisfy a new dependent, because startup must not rely on a prerequisite that is already shutting down. After a dependent starts, later dependency loss does not stop or restart it unless a future explicit lifetime policy changes this rule.

## Considered options

- [dekit](https://github.com/pvolok/mprocs/blob/dcadb69e30446568743a8feffdab8c41360361ca/src/kernel/task.rs#L296-L312) uses a current-state condition, but it also removes support from active dependents.
- [Process Compose](https://github.com/F1bonacc1/process-compose/blob/23b0acacc937d745279fb1551337f4031c4fc865/src/app/process.go#L386-L397) uses a one-way started event that can remain satisfied while the prerequisite stops.
