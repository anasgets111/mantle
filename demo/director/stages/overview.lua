-- A Super+Tab overview: a still `capture` of the screen over the open windows. The capture is taken
-- as the sheet maps, so the sheet fades in from 0: its first frame is clear of the one it captures.
local theme = require("theme")
local taskbar = require("taskbar")
local target = require("targets")

local open = state("overview_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

-- Sized to the 56% of the screen the director's code pane leaves open.
local layout = mantle.screens:map(function(screens)
    local screen = screens[1] or { name = "", width = 1920, height = 1080 }
    local sheet = math.floor(screen.width * 0.56 * 0.86)
    local shot = sheet - 64
    return {
        output = screen.name,
        left = math.floor((screen.width * 0.56 - sheet) / 2),
        sheet = sheet,
        shot = shot,
        shot_height = math.floor(shot * screen.height / screen.width),
    }
end)

local function card(w)
    return button {
        geometry = target("card:" .. w.id),
        width = 200,
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
        children = {
            column {
                width = "Fill",
                spacing = 12,
                children = {
                    icon { name = taskbar.icon_of(w.app_id), size = 64, align_h = "Center" },
                    text {
                        content = w.title,
                        width = "Fill",
                        wrap = "Word",
                        text_align = "Center",
                        font_size = 17,
                        foreground = w.focused and theme.text or theme.subtext,
                    },
                },
            },
        },
    }
end

return panel {
    id = "overview",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = layout:map(function(l) return { top = 24, left = l.left } end),
    visible = mapped,
    child = column {
        width = layout:map(function(l) return l.sheet end),
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
                width = layout:map(function(l) return l.shot end),
                height = layout:map(function(l) return l.shot_height end),
                radius = 18,
                clip = "Rounded",
                border_width = 2,
                border_color = theme.accent,
                scale = open:map(function(on) return on and 1 or 1.08 end),
                animate = { scale = { duration = 450, easing = "OutCubic", from = 1.08 } },
                children = {
                    capture {
                        output = layout:map(function(l) return l.output end),
                        fit = "cover",
                        width = "Fill",
                        height = "Fill",
                    },
                },
            },
            list {
                direction = "Horizontal",
                spacing = 18,
                align_h = "Center",
                source = taskbar.windows:map(function(w) return w.windows end),
                key = function(w) return w.id end,
                itemfn = card,
            },
        },
    },
}
