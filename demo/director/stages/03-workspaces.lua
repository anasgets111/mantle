fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

-- Catppuccin Mocha for now; `fade` appends an alpha.
local theme = {
    crust = "#11111b",
    base = "#1e1e2e",
    surface = "#313244",
    muted = "#6c7086",
    text = "#cdd6f4",
    accent = "#89b4fa",
}
function theme.fade(role, alpha) return theme[role] .. alpha end

require("targets")

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
            -- Numbered only when active, the rest dots, so a dozen workspaces leave the bar room.
            width = w.active and 48 or 14,
            height = w.active and 32 or 14,
            radius = w.active and 16 or 7,
            align_v = "center",
            background = w.active and theme.accent or theme.surface,
            animate = {
                width = { spring = { stiffness = 400, damping = 18 } },
                height = { spring = { stiffness = 400, damping = 18 } },
                background = 300,
            },
            on_click = function() mantle.workspaces:focus(w.id) end,
            children = {
                text {
                    visible = w.active,
                    content = w.label,
                    align_h = "center",
                    align_v = "center",
                    font_size = 18,
                    foreground = w.active and theme.crust or theme.text,
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
        background = theme.fade("crust", "e6"),
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
                    foreground = theme.text,
                },
            },
        },
    },
}
