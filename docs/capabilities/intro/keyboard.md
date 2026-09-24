```lua
button {
    on_click = function()
        local keyboard = mantle.keyboard:get()
        if keyboard and keyboard.layout_count > 1 then
            mantle.keyboard:invoke("switch_layout", (keyboard.active_layout_index + 1) % keyboard.layout_count)
        end
    end,
    children = {
        text {
            content = mantle.keyboard:map(function(keyboard)
                if keyboard == nil then
                    return ""
                end
                return keyboard.active_layout .. (keyboard.caps_lock and " ⇪" or "")
            end),
        },
    },
}
```

<!-- reference -->

## Backend

| Part | Source | Without it |
| :--- | :--- | :--- |
| Lock keys | `EV_LED` events from the first `/dev/input` device with a Caps Lock LED; a replugged keyboard is reopened | sysfs `*::capslock`, `*::numlock`, `*::scrolllock` read once, then frozen; with none of those, `false` |
| Backlight | Reads sysfs `*::kbd_backlight`, writes through logind's `SetBrightness` | `backlight_pct = -1`; `set_backlight` is logged and ignored |
| Layout | niri's event stream, or Hyprland's `devices` for the keyboard marked `main` (the one typed on last) | `active_layout = ""`, `layout_count = 0`; `switch_layout` is logged and ignored |

The first push comes from the compositor's first layout report. Without niri or Hyprland,
`mantle.keyboard` stays `nil` until a lock key, the backlight or a replugged keyboard pushes.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `caps_lock` never changes | The Supervisor cannot read `/dev/input`, so the sysfs fallback was read once. Add the user to the `input` group |
| `backlight_pct` misses a change another program made | It refreshes only on hardware hotkeys and `set_backlight`. Change it through `set_backlight` |
| `switch_layout` on niri with an index above 255 does nothing | niri takes a `u8`; the call is logged and dropped |

See also: [brightness](brightness.md) for the screen backlight.
