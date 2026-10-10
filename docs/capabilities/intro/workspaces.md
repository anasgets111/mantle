```lua
panel {
    id = "bar",
    layer = "top",
    anchor = { top = true, left = true, right = true },
    height = 28,
    child = function(output) -- one instance per monitor, named by connector
        return text {
            content = mantle.workspaces:map(function(workspaces)
                for _, entry in ipairs(workspaces and workspaces.outputs or {}) do
                    if entry.name == output then
                        for _, workspace in ipairs(entry.workspaces) do
                            if workspace.id == entry.active_workspace then
                                return "workspace " .. (workspace.number or workspace.name)
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

The Supervisor picks the compositor once, from `$HYPRLAND_INSTANCE_SIGNATURE`, then `$NIRI_SOCKET`, then `$SWAYSOCK`, then `$MANGO_INSTANCE_SIGNATURE`
([`compositor.rs`](../../supervisor/src/compositor.rs)). One reader feeds both `workspaces` and
[`windows`](windows.md).

| Capability | niri | Hyprland | sway | mango | None |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `workspaces` | IPC event stream | `.socket2.sock` events, then one re-read per burst over `.socket.sock`; a title change alone patches in place | i3 IPC subscription (`workspace`, `window`, `input` events), then `GET_WORKSPACES` and `GET_TREE` re-read per event | `watch all-monitors` snapshots plus one `get all-clients` each | `nil` for the run |
| `windows` | Same event stream | Same re-read | Same re-read | Same snapshots | `zwlr_foreign_toplevel_manager_v1` on its own Wayland connection; `nil` if the protocol is missing or setup takes over 5 s |

Hyprland's, sway's and mango's refusal of a write logs at debug level only (`MANTLE_LOG=debug`); niri's is not logged.

### mango tags

mango has tags, not workspaces: each output has a fixed set (1 to 9 by default) and may show several at once. Every tag is one workspace with `number` = the tag number, no `name`, and `id` = `"<output>:<tag>"` (for example `"DP-1:3"`), listed whether empty or not. With several tags shown, only the lowest is `active_workspace`; `focus` shows just the chosen tag on its output and moves focus there. `special` and `overview_open` are `nil`.

## How do I…

### Draw workspace buttons

Draw `number` or `name`, send `id` ([`list`](../nodes/list.md) builds one button per entry):

```lua
list {
    direction = "horizontal",
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
        return rect {
            padding = { left = 8, right = 8 },
            radius = 6,
            background = active:map(function(is_active) return is_active and "#89B4FA" or "#313244" end),
            on_click = function() mantle.workspaces:focus(workspace.id) end,
            children = { text { content = tostring(workspace.number or workspace.name) } },
        }
    end,
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Labels show large or odd numbers | Draw `number` or `name`, send `id`. `id` is an opaque string: on Hyprland the workspace id in decimal (`"3"`, or negative like `"-1337"` for a named workspace), on niri its own id, on mango `"<output>:<tag>"`, on sway the workspace name (sway focuses and moves by name). Don't do arithmetic on it |
| The strip differs between compositors | Hyprland lists no empty workspace but the active one, and `focus` on an unlisted number creates it (a numbered workspace's `id` is its number as a string, so `focus("7")` works); niri keeps its own empty workspace and ignores an unknown `id`. sway also creates a workspace on `focus` of an unlisted name. Branch on `compositor` |
| `focus` does nothing on sway for some workspace names | Sway reads `next`, `prev`, `current`, `number`, `output`, `gaps`, `back_and_forth` (any case) and names starting `--` as commands, and cannot take a name holding `"`, `\` or `$`. The call is logged at debug level and dropped |
| Actions do nothing on Hyprland older than 0.56 | Writes use 0.56's Lua dispatch syntax; older versions refuse them while reads still work. Update Hyprland; `MANTLE_LOG=debug` shows the refusal |

See also: [Workspaces](../cookbook/workspaces.md) recipe.
