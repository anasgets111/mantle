-- A picker over the PNGs in `wallpapers/` that re-themes the shell from the chosen one.
-- `mantle call wallpaper <file>` picks one the way a click on its thumbnail does.
-- Tiles and the quantizer read the small copies in `wallpapers/thumbs/`, not the screen-sized files.
local theme = require("theme")
local target = require("targets")
local layout = require("layout")
local DIR = mantle.config_dir .. "/wallpapers"
local THUMBS = DIR .. "/thumbs/"
mantle.files:watch(DIR, { "png" })

local current = state("wallpaper", "")
local open = state("picker_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

local function choose(name)
    current:set(name)
    theme.choose(THUMBS .. name)
    return name
end
action("wallpaper", choose)

local image_node = image {
    id = "wallpaper",
    width = "fill",
    height = "fill",
    async = true,
    source = current:map(function(name) return name ~= "" and DIR .. "/" .. name or "" end),
    transition = { duration = 1400, easing = "in_out_sine", shader = mantle.config_dir .. "/chevron.frag" },
    opacity = current:map(function(name) return name ~= "" and 1 or 0 end),
    animate = { opacity = 600 },
}

local picker_box = mantle.screens:map(function(screens)
    return layout.picker(screens and screens[1])
end)

local function thumbnail(entry)
    local chosen = current:map(function(name) return name == entry.name end)
    return rect {
        geometry = target("thumb:" .. entry.name),
        padding = 4,
        radius = 18,
        border_width = 3,
        border_color = computed({ chosen, theme.accent }, function(on, color) return on and color or "#00000000" end),
        scale = chosen:map(function(on) return on and 1 or 0.92 end),
        animate = { border_color = 250, scale = { spring = { stiffness = 320, damping = 16 } } },
        on_click = function() choose(entry.name) end,
        children = {
            image {
                source = THUMBS .. entry.name,
                async = true,
                width = picker_box:map(function(p) return p.tile end),
                height = picker_box:map(function(p) return p.tile_h end),
                radius = 14,
            },
        },
    }
end

local picker = panel {
    id = "picker",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = picker_box:map(function(p) return { top = p.top, left = p.left } end),
    visible = mapped,
    child = row {
        padding = 16,
        radius = 26,
        background = theme.fade("crust", "e6"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = open:map(function(on) return on and 1 or 0 end),
        translate = open:map(function(on) return { y = on and 0 or -20 } end),
        animate = {
            opacity = { duration = 200, from = 0 },
            translate = { duration = 320, easing = "out_back", from = { y = -20 } },
        },
        children = {
            list {
                direction = "horizontal",
                spacing = 16,
                source = mantle.files:map(function(files)
                    local folder = files and files.folders[DIR]
                    return folder and folder.entries or {}
                end),
                key = function(entry) return entry.name end,
                itemfn = thumbnail,
            },
        },
    },
}

return { image = image_node, picker = picker }
