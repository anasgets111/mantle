-- Your shell. Everything on screen is declared here or in a file this requires.
--
-- Capabilities read `nil` until their first push, so the clock's map guards `s and s.time`;
-- `os.date` with a nil time is now. Docs: docs/lua-api.md.
fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
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
