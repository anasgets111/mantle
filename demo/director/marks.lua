-- Password dots and letter avatars, shared by the demo shell's stages and the director's mockups.
local theme = require("theme")

local M = {}

-- `count` dots of `px`, each popping in as it is typed.
function M.dots(count, px)
    local out = {}
    for k = 1, count do
        out[k] = rect {
            id = "dot:" .. k,
            width = px,
            height = px,
            radius = px / 2,
            align_v = "center",
            background = theme.text,
            scale = 1,
            animate = { scale = { duration = 220, easing = "out_back", from = 0 } },
        }
    end
    return out
end

-- A `px` disc of `color` holding the first character of `name`.
function M.avatar(name, color, px)
    return rect {
        width = px,
        height = px,
        radius = px / 2,
        background = color,
        align_h = "center",
        align_v = "center",
        children = {
            text {
                content = name:match("^[%z\1-\127\194-\244][\128-\191]*") or "?",
                align_h = "center",
                align_v = "center",
                font_size = math.floor(px * 0.44),
                font_weight = 700,
                foreground = theme.crust,
            },
        },
    }
end

return M
