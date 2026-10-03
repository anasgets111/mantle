-- Your shell. Everything on screen is declared here or in a file this requires.
--
-- Capabilities read `nil` until their first push, so the clock's map guards `s and s.time`;
-- `os.date` with a nil time is now. Docs: https://anasgets111.github.io/mantle/

-- Example families: a missing one is skipped, and omitting `fonts` keeps the default chain.
fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
}

return {
    panel {
        id = "bar",
        layer = "top",
        anchor = { top = true, left = true, right = true },
        exclusive_zone = true,
        width = "fill",
        height = 34,
        background = "#1e1e2e80",
        child = row {
            width = "fill",
            height = "fill",
            align_h = "end",
            padding = { left = 12, right = 12 },
            children = {
                text {
                    content = mantle.system:map(function(s)
                        return os.date("%H:%M", s and s.time)
                    end),
                    align_v = "center",
                    font_size = 13,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
