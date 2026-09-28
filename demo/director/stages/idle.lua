-- Demo: the director feeds `mock_idle` in `mantle.idle`'s shape with `mantle set`, so the take
-- needs no real inhibitor. A real shell reads `mantle.idle`.
local theme = require("theme")
local idle = state("mock_idle", { inhibited = false, inhibitors = {} })

local indicator = rect {
    visible = idle:map(function(i) return i.inhibited end),
    margin = { right = 14 },
    height = 40,
    align_v = "Center",
    padding = { left = 16, right = 18 },
    radius = 20,
    background = theme.caution,
    scale = 1,
    animate = { scale = { duration = 320, easing = "OutBack", from = 0.5 } },
    children = {
        row {
            height = "Fill",
            spacing = 10,
            children = {
                icon { name = "view-reveal-symbolic", size = 24, align_v = "Center", foreground = theme.crust },
                text {
                    content = idle:map(function(i)
                        local holder = i.inhibitors[1]
                        if not holder then return "Kept awake" end
                        return holder.why ~= "" and (holder.who .. " · " .. holder.why) or holder.who
                    end),
                    align_v = "Center",
                    font_size = 20,
                    font_weight = 700,
                    foreground = theme.crust,
                },
            },
        },
    },
}

return { indicator = indicator }
