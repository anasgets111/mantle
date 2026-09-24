# lock

The session lock screen: one `ext_session_lock_surface_v1` per connected output, covering it for as
long as the compositor holds the session locked. Declaring a `lock` does not lock;
`mantle.lock:invoke("lock")` does, and only a correct password typed into its secure field unlocks.
The lock's state (`active`, `authenticating`, `attempts`, `error`, `unlocking`) and actions are on
the [lock capability](../capabilities/lock.md). Rules every role shares are in [surfaces](index.md).

```lua,shot
mantle.lock:invoke("set_unlock_animation", 250)

local up = mantle.lock:map(function(lock) return lock ~= nil and lock.active and not lock.unlocking end)

local hint = mantle.lock:map(function(lock)
    if lock == nil then return "" end
    if lock.error ~= "" then return string.format("%s (%d)", lock.error, lock.attempts) end
    if lock.authenticating then return "Checking…" end
    return lock.active and "Enter your password" or "Locking…"
end)

local lock_screen = lock {
    id = "lock",
    background = "#11111b",
    child = function(output)
        return column {
            width = "Fill", height = "Fill", align_v = "Center", spacing = 12,
            opacity = up:map(function(on) return on and 1 or 0 end),
            animate = { opacity = { duration = 200, from = 0 } },
            children = {
                text { content = output, foreground = "#6c7086", align_h = "Center" },
                rect {
                    width = 320, align_h = "Center", padding = 8, radius = 18, background = "#1e1e2e",
                    children = {
                        textfield {
                            width = "Fill",
                            height = 20,
                            placeholder = "Password",
                            mask_character = "•",
                            secure_submit = { capability = "lock", action = "authenticate" },
                        },
                    },
                },
                text { content = hint, foreground = "#a6adc8", align_h = "Center" },
            },
        }
    end,
}

action("lock", function() mantle.lock:invoke("lock") end)

return { lock_screen }
```

`mantle call lock` from a keybind ([action](../guide/scripting.md#action)) locks the session. Each
output gets its own card; the field is typable as soon as the compositor gives the lock the
keyboard, with no click. After a correct password the lock stays up 250 ms while the card fades
out, then the session unlocks.

## Properties

A `lock` takes `id`, `child` and the [common and box node properties](../nodes/index.md), minus
the ones the protocol owns.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id` |
| `child` | `Node\|fun(output: string): Node?\|Bound` | None | The root's content. A function runs per output instance with its connector name; `nil` leaves that instance empty ([per-output child](index.md#per-output-child)) |
| `width` | `nil` | None | Refused: the lock covers each output |
| `height` | `nil` | None | Refused, as `width` |
| `visible` | `nil` | None | Refused: the session lock decides when it shows |
<!-- End of the generated table. -->

`monitor` and `anchor` are refused too: the protocol owns coverage and lifetime. The root fills the
output whatever its common properties say; give children `"Fill"` to cover it. A rename of `id` is
refused while the session is locked (a warning in `mantle log`); save again after unlocking. A
config declares at most one `lock`; two are refused at evaluation.

## When a lock is refused

`mantle.lock:invoke("lock")` checks the lock screen before asking the compositor, because the
compositor does not unlock when the client dies: a lock screen with no way to type a password
leaves only a VT switch. A refusal leaves the session unlocked and puts the reason in
`mantle.lock`'s `error` and in [`mantle.rescue`](../capabilities/index.md#renderer-members).

| Condition | Result |
| :--- | :--- |
| The config declares no `lock` | Refused |
| No lock instance holds exactly one shown `textfield` with `secure_submit = { capability = "lock", action = "authenticate" }` (none, two, or hidden) | Refused |
| Another client already holds the session lock | The compositor denies it; reported in `error` |
| While locked, a reload leaves no lock instance with that single field | The reload is refused and the lock screen on screen stays |
| The compositor ends a held lock by its own mechanism | The session is unlocked; the reason goes to `mantle.rescue` only |

The password never reaches Lua: the field's keystrokes go straight to PAM. Give it no `on_change`
or `on_submit`. See [secure fields](../guide/input.md#secure-fields).

## How do I…

| Task | Answer |
| :--- | :--- |
| Lock from a keybind | `action("lock", ...)` as in the example, then `mantle call lock` |
| Show "wrong password" | Read `error` and `attempts` from `mantle.lock`, as the example's `hint` does |
| Show that PAM is checking | Read `authenticating` |
| Animate the lock screen out | `set_unlock_animation` with the animation's length, and drive `opacity` or `scale` from `unlocking` ([lock capability](../capabilities/lock.md)) |
| Animate it in | Drive the same property from `active`; `animate.from` covers the first frame |
| Show the desktop wallpaper behind it | An `image` in the per-output `child`, keyed by `output` ([per-output content](panel.md#per-output-content)) |
| Put a clock on it | `mantle.system:map(function(system) return system and os.date("%H:%M", system.time) or "" end)` |
| Add an unlock button beside the field | A `button { submit = true }` sends the field like Enter ([pointer](../guide/input.md#pointer)) |
| Lock before suspend | Invoke `lock` from your idle or suspend handler ([idle](../capabilities/idle.md)) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `mantle call lock` does nothing | Read `mantle.lock`'s `error`: the config declares no `lock`, or its tree lacks exactly one shown secure field |
| Two secure fields in one lock tree make the lock refuse | One shown field per output instance; hide the others |
| A reload while locked is ignored | It removed the lock's password field; the running lock screen stays. Fix the file |
| Renaming the lock's `id` while locked is refused | Save again after unlocking |
| `visible`, `width`, `height`, `monitor` or `anchor` on a `lock` is refused | Remove them; the lock always covers every output |
| The card's exit animation is cut off | The session unlocks when the `set_unlock_animation` time ends; make it at least the animation's length |
| A second `lock` declaration is refused | A config has at most one |

See also: [lock capability](../capabilities/lock.md), [secure fields](../guide/input.md#secure-fields),
[surfaces](index.md), [animation](../guide/animation.md).

Source: [lock spec](../../renderer/src/layout/node/spec.rs),
[at most one lock](../../renderer/src/lua/surfaces.rs),
[session lock](../../renderer/src/wayland/lock.rs),
[authenticate check](../../renderer/src/layout/secure_submit.rs),
[rename while locked](../../renderer/src/wayland/output.rs).
