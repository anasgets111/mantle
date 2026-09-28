-- Demo: the director feeds `mock_polkit` in `mantle.polkit`'s shape and the typed length in
-- `polkit_typed` with `mantle set`, so no password of yours is typed. A real agent draws a secure
-- field that hands its text to `mantle.polkit:authenticate`.
local theme = require("theme")

local polkit = state("mock_polkit", { active = false, message = "", user = "" })
local typed = state("polkit_typed", 0)
local shown = polkit:map(function(p) return p.active end)
local mapped = computed({ shown, delay(shown, 300) }, function(now, was) return now or was end)

local WIDTH = 640

local function dots(count)
    local out = {}
    for k = 1, count do
        out[k] = rect {
            id = "dot:" .. k,
            width = 14,
            height = 14,
            radius = 7,
            align_v = "Center",
            background = theme.text,
            scale = 1,
            animate = { scale = { duration = 220, easing = "OutBack", from = 0 } },
        }
    end
    return out
end

return panel {
    id = "polkit",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = mantle.screens:map(function(screens)
        local screen = screens[1] or { width = 1920, height = 1080 }
        return { top = math.floor(screen.height * 0.22), left = math.floor((screen.width * 0.56 - WIDTH) / 2) }
    end),
    visible = mapped,
    keyboard_interactivity = "None",
    child = column {
        width = WIDTH,
        padding = 34,
        spacing = 20,
        radius = 30,
        background = theme.base,
        border_width = 1,
        border_color = theme.overlay,
        shadow_color = "#00000099",
        shadow_blur = 60,
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
                align_h = "Center",
                foreground = theme.accent,
            },
            text {
                content = "Authentication required",
                align_h = "Center",
                font_size = 30,
                font_weight = 800,
                foreground = theme.text,
            },
            text {
                content = polkit:map(function(p) return p.message end),
                width = "Fill",
                wrap = "Word",
                text_align = "Center",
                font_size = 20,
                foreground = theme.subtext,
            },
            rect {
                width = "Fill",
                height = 60,
                radius = 16,
                margin = { top = 8 },
                background = theme.crust,
                border_width = 2,
                border_color = theme.accent,
                padding = { left = 22, right = 22 },
                children = {
                    row {
                        height = "Fill",
                        spacing = 10,
                        children = typed:map(dots),
                    },
                },
            },
            text {
                content = polkit:map(function(p) return "Password for " .. p.user end),
                align_h = "Center",
                font_size = 16,
                foreground = theme.muted,
            },
        },
    },
}
