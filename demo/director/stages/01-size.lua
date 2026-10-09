fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}


return {
    panel {
        id = "bar",
        layer = "top",
        anchor = { top = true, left = true, right = true },
        exclusive_zone = true,
        width = "fill",
        height = 56,
        background = "#1e1e2e80",
        child = row {
            width = "fill",
            height = "fill",
            padding = { left = 12, right = 12 },
            align_h = "end",
            children = {
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%H:%M", s and s.time)
                    end),
                    align_v = "center",
                    font_size = 22,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
