fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

--@ 07-wallpaper
--@ 03-workspaces
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

--@ else

--@ end
--@ 03-workspaces
require("targets")
--@ end
--@ 07-wallpaper
local theme = require("theme")
local wallpaper = require("wallpaper")
--@ end
--@ 08-windows
local taskbar = require("taskbar")
local overview = require("overview")
--@ end
--@ 09-media
local media = require("media")
local tray = require("tray")
--@ end
--@ 10-control
local control = require("control")
local osd = require("osd")
--@ end
--@ 11-notifications
local notifications = require("notifications")
--@ end
--@ 12-indicators
local privacy = require("privacy")
local idle = require("idle")
--@ end
--@ 13-updates
local updates = require("updates")
local polkit = require("polkit")
--@ end
--@ 14-sysinfo
local sysinfo = require("sysinfo")
local banner = require("banner")
--@ end
--@ 15-lock
local lock = require("lock")
--@ end
--@ 03-workspaces

--@ end
--@ 16-agent
-- Focus mode: dims the desktop. An agent finds `focus` with `mantle call` and flips it.
local focus_on = state("focus_on", false)
action("focus", function()
    focus_on:set(not focus_on:get())
    log.info("focus", focus_on:get() and "on" or "off")
    return focus_on:get()
end)

--@ end
--@ 03-workspaces
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

--@ end
--@ 04-launcher
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
    --@ end
    --@ 05-restyle
    background = theme.fade("surface", "70"),
    radius = 24,
    behind_blur = true,
    --@ 04-launcher
    background = theme.fade("base", "f2"),
    radius = 16,
    --@ end
    --@ 04-launcher
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

--@ end
--@ 06-shader
-- Whole over the bottom third, clear 60% up: the frost fades out.
local FADE = {
    gradient = "linear",
    angle = 0,
    stops = { { 0, "#ffffffff" }, { 0.3, "#ffffffff" }, { 0.6, "#ffffff00" } },
}

--@ end
return {
    --@ 06-shader
    panel {
        id = "aurora",
        layer = "background",
        anchor = { top = true, bottom = true, left = true, right = true },
        width = "fill",
        height = "fill",
        exclusive_zone = "ignore",
        child = rect {
            width = "fill",
            height = "fill",
            children = {
                --@ end
                --@ 07-wallpaper
                wallpaper.image,
                --@ 06-shader
                -- The frost blurs only this surface, so the art it frosts is drawn here too.
                image {
                    source = mantle.config_dir .. "/wallpapers/mantle.png",
                    width = "fill",
                    height = "fill",
                    fit = "cover",
                },
                --@ end
                --@ 06-shader
                shader {
                    width = "fill",
                    height = "fill",
                    --@ end
                    --@ 07-wallpaper
                    params = theme.tints,
                    --@ 06-shader
                    params = { tint_a = { 0.54, 0.71, 0.98 }, tint_b = { 0.80, 0.65, 0.97 } },
                    --@ end
                    --@ 06-shader
                    progress = 0,
                    -- Two passes, then still: a desktop need not redraw forever, nor restart.
                    animate = state("aurora_settled", false):map(function(settled)
                        local run = { duration = 16000, easing = "linear", keyframes = { 0, 1 }, loops = 2 }
                        return { progress = not settled and run or nil }
                    end),
                    source = mantle.config_dir .. "/aurora.frag",
                },
                rect {
                    width = "fill",
                    height = "fill",
                    effect = {
                        backdrop = {
                            blur = 30,
                            mask = FADE,
                        },
                    },
                },
                --@ end
                --@ 16-agent
                rect {
                    width = "fill",
                    height = "fill",
                    background = theme.crust,
                    opacity = focus_on:map(function(on) return on and 0.7 or 0 end),
                    animate = { opacity = 400 },
                },
                --@ end
                --@ 06-shader
            },
        },
    },
    --@ end
    --@ 07-wallpaper
    wallpaper.picker,
    --@ end
    --@ 08-windows
    overview,
    --@ end
    --@ 09-media
    media.card,
    --@ end
    --@ 10-control
    control.panel,
    osd,
    --@ end
    --@ 11-notifications
    notifications,
    --@ end
    --@ 13-updates
    updates.popover,
    polkit,
    --@ end
    --@ 14-sysinfo
    banner,
    --@ end
    --@ 15-lock
    lock,
    --@ end
    --@ 04-launcher
    launcher,
    --@ end
    panel {
        id = "bar",
        layer = "top",
        anchor = { top = true, left = true, right = true },
        exclusive_zone = true,
        width = "fill",
        --@ 01-size
        height = 56,
        --@ else
        height = 34,
        --@ end
        --@ 03-workspaces
        background = theme.fade("crust", "e6"),
        --@ 02-color
        background = "#11111be6",
        --@ else
        background = "#1e1e2e80",
        --@ end
        child = row {
            width = "fill",
            height = "fill",
            padding = { left = 12, right = 12 },
            --@ 03-workspaces
            --@ else
            align_h = "end",
            --@ end
            children = {
                --@ 03-workspaces
                workspaces,
                --@ end
                --@ 08-windows
                taskbar.bar,
                --@ end
                --@ 03-workspaces
                rect { width = "fill" },
                --@ end
                --@ 09-media
                media.chip,
                tray,
                --@ end
                --@ 10-control
                control.status,
                --@ end
                --@ 12-indicators
                privacy,
                idle.indicator,
                --@ end
                --@ 13-updates
                updates.badge,
                --@ end
                --@ 14-sysinfo
                sysinfo,
                --@ end
                --@ 16-agent
                rect {
                    geometry = require("targets")("focus"),
                    visible = focus_on,
                    margin = { right = 14 },
                    height = 40,
                    align_v = "center",
                    padding = { left = 16, right = 16 },
                    radius = 20,
                    background = theme.accent,
                    on_click = function() focus_on:set(false) end,
                    children = {
                        text {
                            content = "Focus",
                            align_v = "center",
                            font_size = 18,
                            foreground = theme.crust,
                        },
                    },
                },
                --@ end
                text {
                    content = mantle.system:map(function(s)
                        --@ 02-color
                        return os.date("%a %d %b   %H:%M", s and s.time)
                        --@ else
                        return os.date("%H:%M", s and s.time)
                        --@ end
                    end),
                    align_v = "center",
                    --@ 01-size
                    font_size = 22,
                    --@ else
                    font_size = 13,
                    --@ end
                    --@ 03-workspaces
                    foreground = theme.text,
                    --@ else
                    foreground = "#cdd6f4ff",
                    --@ end
                },
            },
        },
    },
}
