local theme = require("theme")
local layout = require("layout")
local rescue = mantle.rescue

return panel {
    id = "rescue",
    layer = "overlay",
    anchor = { bottom = true, left = true },
    margin = mantle.screens:map(function(screens)
        local m = layout.metrics(screens and screens[1])
        return { bottom = m.height // 4, left = 48 }
    end),
    visible = rescue:map(function(r) return r ~= nil and r.is_rescue end),
    width = mantle.screens:map(function(screens)
        return layout.fit(screens and screens[1], 900)
    end),
    background = theme.danger,
    radius = 14,
    child = column {
        width = "fill",
        padding = 18,
        spacing = 6,
        children = {
            text {
                content = "Reload failed. The last good shell is still running.",
                font_size = 20,
                font_weight = 700,
                foreground = theme.crust,
            },
            text {
                content = rescue:map(function(r) return r and r.error_log or "" end),
                width = "fill",
                wrap = "word",
                font = "CaskaydiaCove Nerd Font Mono",
                font_size = 16,
                foreground = theme.crust,
            },
        },
    },
}
