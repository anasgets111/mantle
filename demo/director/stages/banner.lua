local rescue = mantle.rescue

return panel {
    id = "rescue",
    layer = "Overlay",
    anchor = { bottom = true, left = true },
    margin = { bottom = 300, left = 48 },
    visible = rescue:map(function(r) return r ~= nil and r.is_rescue end),
    width = 900,
    background = "#f38ba8",
    radius = 14,
    child = column {
        width = "Fill",
        padding = 18,
        spacing = 6,
        children = {
            text {
                content = "Reload failed. The last good shell is still running.",
                font_size = 20,
                font_weight = 700,
                foreground = "#11111b",
            },
            text {
                content = rescue:map(function(r) return r and r.error_log or "" end),
                width = "Fill",
                wrap = "Word",
                font = "CaskaydiaCove Nerd Font Mono",
                font_size = 16,
                foreground = "#11111b",
            },
        },
    },
}
