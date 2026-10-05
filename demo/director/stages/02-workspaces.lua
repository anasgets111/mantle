fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local workspaces = list {
    direction = "horizontal",
    spacing = 6,
    align_v = "center",
    source = mantle.workspaces:map(function(ws)
        local output = ws and ws.outputs[1]
        local items = {}
        local active = output and output.active_workspace
        for _, w in ipairs(output and output.workspaces or {}) do
            items[#items + 1] = { id = w.id, label = tostring(w.number or w.name), active = w.id == active }
        end
        return items
    end),
    key = function(w) return tostring(w.id) end,
    itemfn = function(w)
        return rect {
            width = w.active and 68 or 40,
            height = 40,
            radius = 20,
            background = w.active and "#89b4fa" or "#313244",
            animate = { width = { spring = { stiffness = 400, damping = 18 } }, background = 300 },
            on_click = function() mantle.workspaces:focus(w.id) end,
            children = {
                text {
                    content = w.label,
                    align_h = "center",
                    align_v = "center",
                    font_size = 20,
                    foreground = w.active and "#11111b" or "#cdd6f4",
                },
            },
        }
    end,
}

return {
    panel {
        id = "bar",
        layer = "top",
        anchor = { top = true, left = true, right = true },
        exclusive_zone = true,
        width = "fill",
        height = 56,
        background = "#11111be6",
        child = row {
            width = "fill",
            height = "fill",
            padding = { left = 12, right = 12 },
            children = {
                workspaces,
                rect { width = "fill" },
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%a %d %b   %H:%M", s and s.time)
                    end),
                    align_v = "center",
                    font_size = 22,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
