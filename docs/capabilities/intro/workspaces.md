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

The Supervisor picks the compositor once, from `$HYPRLAND_INSTANCE_SIGNATURE`, then `$NIRI_SOCKET`
([`compositor.rs`](../../supervisor/src/compositor.rs)). One reader feeds both `workspaces` and
[`windows`](windows.md).

| Capability | niri | Hyprland | Neither |
| :--- | :--- | :--- | :--- |
| `workspaces` | IPC event stream | `.socket2.sock` events, then one re-read per burst over `.socket.sock`; a title change alone patches in place | `nil` for the run |
| `windows` | Same event stream | Same re-read | `zwlr_foreign_toplevel_manager_v1` on its own Wayland connection; `nil` if the protocol is missing or setup takes over 5 s |

Hyprland's refusal of a write logs at debug level only (`MANTLE_LOG=debug`); niri's is not logged.

## How do I…

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
            on_click = function() mantle.workspaces:focus(workspace.id) end,
            children = { text { content = tostring(workspace.idx) } },
        }
    end,
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Labels show large or odd numbers on niri | Draw `idx`, send `id`. niri's `id` is opaque |
| The strip differs between compositors | Hyprland lists no empty workspace but the active one, and `focus` on a missing number creates it; niri keeps its own empty workspace and ignores an unknown `id`. Branch on `compositor` |
| Actions do nothing on Hyprland older than 0.56 | Writes use 0.56's Lua dispatch syntax; older versions refuse them while reads still work. Update Hyprland; `MANTLE_LOG=debug` shows the refusal |

See also: [Workspaces](../cookbook/workspaces.md) recipe.
