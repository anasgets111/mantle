fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local launcher_open = state("launcher_open", false)
local query = state("launcher_query", "")
local theme = require("theme")
local wallpaper = require("wallpaper")
local taskbar = require("taskbar")
local overview = require("overview")
local osd = require("osd")
local media = require("media")
local tray = require("tray")
local control = require("control")
local notifications = require("notifications")
local privacy = require("privacy")
local idle = require("idle")
local updates = require("updates")
local polkit = require("polkit")

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
        return rect {
            width = w.active and 68 or 40,
            height = 40,
            radius = 20,
            background = w.active and theme.accent or theme.surface,
            animate = { width = { duration = 300, easing = "OutCubic" }, background = 300 },
            on_click = function() mantle.workspaces:focus(w.id) end,
            children = {
                text {
                    content = tostring(w.idx),
                    align_h = "Center",
                    align_v = "Center",
                    font_size = 20,
                    foreground = w.active and theme.crust or theme.text,
                },
            },
        }
    end,
}

local PINNED = {
    "kitty", "dev.zed.Zed", "org.gnome.Nautilus", "helium",
    "org.telegram.desktop", "vesktop", "steam",
}

-- Pinned apps until you type, then the best fuzzy matches.
local apps = computed({ mantle.applications, query }, function(a, needle)
    local out = {}
    if needle == "" then
        for _, id in ipairs(PINNED) do
            local index = a and a.by_app_id[id]
            if index and not a.entries[index].no_display then out[#out + 1] = a.entries[index] end
        end
        return out
    end
    for _, entry in ipairs(a and a.entries or {}) do
        local score = fuzzy(entry.name, needle)
        if not entry.no_display and score then out[#out + 1] = { entry = entry, score = score } end
    end
    table.sort(out, function(l, r) return l.score > r.score or (l.score == r.score and l.entry.id < r.entry.id) end)
    local best = {}
    for k = 1, math.min(#out, 6) do
        best[k] = out[k].entry
    end
    return best
end)

local launcher = panel {
    id = "launcher",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = { top = 12, left = 12 },
    visible = launcher_open,
    width = 560,
    background = theme.fade("surface", "70"),
    radius = 24,
    blur = true,
    child = column {
        width = "Fill",
        padding = 10,
        spacing = 6,
        children = {
            text {
                content = query:map(function(q) return q == "" and "Search apps" or q end),
                padding = 12,
                font_size = 26,
                foreground = theme.text,
                opacity = query:map(function(q) return q == "" and 0.45 or 1 end),
            },
            list {
                width = "Fill",
                spacing = 2,
                source = apps,
                key = function(app) return app.id end,
                itemfn = function(app)
                    return row {
                        width = "Fill",
                        padding = 12,
                        radius = 12,
                        on_click = function() mantle.applications:launch(app.id) end,
                        spacing = 14,
                        children = {
                            icon { name = app.icon or "application-x-executable", size = 52 },
                            text { content = app.name, align_v = "Center", font_size = 26 },
                        },
                    }
                end,
            },
        },
    },
}

local aurora = panel {
    id = "aurora",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    exclusive = "Ignore",
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            wallpaper.image,
            shader {
                width = "Fill",
                height = "Fill",
                source = mantle.config_dir .. "/aurora.frag",
                params = theme.tints,
                progress = 0,
                animate = {
                    progress = { duration = 16000, easing = "Linear", keyframes = { 0, 1 }, loops = "Infinite" },
                },
            },
        },
    },
}

return {
    aurora,
    wallpaper.picker,
    overview,
    osd,
    media.card,
    control.panel,
    notifications,
    updates.popover,
    polkit,
    launcher,
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        width = "Fill",
        height = 56,
        background = theme.fade("crust", "e6"),
        child = row {
            width = "Fill",
            height = "Fill",
            padding = { left = 12, right = 12 },
            children = {
                workspaces,
                taskbar.bar,
                rect { width = "Fill" },
                media.chip,
                tray,
                control.status,
                privacy,
                idle.indicator,
                updates.badge,
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%a %d %b   %H:%M", s and s.time)
                    end),
                    align_v = "Center",
                    font_size = 22,
                    foreground = theme.text,
                },
            },
        },
    },
}
