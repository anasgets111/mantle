---
status: accepted
---

# Own MPRIS from the durable Supervisor

The Supervisor owns the first MPRIS implementation, not a renderer
generation. It discovers `org.mpris.MediaPlayer2.*` names, subscribes to
`PropertiesChanged`, and caches normalized track metadata independent of any
renderer generation. On `RegisterCapability(MPRIS)` it pushes the cached
state immediately, so the new generation's first frame carries correct media
info. Control commands stay generation-guarded through the capability
authority (ADR 0003); a command from a superseded epoch is dropped.

## Considered options

- Renderer-local first slice, promoted to a durable owner later only if
  continuity, public ownership, or overlap-safe lifetime became a real caller
  requirement. Rejected: a renderer-local backend drops and reconnects D-Bus
  player subscriptions on every reload, producing a visible stutter and a
  blank media widget on the new generation's first frame. That is a real
  requirement from the first fixture that renders media state, not a
  hypothetical future caller.
- Durable but off the shared capability authority. Rejected for the same
  reason ADR 0003 exists: stale commands need one common rejection path.

## Consequences

- The first capability slice has one durable, generation-independent backend
  owner.
- Reload never reconnects MPRIS state.
- Placement is decided up front for capabilities like tray and notifications
  that share the same reload-continuity requirement: durable by default,
  renderer-local only when a real reason argues for it.
