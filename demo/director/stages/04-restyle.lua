fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local launcher_open = state("launcher_open", false)

local workspaces = list {
    direction = "horizontal",
    spacing = 6,
    align_v = "center",
    source = mantle.workspaces:map(function(ws)
        local output = ws and ws.outputs[1]
        local items = {}
        for _, w in ipairs(output and output.workspaces or {}) do
            items[#items + 1] = { id = w.id, idx = w.idx, active = w.id == output.active_workspace }
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
            animate = { width = { duration = 300, easing = "out_cubic" }, background = 300 },
            on_click = function() mantle.workspaces:focus(w.id) end,
            children = {
                text {
                    content = tostring(w.idx),
                    align_h = "center",
                    align_v = "center",
                    font_size = 20,
                    foreground = w.active and "#11111b" or "#cdd6f4",
                },
            },
        }
    end,
}

local PINNED = {
    "kitty", "dev.zed.Zed", "org.gnome.Nautilus", "helium",
    "org.telegram.desktop", "vesktop", "steam",
}

local apps = mantle.applications:map(function(a)
    local out = {}
    for _, id in ipairs(PINNED) do
        local index = a and a.by_app_id[id]
        if index and not a.entries[index].no_display then out[#out + 1] = a.entries[index] end
    end
    return out
end)

local launcher = panel {
    id = "launcher",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = { top = 12, left = 12 },
    visible = launcher_open,
    width = 560,
    background = "#31324470",
    radius = 24,
    behind_blur = true,
    child = list {
        width = "fill",
        padding = 10,
        spacing = 2,
        source = apps,
        key = function(app) return app.id end,
        itemfn = function(app)
            return row {
                width = "fill",
                padding = 12,
                radius = 12,
                on_click = function() mantle.applications:launch(app.id) end,
                spacing = 14,
                children = {
                    icon { name = app.icon or "application-x-executable", size = 52 },
                    text { content = app.name, align_v = "center", font_size = 26 },
                },
            }
        end,
    },
}

return {
    launcher,
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
