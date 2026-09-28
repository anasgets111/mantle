fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local launcher_open = state("launcher_open", false)
local osd = require("osd")
local notifications = require("notifications")
local privacy = require("privacy")
local idle = require("idle")

local workspaces = list {
    direction = "Horizontal",
    spacing = 6,
    align_v = "Center",
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
        return button {
            width = w.active and 68 or 40,
            height = 40,
            radius = 20,
            background = w.active and "#89b4fa" or "#313244",
            animate = { width = { duration = 300, easing = "OutCubic" }, background = 300 },
            on_click = function() mantle.workspaces:focus(w.id) end,
            children = {
                text {
                    content = tostring(w.idx),
                    align_h = "Center",
                    align_v = "Center",
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
        if index then out[#out + 1] = a.entries[index] end
    end
    return out
end)

local launcher = panel {
    id = "launcher",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = { top = 12, left = 12 },
    visible = launcher_open,
    width = 560,
    background = "#313244f2",
    radius = 24,
    child = list {
        width = "Fill",
        padding = 10,
        spacing = 2,
        source = apps,
        key = function(app) return app.id end,
        itemfn = function(app)
            return button {
                width = "Fill",
                padding = 12,
                radius = 12,
                on_click = function() mantle.applications:launch(app.id) end,
                children = {
                    row {
                        spacing = 14,
                        children = {
                            icon { name = app.icon or "application-x-executable", size = 52 },
                            text { content = app.name, align_v = "Center", font_size = 26 },
                        },
                    },
                },
            }
        end,
    },
}

local aurora = panel {
    id = "aurora",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    exclusive = "Ignore",
    child = shader {
        width = "Fill",
        height = "Fill",
        source = mantle.config_dir .. "/aurora.frag",
        params = { tint_a = { 0.54, 0.71, 0.98 }, tint_b = { 0.80, 0.65, 0.97 } },
        progress = 0,
        animate = {
            progress = { duration = 16000, easing = "Linear", keyframes = { 0, 1 }, loops = "Infinite" },
        },
    },
}

return {
    aurora,
    osd,
    notifications,
    idle.away,
    launcher,
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        width = "Fill",
        height = 56,
        background = "#11111be6",
        child = row {
            width = "Fill",
            height = "Fill",
            padding = { left = 12, right = 12 },
            children = {
                workspaces,
                rect { width = "Fill" },
                idle.indicator,
                privacy,
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%a %d %b   %H:%M", s and s.time)
                    end),
                    align_v = "Center",
                    font_size = 22,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
