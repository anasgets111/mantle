The lock screen itself is a [`lock` surface](../surfaces/lock.md); this capability locks the
session and relays PAM's conversation. PAM starts when the session locks: `prompt` is its own
question (`"Password: "`, a verification code) and `messages` its info and error lines. The one
[secure field](../guide/input.md#secure-fields) answers every prompt; bind its `masked` to
`not prompt.echo` so a visible answer shows.

With fprintd and an enrolled finger, a fingerprint unlocks too. A second conversation on the
`mantle-fingerprint` PAM service runs beside the password, and the first to succeed unlocks.
`fingerprint` is `nil` while it is off, else `{ messages }` with pam_fprintd's lines such as
"Place your finger". It is briefly non-nil on every lock even without a reader, since the worker
exits within milliseconds, so draw a fingerprint icon only once `fingerprint.messages` is non-empty.

```lua
rect {
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
| Unlock | Only a successful PAM conversation, run in a re-exec'd [worker](../../supervisor/src/pam_worker.rs) with the `mantle` PAM service from `/etc/pam.d` or `/usr/lib/pam.d`, else `login`. PAM must reply within 30 s of an answer; a prompt waits for you without limit. An expired password still unlocks |
| Conversation | Starts on lock and again after a wrong password. A failure before any prompt (a faillock lockout) waits for the next submit. An answer typed before its prompt goes to the next masked prompt; a visible prompt drops it |
| Fingerprint | Starts on lock when `mantle-fingerprint` is in `/etc/pam.d` or `/usr/lib/pam.d`, with no fallback service. pam_fprintd retries misses for the whole lock. Any other end, or a prompt from that stack, turns it off until the next password submit or lock. Misses never count as `attempts` or set `error`. Packages install the stack |
| Unlock animation | `set_unlock_animation` delays the release by up to 600 ms; the value persists across reloads |
| Crash | A dead Renderer never unlocks; the replacement retakes the lock. `$XDG_RUNTIME_DIR/mantle/session-locked` carries the lock across a Supervisor restart |
| Refused or lost | Sets [`mantle.rescue`](index.md#renderer-members) with the reason |
| Reload | An edit that would recreate a lock surface is refused while locked ([lock surface](../surfaces/lock.md)) |

## How do I…

| Task | Answer |
| :--- | :--- |
| Show "wrong password" | `error` and `attempts`: [lock surface example](../surfaces/lock.md) |
| Ask for a second factor | Show `prompt.text` above the field and set `masked = lock:map(function(l) return not (l.prompt and l.prompt.echo) end, true)` |
| Show a lockout notice | `messages`, each `{ kind = "info" or "error", text }`, cleared by the next answer |
| Show a fingerprint hint | `fingerprint.messages`, the same shape, when `fingerprint` is not `nil`: [Lock screen](../cookbook/lock-screen.md) recipe |
| Turn fingerprint off | Replace the content of `/etc/pam.d/mantle-fingerprint` with `auth requisite pam_deny.so`. An empty file falls back to PAM's `other` stack, and a deleted one returns with the next upgrade |
| Animate the lock screen out | `set_unlock_animation` with the animation's length, then drive the fade from `unlocking`: [Lock screen](../cookbook/lock-screen.md) recipe |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Removing `set_unlock_animation` from the config keeps the old delay | The value outlives reloads. Call `set_unlock_animation` with no argument to reset it to `0` |
| A 1 s out-animation is cut short | The delay clamps to 600 ms. Keep the animation within it |
| With `pam_fprintd` inside the password stack (`system-auth`, as Fedora's authselect sets), each submit restarts its scan | Use the `mantle-fingerprint` stack instead, and point `/etc/pam.d/mantle` at a stack without `pam_fprintd` |
| `fingerprint` turns `nil` with "device claimed" after a quick relock, or after suspend | The reader was still busy. Submit a password, even a wrong one, to retry, or lock again |

See also: [Lock screen](../cookbook/lock-screen.md) recipe; [idle](idle.md) to lock after inactivity.
