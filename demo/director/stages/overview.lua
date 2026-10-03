-- A Super+Tab overview: a still `capture` of the screen over the open windows. The capture is taken
-- as the sheet maps, so the sheet fades in from 0: its first frame is clear of the one it captures.
local theme = require("theme")
local taskbar = require("taskbar")
local target = require("targets")
local layout = require("layout")

local open = state("overview_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

-- Sized to the stage the director's code pane leaves open.
local frame = mantle.screens:map(function(screens)
    return layout.overview(screens and screens[1])
end)

local function card(w)
    return column {
        geometry = target("card:" .. w.id),
        width = frame:map(function(b) return b.card end),
        padding = 18,
        radius = 20,
        background = w.focused and theme.surface or theme.fade("base", "cc"),
        border_width = 2,
        border_color = w.focused and theme.accent or "#00000000",
        scale = w.focused and 1.06 or 1,
        animate = {
            background = 200,
            border_color = 200,
            scale = { spring = { stiffness = 340, damping = 18 } },
        },
        on_click = function() mantle.windows:focus(w.id) end,
        spacing = 12,
        children = {
            icon { name = taskbar.icon_of(w.app_id), size = 64, align_h = "center" },
            text {
                content = w.title,
                width = "fill",
                wrap = "word",
                text_align = "center",
                font_size = 17,
                foreground = w.focused and theme.text or theme.subtext,
            },
        },
    }
end

return panel {
    id = "overview",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = frame:map(function(l) return { top = l.top, left = l.left } end),
    visible = mapped,
    child = column {
        width = frame:map(function(l) return l.sheet end),
        padding = 32,
        spacing = 28,
        radius = 28,
        background = theme.fade("crust", "e6"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = open:map(function(on) return on and 1 or 0 end),
        animate = { opacity = { duration = 300, from = 0 } },
        children = {
            rect {
                width = frame:map(function(l) return l.shot end),
                height = frame:map(function(l) return l.shot_height end),
                radius = 18,
                clip = "rounded",
                border_width = 2,
                border_color = theme.accent,
                scale = open:map(function(on) return on and 1 or 1.08 end),
                animate = { scale = { duration = 450, easing = "out_cubic", from = 1.08 } },
                children = {
                    capture {
                        output = frame:map(function(l) return l.output end),
                        fit = "cover",
                        width = "fill",
                        height = "fill",
                    },
                },
            },
            list {
                direction = "horizontal",
                spacing = 18,
                align_h = "center",
                source = taskbar.windows:map(function(w) return w.windows end),
                key = function(w) return w.id end,
                itemfn = card,
            },
        },
    },
}
