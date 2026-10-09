local theme = require("theme")
local layout = require("layout")
local rescue = mantle.rescue

-- Sized from the code font, so it reads at a glance on any stage the director leaves open.
local m = mantle.screens:map(function(screens) return layout.metrics(screens and screens[1]) end)

return panel {
    id = "rescue",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = m:map(function(s) return { top = s.height // 4, left = 48 } end),
    visible = rescue:map(function(r) return r ~= nil and r.is_rescue end),
    width = mantle.screens:map(function(screens)
        return layout.fit(screens and screens[1], 1400)
    end),
    child = column {
        width = "fill",
        padding = m:map(function(s) return s.font + 8 end),
        spacing = m:map(function(s) return s.font // 2 end),
        radius = 22,
        background = theme.danger,
        border_width = 3,
        border_color = theme.crust,
        scale = 1,
        animate = { scale = { duration = 420, easing = "out_back", from = 0.85 } },
        children = {
            text {
                content = "Reload failed. The last good shell is still running.",
                width = "fill",
                wrap = "word",
                font_size = m:map(function(s) return s.font * 5 // 2 end),
                font_weight = 800,
                foreground = theme.crust,
            },
            text {
                content = rescue:map(function(r) return r and r.error_log or "" end),
                width = "fill",
                wrap = "word",
                font = "CaskaydiaCove Nerd Font Mono",
                font_size = m:map(function(s) return s.font + 4 end),
                foreground = theme.crust,
            },
        },
    },
}
