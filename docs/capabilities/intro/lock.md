The lock screen itself is a [`lock` surface](../surfaces/lock.md); this capability locks the
session and reports the password attempt.

```lua
button {
    on_click = function() mantle.lock:lock() end,
    children = { text { content = "Lock" } },
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Ownership | The Supervisor decides lock and unlock; the Renderer holds and paints `ext_session_lock_v1`. Built at boot, unlike other capabilities |
| Triggers | The `lock` action, and logind's `Lock` signal (`loginctl lock-session`). logind's `Unlock` is logged and ignored. Each lock and unlock sets logind's `LockedHint` |
| Unlock | Only a successful PAM conversation, run in a re-exec'd [worker](../../supervisor/src/pam_worker.rs) with the `mantle` PAM service from `/etc/pam.d` or `/usr/lib/pam.d`, else `login`. An exchange that takes over 30 s fails |
| Unlock animation | `set_unlock_animation` delays the release by up to 600 ms; the value persists across reloads |
| Crash | A dead Renderer never unlocks; the replacement retakes the lock. `$XDG_RUNTIME_DIR/mantle/session-locked` carries the lock across a Supervisor restart |
| Refused or lost | Sets [`mantle.rescue`](index.md#renderer-members) with the reason |
| Reload | An edit that would recreate a lock surface is refused while locked ([lock surface](../surfaces/lock.md)) |

## How do I…

| Task | Answer |
| :--- | :--- |
| Show "wrong password" | `error` and `attempts`: [lock surface example](../surfaces/lock.md) |
| Animate the lock screen out | `set_unlock_animation` with the animation's length, then drive the fade from `unlocking`: [Lock screen](../cookbook/lock-screen.md) recipe |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Removing `set_unlock_animation` from the config keeps the old delay | The value outlives reloads. Call `set_unlock_animation` with no argument to reset it to `0` |
| A 1 s out-animation is cut short | The delay clamps to 600 ms. Keep the animation within it |

See also: [Lock screen](../cookbook/lock-screen.md) recipe; [idle](idle.md) to lock after inactivity.
