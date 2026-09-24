# Workspaces

A bar on every monitor listing that monitor's workspaces, on Hyprland and niri alike. The active
workspace is a wide pill, occupied ones are brighter, a click focuses one and the wheel steps
through them. On Hyprland, special workspaces get their own toggles.

```lua,shot
-- The workspaces of one output, `nil` until the compositor answers.
local function output_of(workspaces, name)
    for _, output in ipairs(workspaces and workspaces.outputs or {}) do
        if output.name == name then
            return output
        end
    end
end

local function focus_step(name, step)
    local output = output_of(mantle.workspaces:get(), name)
    if output == nil then
        return
    end
    for index, workspace in ipairs(output.workspaces) do
        if workspace.id == output.active_workspace then
            local target = output.workspaces[index + step]
            if target then
                mantle.workspaces:focus(target.id)
            end
            return
        end
    end
end

local function strip(name)
    return button {
        align_v = "Center",
        -- Wheel up is positive; step towards lower numbers.
        on_wheel = function(_, steps) focus_step(name, steps > 0 and -1 or 1) end,
        children = {
            list {
                direction = "Horizontal",
                spacing = 4,
                align_v = "Center",
                source = mantle.workspaces:map(function(workspaces)
                    local output = output_of(workspaces, name)
                    local items = {}
                    for index, workspace in ipairs(output and output.workspaces or {}) do
                        items[index] = {
                            id = workspace.id,
                            label = tostring(workspace.idx), -- draw idx, send id
                            active = workspace.id == output.active_workspace,
                            populated = workspace.populated,
                        }
                    end
                    return items
                end),
                key = function(item) return tostring(item.id) end,
                itemfn = function(item)
                    return button {
                        width = item.active and 36 or 22,
                        height = 20,
                        radius = 10,
                        background = item.active and "#89b4fa" or (item.populated and "#45475a" or "#313244"),
                        animate = { width = { duration = 180, easing = "OutCubic" }, background = 180 },
                        on_click = function() mantle.workspaces:focus(item.id) end,
                        children = {
                            text {
                                content = item.label,
                                align_h = "Center",
                                align_v = "Center",
                                font_size = 11,
                                foreground = item.active and "#1e1e2e" or "#cdd6f4",
                            },
                        },
                    }
                end,
            },
        },
    }
end

-- Hyprland's scratchpads; `special` is nil on niri, so this list is empty there.
local specials = list {
    direction = "Horizontal",
    spacing = 4,
    align_v = "Center",
    source = mantle.workspaces:map(function(workspaces)
        return workspaces and workspaces.special or {}
    end),
    key = function(special) return special.name end,
    itemfn = function(special)
        local short = special.name:gsub("^special:?", "") -- "special:scratch" -> "scratch"
        return button {
            padding = { left = 8, right = 8, top = 2, bottom = 2 },
            radius = 10,
            background = special.shown_on and "#f9e2af" or "#313244",
            on_click = function() mantle.workspaces:toggle_special(special.name) end,
            children = {
                text {
                    content = short ~= "" and short or "special",
                    font_size = 11,
                    foreground = special.shown_on and "#1e1e2e" or "#cdd6f4",
                },
            },
        }
    end,
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        width = "Fill",
        height = 32,
        exclusive = true,
        child = function(output) -- one instance per monitor, named by connector
            return row {
                width = "Fill",
                height = "Fill",
                spacing = 12,
                padding = { left = 8, right = 8 },
                background = "#1e1e2e",
                children = { strip(output), specials },
            }
        end,
    },
}
```

## How it works

- `child = function(output)` builds one bar per monitor and passes its connector name ([per-output child](../surfaces/index.md#per-output-child)).
- `outputs[].name` matches that connector; `active_workspace` is the `id` shown there ([workspaces](../capabilities/workspaces.md)).
- The `list` rebuilds its buttons from the mapped array, and `key` keeps each button's tweens when workspaces come and go ([list](../nodes/list.md)).
- Labels draw `idx` and clicks send `id`: niri's ids are opaque ([workspaces gotchas](../capabilities/workspaces.md#gotchas)).
- `on_wheel` on the outer button reads the live state with `:get()` inside the handler ([pointer](../guide/input.md#pointer)).
- The `width` tween makes the active pill grow in place ([animation](../guide/animation.md)).

## Variations

| Change | Edit |
| :--- | :--- |
| Always show workspaces 1 to 5 on Hyprland | Pad `items` with `{ id = n, label = tostring(n), active = false, populated = false }` for missing numbers; `focus` creates them |
| App icons instead of numbers | Carry `app_id = workspace.app_id` into `items` and draw `icon { name = item.app_id or "", size = 14 }`. Where the icon name differs from the `app_id`, read `entries[by_app_id[app_id]].icon` from `mantle.applications` |
| Named workspaces | `label = workspace.name or tostring(workspace.idx)`, with `min_width` and side `padding` instead of a fixed `width` |
| Dots only | Drop the `text` and set `width = item.active and 20 or 8, height = 8` |
| Show the focused window's title | Add `text { content = mantle.workspaces:map(function(workspaces) return workspaces and workspaces.active_client and workspaces.active_client.title or "" end), elide = "End", max_width = 400 }` |
| Vertical bar | Anchor `left`, set `width = 40`, `height = "Fill"`, use a `column` and `direction = "Vertical"` |
