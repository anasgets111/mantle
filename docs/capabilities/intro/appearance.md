Follow the system's dark mode and accent colour instead of hardcoding them:

```lua
local scheme = mantle.appearance:map(function(appearance)
    return appearance and appearance.color_scheme or "default"
end)

rect {
    padding = 8,
    background = scheme:map(function(name)
        return name == "light" and "#EFF1F5" or "#1E1E2E"
    end),
    border = {
        width = 2,
        color = mantle.appearance:map(function(appearance)
            return appearance and appearance.accent or "#89B4FA"
        end),
    },
}
```

<!-- reference -->

## Backend

| Field | Portal key (`org.freedesktop.appearance`) | Default |
| :--- | :--- | :--- |
| `color_scheme` | `color-scheme`: `1` is `"dark"`, `2` is `"light"` | `"default"` |
| `accent` | `accent-color`: an sRGB triple in `[0, 1]`, as `"#rrggbb"` | `nil`, also for a triple out of range |
| `contrast` | `contrast`: `1` is `"high"` | `"normal"` |
| `reduced_motion` | `reduced-motion`: `1` is `true` | `false` |

The source is `org.freedesktop.portal.Settings` on the session bus. Mantle reads the namespace once,
then again on each `SettingChanged` in it and when `org.freedesktop.portal.Desktop` gains or loses an
owner. Any other value of a key, or a key the portal does not provide, is the default.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Every field stays at its default | No portal backend owns `org.freedesktop.portal.Desktop` on the session bus. Install xdg-desktop-portal with a backend that serves Settings (GNOME, KDE and `xdg-desktop-portal-gtk` do) |
| `reduced_motion` is always `false` | The portal predates the `reduced-motion` key |
