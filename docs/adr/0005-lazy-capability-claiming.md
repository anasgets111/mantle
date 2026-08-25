---
status: accepted
---

# Claim singleton system resources lazily, never at boot

The Supervisor could register `org.freedesktop.Notifications`,
`org.kde.StatusNotifierWatcher`, and the PolicyKit agent unconditionally at
boot, the same way it owns generation IDs from the start. Instead each claim
is deferred to the first `require("oblisk.<name>")` (or equivalent init call)
in a generation. A shell that never builds a notification widget, tray
drawer, or polkit dialog leaves the matching system default (dunst, mako,
polkit-gnome) fully in control. Background hardware pollers (audio, battery,
brightness) already start lazily for zero-idle-footprint reasons; this
extends the same rule to singleton D-Bus names, where getting it wrong is
user-visible (two competing notification daemons, a stolen tray) rather than
just wasted CPU.

## Considered options

- Claim these names at Supervisor boot. Rejected: a user who runs Oblisk for
  its bar and reload model but keeps `dunst` for notifications would silently
  lose their notification daemon the moment the Supervisor starts, not when
  their config asks for it.
- Claim at boot only if the entry config statically references the
  capability, instead of waiting for the runtime `require()`. Rejected: a
  second, weaker activation path that can drift from what `require()` actually
  triggers, when the runtime signal is already what the capability-command
  channel depends on (ADR 0003).

## Consequences

- Every singleton-resource capability (notifications, tray, Polkit agent,
  wallpaper surface, launcher indexer) needs an explicit activation trigger
  tied to `require()`, not a boot-time init list.
- The Supervisor tracks per-capability claim state independent of any
  generation, since ownership outlives reload.
- A user mixing Oblisk with another notification daemon, tray host, or polkit
  agent sees no conflict as long as they never load that Oblisk capability.
