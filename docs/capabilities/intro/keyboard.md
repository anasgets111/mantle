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

| Part | Source |
| :--- | :--- |
| Lock keys | LEDs of the first evdev device with `LED_CAPSL`; sysfs read once as fallback |
| Backlight | `*::kbd_backlight` via logind; `-1` without one |
| Layout | The compositor (niri or Hyprland). Empty elsewhere. On Hyprland, `switch_layout` sends `switchxkblayout main <i>` to the keyboard marked `main` |
