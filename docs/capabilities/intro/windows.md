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

Shares one compositor reader with `workspaces`; the per-compositor sources are on
[its backend table](workspaces.md#backend). A window action the backend lacks is logged and dropped.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Window flags are `nil` | `fullscreen` and `maximized` are `nil` on niri, `minimized` except on wlr; `set_fullscreen` and `set_maximized` are no-ops on niri |

See also: [workspaces](workspaces.md) for the focused window and per-output workspaces.
