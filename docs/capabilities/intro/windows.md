```lua
list {
    source = mantle.windows:map(function(windows)
        return windows and windows.windows or {}
    end),
    key = function(window) return window.id end,
    itemfn = function(window)
        return button {
            on_click = function() mantle.windows:invoke("focus", window.id) end,
            children = {
                text { content = window.title, foreground = window.focused and "#89B4FA" or "#CDD6F4" },
            },
        }
    end,
}
```

<!-- reference -->

## Backend

niri and Hyprland share the `workspaces` reader; any other compositor needs
`zwlr_foreign_toplevel_manager_v1` ([backend table](workspaces.md#backend)).

| Backend | Reports | Writes |
| :--- | :--- | :--- |
| niri | `floating` | `focus`, `close` |
| Hyprland | `floating`, `fullscreen`, `maximized` | `focus`, `close`, `set_fullscreen`, `set_maximized` |
| wlr foreign-toplevel | `fullscreen`, `maximized`, `minimized` | Every action |

A flag a backend does not report is `nil`; an action it lacks is logged at debug level and dropped.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `if window.fullscreen == false` never matches on niri | The flag is `nil` there. Test truthiness, or branch on `source` |
| `output` is `nil` for a window on a monitor plugged in after startup | wlr binds outputs once, at connect. Restart the Supervisor after a hotplug if a dock sorts by `output` |

See also: [workspaces](workspaces.md) for the focused window and per-output workspaces.
