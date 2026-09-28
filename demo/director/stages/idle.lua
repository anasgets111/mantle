-- Demo: the director feeds `mock_idle` in `mantle.idle`'s shape and flips `away` with `mantle set`,
-- so the take needs no real inhibitor or idle wait. A real shell reads `mantle.idle` and sets
-- `away` from `mantle.idle:register_threshold(seconds, on_idle, on_resume)`.
local idle = state("mock_idle", { inhibited = false, inhibitors = {} })
-- "on", then "leaving" for the fade out, then "off".
local away = state("away", "off")

local indicator = rect {
    visible = idle:map(function(i) return i.inhibited end),
    margin = { right = 14 },
    height = 40,
    align_v = "Center",
    padding = { left = 16, right = 18 },
    radius = 20,
    background = "#f9e2af",
    scale = 1,
    animate = { scale = { duration = 320, easing = "OutBack", from = 0.5 } },
    children = {
        row {
            height = "Fill",
            spacing = 10,
            children = {
                icon { name = "view-reveal-symbolic", size = 24, align_v = "Center", foreground = "#11111b" },
                text {
                    content = idle:map(function(i)
                        local holder = i.inhibitors[1]
                        if not holder then return "Kept awake" end
                        return holder.why ~= "" and (holder.who .. " · " .. holder.why) or holder.who
                    end),
                    align_v = "Center",
                    font_size = 20,
                    font_weight = 700,
                    foreground = "#11111b",
                },
            },
        },
    },
}

local screen = panel {
    id = "away",
    layer = "Overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    exclusive = "Ignore",
    visible = away:map(function(a) return a ~= "off" end),
    child = rect {
        width = "Fill",
        height = "Fill",
        background = "#0b0b14f0",
        opacity = away:map(function(a) return a == "on" and 1 or 0 end),
        animate = { opacity = { duration = 500, easing = "OutCubic", from = 0 } },
        children = {
            column {
                align_h = "Center",
                align_v = "Center",
                spacing = 18,
                children = {
                    text {
                        content = mantle.system:map(function(s) return os.date("%H:%M", s and s.time) end),
                        align_h = "Center",
                        font_size = 220,
                        font_weight = 200,
                        foreground = "#cdd6f4",
                    },
                    text {
                        content = mantle.system:map(function(s) return os.date("%A, %d %B", s and s.time) end),
                        align_h = "Center",
                        font_size = 40,
                        foreground = "#a6adc8",
                    },
                    text {
                        content = "Away. Move the mouse to come back.",
                        align_h = "Center",
                        margin = { top = 40 },
                        font_size = 26,
                        foreground = "#6c7086",
                    },
                },
            },
        },
    },
}

return { indicator = indicator, away = screen }
