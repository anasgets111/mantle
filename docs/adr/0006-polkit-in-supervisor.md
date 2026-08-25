---
status: accepted
---

# Run the Polkit agent in the Supervisor, not the lock process

The session-lock process and the Polkit agent both handle authentication, but
they do not share a threat model. Session-lock owns the compositor's
session-lock protocol and gives Lua no hook at all: no challenge event, no
callback, a fully data-only scene. A Polkit challenge (package installs,
other privileged actions) needs a themeable dialog: `action_id`, `message`,
and `cookie` reach Lua so a config can render its own confirmation UI, and
only the password field itself is Rust-native and zeroized. That is a
capability with a bounded snapshot and a command envelope, the same shape as
every other Supervisor-owned capability, not a variant of the lock seam. The
Supervisor registers as the PolicyKit agent
(`org.freedesktop.PolicyKit1.Authority.RegisterAgent`), claimed lazily per
ADR 0005.

## Considered options

- Fold Polkit into the separate lock/auth process, for uniform "authentication
  stays outside Lua" isolation. Rejected: Polkit challenges are not
  data-only. Lua needs to see and style the challenge metadata to build a
  themed dialog, which the lock scene explicitly forbids. Forcing Polkit into
  that seam means either breaking the lock process's no-callback invariant or
  duplicating the capability-authority machinery inside a process designed
  not to need it.
- Give Polkit its own third process. Rejected: no continuity, public
  ownership, or overlap-safe lifetime requirement sets it apart from any
  other durable capability; it fits the existing Supervisor-owned pattern
  without a new process to maintain.

## Consequences

- The "authentication stays outside Lua" claim narrows to the password field
  specifically. Challenge metadata (`action_id`, `message`, `cookie`) is
  ordinary Lua-visible capability state.
- Polkit is a capability like any other: bounded snapshot, generation-guarded
  commands, lazy claim on `polkit:enable_agent()`.
- Session-lock keeps its stricter, callback-free isolation. The two auth
  surfaces are not interchangeable and should not be merged later for
  consistency's sake.
