-- Your shell. Everything on screen is declared here or in a file this requires.
--
-- A capability's `:map(fn, before)` skips `fn` until the first push and holds `before` meanwhile.
-- Docs: https://anasgets111.github.io/mantle/

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
                    content = mantle.clock:map(function(s)
                        return os.date("%H:%M", s.time)
                    end, os.date("%H:%M")),
                    align_v = "center",
                    font_size = 13,
                    foreground = "#cdd6f4ff",
                },
            },
        },
    },
}
