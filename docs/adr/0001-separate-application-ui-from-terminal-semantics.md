---
status: accepted
---

# Separate application UI from terminal semantics

Stackhand will use Ratatui for application layout and controls, and `libghostty-vt` for terminal behavior. This division avoids a custom TUI framework and terminal emulator while keeping terminal integration behind a small Stackhand-owned adapter. The terminal prototype must prove that this boundary gives sufficient rendering, input, selection, resize, and packaging behavior before broader implementation continues.

The terminal prototype passed its automated suite and its real-program matrix
on macOS arm64. The evidence supports this boundary. Host input and Linux
validation limits remain, but they do not require a different terminal owner.
See [terminal prototype validation](../implementation/terminal-prototype-validation.md).
