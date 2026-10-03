fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive_zone = true,
        width = "Fill",
        height = 34,
        background = "#1e1e2e80",
        child = row {
            width = "Fill",
            height = "Fill",
            align_h = "End",
            padding = { left = 12, right = 12 },
            children = {
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%H:%M", s and s.time)
                    end),
                    align_v = "Center",
                    font_size = 13,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
