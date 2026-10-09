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

local open = state("launcher_open", false)
local query = state("launcher_query", "")

local PINNED = {
    "kitty", "dev.zed.Zed", "org.gnome.Nautilus", "helium",
    "org.telegram.desktop", "vesktop", "steam",
}

-- Pinned apps until you type, then the best fuzzy matches.
local apps = computed({ mantle.applications, query }, function(a, q)
    local out = {}
    if q == "" then
        for _, id in ipairs(PINNED) do
            local index = a and a.by_app_id[id]
            if index and not a.entries[index].no_display then out[#out + 1] = a.entries[index] end
        end
        return out
    end
    for _, app in ipairs(a and a.entries or {}) do
        local score = fuzzy(app.name, q)
        if not app.no_display and score then out[#out + 1] = { app = app, score = score } end
    end
    table.sort(out, function(l, r)
        return l.score > r.score or (l.score == r.score and l.app.id < r.app.id)
    end)
    local best = {}
    for k = 1, math.min(#out, 6) do
        best[k] = out[k].app
    end
    return best
end)

local launcher = panel {
    id = "launcher",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = { top = 12, left = 12 },
    visible = open,
    keyboard_interactivity = "on_demand",
    width = 560,
    background = theme.fade("surface", "70"),
    radius = 24,
    behind_blur = true,
    child = column {
        width = "fill",
        padding = 10,
        spacing = 6,
        children = {
            textfield {
                focus_target = focus_target("search"),
                width = "fill",
                margin = 12,
                font_size = 26,
                placeholder = "Search apps",
                foreground = theme.text,
                placeholder_color = theme.muted,
                caret = { color = theme.accent },
                autofocus = true,
                on_change = function(q) query:set(q) end,
            },
            list {
                width = "fill",
                spacing = 2,
                source = apps,
                key = function(app) return app.id end,
                itemfn = function(app)
                    return row {
                        width = "fill",
                        padding = 12,
                        radius = 12,
                        opacity = 1,
                        animate = { move = 180, opacity = { duration = 180, from = 0 } },
                        on_click = function() mantle.applications:launch(app.id) end,
                        spacing = 14,
                        children = {
                            icon { name = app.icon or "application-x-executable", size = 52 },
                            text { content = app.name, align_v = "center", font_size = 26 },
                        },
                    }
                end,
            },
        },
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
