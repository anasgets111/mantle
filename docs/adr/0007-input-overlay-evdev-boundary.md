---
status: accepted
---

# Ship a global key-visualizer via unprivileged evdev, not the engine's input pipeline

Wayland's security model has no portal for capturing keyboard events outside
a client's own focused surface, by design. A streaming key-visualizer widget
(showing keys as they are pressed, the screenkey/carnac use case) needs
exactly that: input from whatever window has focus, not just Oblisk's own
surfaces. The only path on Linux without a compositor-specific hack is raw
`evdev`: open `/dev/input/event*`, gated by the user's account belonging to
the `input` group, translate keycodes through `xkbcommon`, and push symbols
into a bounded, debounced ring buffer. This is a deliberate crossing of a
boundary Wayland put there on purpose, taken on for one named caller.

## Considered options

- Scope capture to Oblisk's own surfaces through the existing input pipeline
  (engine hit-tests retained nodes, delivers `on_key`). Rejected: does not
  serve the use case. A stream overlay has to show keys pressed in the game
  or app that currently has focus, not in the shell.
- Run the capture helper as root instead of requiring `input`-group
  membership. Rejected: broader privilege than the task needs; group
  membership is the standard, narrower grant for evdev access.
- Do not build it. Rejected now that there is a named caller (the streaming
  key-visualizer widget); revisit if that caller goes away.

## Consequences

- Install docs must call out `sudo usermod -aG input $USER` as a real
  prerequisite, not an implementation detail.
- The capability claims lazily on `require("oblisk.input_overlay")`, per
  ADR 0005: a shell that never builds the widget never opens an input device.
- Symbols are ephemeral: a 10-entry ring buffer, debounced repeats, never
  logged, written, or included in any snapshot passed to persist or
  `state.json`, matching the same "passwords never enter snapshots, logs,
  IPC, or reload state" bar the lock scene holds itself to.
- This is the one capability in the framework that can observe keystrokes
  typed outside the shell entirely. Treat any future consumer of
  `input_overlay.keys` as sensitive by default.
