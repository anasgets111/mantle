```lua
panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 28,
    child = function(output) -- one instance per monitor, named by connector
        return text {
            content = mantle.workspaces:map(function(workspaces)
                for _, entry in ipairs(workspaces and workspaces.outputs or {}) do
                    if entry.name == output then
                        for _, workspace in ipairs(entry.workspaces) do
                            if workspace.id == entry.active_workspace then
                                return "workspace " .. workspace.idx
                            end
                        end
                    end
                end
                return ""
            end),
        }
    end,
}
```

<!-- reference -->

## Backend

The compositor is probed once, from `$HYPRLAND_INSTANCE_SIGNATURE` then `$NIRI_SOCKET`.
`workspaces` and `windows` share one reader.

| Capability | niri | Hyprland | Other |
| :--- | :--- | :--- | :--- |
| `workspaces` | IPC event stream; `overview_open` | Event (`.socket2.sock`) and command (`.socket.sock`) sockets; special workspaces, `is_fullscreen` | `nil` |
| `windows` | Same event stream | Same sockets | `zwlr_foreign_toplevel_management_v1` on its own connection (5 s setup limit); outputs bound once |

Each workspace action opens a fresh compositor socket. The probe is in
[`compositor.rs`](../../supervisor/src/compositor.rs).

## How do I…

| Task | Answer |
| :--- | :--- |
| Give each monitor its own bar and workspaces | A function `child` gets the connector name; match it in `workspaces.outputs`, as in the example above |

### Draw workspace buttons

Draw `idx`, send `id` ([`list`](../nodes/list.md) builds one button per entry):

```lua
list {
    direction = "Horizontal",
    spacing = 4,
    source = mantle.workspaces:map(function(workspaces)
        local output = workspaces and workspaces.outputs[1]
        return output and output.workspaces or {}
    end),
    key = function(workspace) return tostring(workspace.id) end,
    itemfn = function(workspace)
        local active = mantle.workspaces:map(function(workspaces)
            local output = workspaces and workspaces.outputs[1]
            return output ~= nil and output.active_workspace == workspace.id
        end)
        return button {
            padding = { left = 8, right = 8 },
            radius = 6,
            background = active:map(function(is_active) return is_active and "#89B4FA" or "#313244" end),
            on_click = function() mantle.workspaces:invoke("focus", workspace.id) end,
            children = { text { content = tostring(workspace.idx) } },
        }
    end,
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Workspace labels show large or odd numbers on niri | `id` is opaque on niri; draw `idx` (niri: 1-based position per output; Hyprland: the workspace number, equal to `id`) |
| Workspace strip differs between compositors | Hyprland lists no empty workspaces and `focus` on a missing number creates it; niri ignores unknown ids. `special` is `nil` on niri, `overview_open` `nil` on Hyprland. Branch on `workspaces.compositor` |

See also: [Workspaces](../cookbook/workspaces.md) recipe; [windows](windows.md) for every toplevel.
