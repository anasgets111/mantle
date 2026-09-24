Scroll to change the brightness by 5% a notch:

```lua
button {
    on_wheel = function(_, steps)
        local brightness = mantle.brightness:get()
        if brightness then
            local percent = brightness.percent + math.floor(steps * 5) -- math.floor returns an integer
            mantle.brightness:set(math.max(1, math.min(100, percent)))
        end
    end,
    children = {
        text {
            content = mantle.brightness:map(function(brightness)
                return brightness and string.format("☀ %d%%", brightness.percent) or ""
            end),
        },
    },
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Device | One `/sys/class/backlight` device with `max_brightness > 0`, chosen on the first read: `firmware`, then `platform`, then `raw`, then by name. External monitors are not covered |
| No device | Stays `nil` for good; `set` is logged and ignored |
| Updates | A udev `backlight` watch re-reads sysfs `brightness` and pushes on change. If the watch cannot start, a 30 s poll replaces it |
| Writes | logind's `Session.SetBrightness`, so no udev rule or group is needed. logind refuses it from an inactive session; the refusal is logged |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `set` to `0` writes raw `0`, which turns the backlight off on many panels | Clamp the floor to `1`, as the example does. With `max_brightness` under 50, `1` also rounds to raw `0`: clamp higher |

See also: [keyboard](keyboard.md) for the keyboard backlight.
