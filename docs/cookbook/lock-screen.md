# Lock screen

A lock screen on every monitor with a large clock, the date and a password card that reports
checking and wrong passwords. It fades in when the session locks and out after a correct password.
It locks from a keybind (`mantle call lock`) and after five minutes idle.

```lua,shot
local FADE_MS = 250

mantle.lock:invoke("set_unlock_animation", FADE_MS)
mantle.idle:register_threshold(300, function() mantle.lock:invoke("lock") end, function() end)
action("lock", function() mantle.lock:invoke("lock") end)

-- Up while locked and not yet unlocking: drives the fade both ways.
local up = mantle.lock:map(function(lock) return lock ~= nil and lock.active and not lock.unlocking end)

local time = mantle.system:map(function(system) return system and os.date("%H:%M", system.time) or "" end)
local date = mantle.system:map(function(system) return system and os.date("%A, %d %B", system.time) or "" end)

local hint = mantle.lock:map(function(lock)
    if lock == nil then
        return ""
    elseif lock.authenticating then
        return "Checking…"
    elseif lock.error ~= "" then
        return lock.attempts > 1 and string.format("Wrong password (%d tries)", lock.attempts) or "Wrong password"
    end
    return "Type your password and press Enter"
end)

local failed = mantle.lock:map(function(lock) return lock ~= nil and lock.error ~= "" and not lock.authenticating end)

local lock_screen = lock {
    id = "lock",
    background = "#11111b",
    child = function(output)
        return column {
            width = "Fill",
            height = "Fill",
            align_h = "Center",
            align_v = "Center",
            spacing = 16,
            background = { gradient = "Linear", angle = 160, stops = { { 0, "#1e1e2e" }, { 1, "#11111b" } } },
            opacity = up:map(function(on) return on and 1 or 0 end),
            animate = { opacity = { duration = FADE_MS, from = 0 } },
            children = {
                text { content = time, font_size = 96, foreground = "#cdd6f4", align_h = "Center" },
                text { content = date, font_size = 20, foreground = "#a6adc8", align_h = "Center" },
                rect { height = 32 },
                text { content = os.getenv("USER") or "", font_size = 16, foreground = "#cdd6f4", align_h = "Center" },
                rect {
                    width = 320,
                    align_h = "Center",
                    padding = { left = 16, right = 16 },
                    radius = 22,
                    background = "#1e1e2e",
                    border_width = 2,
                    border_color = failed:map(function(bad) return bad and "#f38ba8" or "#45475a" end),
                    animate = { border_color = 150 },
                    children = {
                        textfield {
                            width = "Fill",
                            height = 44,
                            font_size = 16,
                            foreground = "#cdd6f4",
                            text_align = "Center",
                            placeholder = "Password",
                            mask_character = "•",
                            secure_submit = { capability = "lock", action = "authenticate" },
                        },
                    },
                },
                text {
                    content = hint,
                    foreground = failed:map(function(bad) return bad and "#f38ba8" or "#6c7086" end),
                    align_h = "Center",
                },
            },
        }
    end,
}

return { lock_screen }
```

## How it works

- Declaring a `lock` does not lock; `mantle.lock:invoke("lock")` does, and only a correct password in the one secure field unlocks ([lock surface](../surfaces/lock.md), [lock capability](../capabilities/lock.md)).
- `secure_submit` sends keystrokes straight to PAM; Lua never sees the password, so the field has no `on_change` or `on_submit` ([secure fields](../guide/input.md#secure-fields)).
- `child = function(output)` gives every monitor its own copy ([per-output child](../surfaces/index.md#per-output-child)).
- `set_unlock_animation` keeps the lock up for `FADE_MS` after success, while `unlocking` fades the card out ([animation](../guide/animation.md)).
- `error`, `attempts` and `authenticating` drive the hint and the red border.
- `action` exposes `mantle call lock` to a keybind, and an idle threshold locks after 300 s without input ([action](../guide/scripting.md#action), [idle](../capabilities/idle.md#methods)).

## Variations

| Change | Edit |
| :--- | :--- |
| Wallpaper behind it | Wrap the column in a `rect { width = "Fill", height = "Fill" }` whose first child is `image { source = "/path/to/wallpaper.jpg", width = "Fill", height = "Fill" }`, and drop the gradient ([image](../nodes/image.md)) |
| Blurred wallpaper | Add `source_blur = 24` to that `image` ([blurs](../guide/paint.md#blurs)) |
| Unlock button | A `button { submit = true, ... }` beside the field sends it like Enter ([pointer](../guide/input.md#pointer)) |
| Clock on one monitor only | `visible = output == "DP-1"` on the clock texts |
| Lock before suspend | Invoke `lock` from your suspend keybind's `action`, then run `systemctl suspend` with [`process.detach`](../guide/processes.md#processdetach) |
