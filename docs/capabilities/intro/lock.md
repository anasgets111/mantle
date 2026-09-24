The lock screen itself is a [`lock` surface](../surfaces/lock.md).

```lua
button {
    on_click = function() mantle.lock:invoke("lock") end,
    children = { text { content = "Lock" } },
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Ownership | The Supervisor decides lock and unlock; the Renderer holds and paints `ext_session_lock_v1` |
| Triggers | Config `lock`, and logind's `Lock` signal (`loginctl lock-session`). logind's `Unlock` is ignored. Sets `LockedHint` |
| Unlock | Only a successful PAM conversation, in a re-exec'd [worker](../../supervisor/src/pam_worker.rs) using `/etc/pam.d/mantle` or `/usr/lib/pam.d/mantle`, else `login`. A worker silent for 30 s fails the attempt |
| Unlock animation | `set_unlock_animation` delays the release, clamped to 600 ms; kept across reloads |
| Crash | A dead Renderer never unlocks; the replacement retakes the lock. `$XDG_RUNTIME_DIR/mantle/session-locked` carries the fact across a Supervisor restart |
| Refused or lost | Sets [`mantle.rescue`](index.md#renderer-members) with the reason |
| Reload | An edit that would recreate a lock surface is refused while locked ([lock surface](../surfaces/lock.md)) |

See also: [Lock screen](../cookbook/lock-screen.md) recipe.
