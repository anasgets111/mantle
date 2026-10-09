-- Demo: fed `mock_polkit` and `polkit_typed`, so no password of yours is typed.
local theme = require("theme")
local layout = require("layout")

local placed = layout.placed("polkit")

local polkit = state("mock_polkit", { active = false, message = "", user = "" })
local typed = state("polkit_typed", 0)
local shown = polkit:map(function(p) return p.active end)
local mapped = computed({ shown, delay(shown, 300) }, function(now, was) return now or was end)

local function dots(count)
    local out = {}
    for k = 1, count do
        out[k] = rect {
            id = "dot:" .. k,
            width = 14,
            height = 14,
            radius = 7,
            align_v = "center",
            background = theme.text,
            scale = 1,
            animate = { scale = { duration = 220, easing = "out_back", from = 0 } },
        }
    end
    return out
end

return panel {
    id = "polkit",
    layer = "overlay",
    anchor = { top = true, left = true },
    -- The card sits in a 48 px frame so its shadow is not cut at the surface edge.
    margin = placed:map(function(p) return { top = p.top - 48, left = p.left - 48 } end),
    visible = mapped,
    keyboard_interactivity = "none",
    child = column {
        width = placed:map(function(p) return p.width + 96 end),
        padding = 48,
        children = {
            column {
                width = placed:map(function(p) return p.width end),
                padding = 34,
                spacing = 20,
                radius = 30,
                background = theme.base,
                border_width = 1,
                border_color = theme.overlay,
                shadows = { { color = "#00000099", blur = 60 } },
                opacity = shown:map(function(on) return on and 1 or 0 end),
                scale = shown:map(function(on) return on and 1 or 0.94 end),
                animate = {
                    opacity = { duration = 220, from = 0 },
                    scale = { spring = { stiffness = 300, damping = 20 } },
                },
                children = {
                    icon {
                        name = "system-lock-screen-symbolic",
                        size = 56,
                        align_h = "center",
                        foreground = theme.accent,
                    },
                    text {
                        content = "Authentication required",
                        align_h = "center",
                        font_size = 30,
                        font_weight = 800,
                        foreground = theme.text,
                    },
                    text {
                        content = polkit:map(function(p) return p.message end),
                        width = "fill",
                        wrap = "word",
                        text_align = "center",
                        font_size = 20,
                        foreground = theme.subtext,
                    },
                    rect {
                        width = "fill",
                        height = 60,
                        radius = 16,
                        margin = { top = 8 },
                        background = theme.crust,
                        border_width = 2,
                        border_color = theme.accent,
                        padding = { left = 22, right = 22 },
                        children = {
                            row {
                                height = "fill",
                                spacing = 10,
                                children = typed:map(dots),
                            },
                        },
                    },
                    text {
                        content = polkit:map(function(p) return "Password for " .. p.user end),
                        align_h = "center",
                        font_size = 16,
                        foreground = theme.muted,
                    },
                },
            },
        },
    },
}
