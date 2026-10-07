-- Effects scenario: 36 cards over a busy backdrop, driven two ways. `driver.lua` flips two phases every
-- 400 ms, 200 ms apart (Lua re-resolve, 350 ms tweens, layout tweens), while the `LOOP` cards run
-- infinite keyframes in the engine with no Lua per frame.
local on = state("hs_fx", false)
local PHASE = { state("hs_fx_a", false), state("hs_fx_b", false) }
local STRIP = scroll("hs_fx_strip")
local DIR = mantle.config_dir .. "/shaders/"
local TEXT, DARK = "#CDD6F4", "#1E1E2E"
local CARDS = 36

local function swap(s, off, active)
    return s:map(function(v)
        if v then return active end
        return off
    end)
end
local function ramp(a, b) return { gradient = "linear", angle = 135, stops = { { 0, a }, { 1, b } } } end

local stripes = {}
for k = 1, 10 do
    local color = ({ "#F38BA8", "#89B4FA", "#A6E3A1", "#F9E2AF", "#CBA6F7" })[k % 5 + 1] .. "AA"
    stripes[#stripes + 1] = { (k - 1) / 10, color }
    stripes[#stripes + 1] = { k / 10, color }
end
local BUSY = {
    { gradient = "linear", angle = 45,                                              stops = stripes },
    { gradient = "conic",  stops = { { 0, DARK }, { 0.5, "#585B70" }, { 1, DARK } } },
}

-- A recipe tweens properties a family leaves unset; the first free one from `i % 5` wins.
local RECIPES = {
    function(s) return { opacity = swap(s, 1, 0.45), animate = { opacity = 350 } } end,
    function(s)
        return {
            scale = swap(s, 1, 0.88),
            rotate = swap(s, 0, 8),
            translate = swap(s, { x = 0, y = 0 }, { x = 0, y = -4 }),
            animate = { scale = 350, rotate = 350, translate = 350 },
        }
    end,
    function(s)
        local function lift(blur, y, color) return { { color = color, blur = blur, offset = { y = y } } } end
        return { shadows = swap(s, lift(4, 2, "#00000066"), lift(28, 14, "#000000B3")), animate = { shadows = 350 } }
    end,
    function(s) return { background = swap(s, "#89B4FA", "#F38BA8"), animate = { background = 350 } } end,
    function(s) return { width = swap(s, 100, 76), animate = { width = 350 } } end, -- re-lays out the surface
}

local function loop(at) return { effect = { duration = 1600, keyframes = { at(0), at(1), at(0) }, loops = "infinite" } } end

local function outline(tip)
    local c = {
        { op = "M",      points = { 0, 20 } },
        { op = "corner", points = { 0, 0 },            radius = 12 },
        { op = "corner", points = { "100%", 0 },       radius = 12 },
        { op = "corner", points = { "100%", "80%" },   radius = 12 },
        { op = "corner", points = { tip + 10, "80%" }, radius = 2 },
        { op = "corner", points = { tip, "110%" },     radius = 2 },
        { op = "corner", points = { tip - 10, "80%" }, radius = 2 },
        { op = "corner", points = { 0, "80%" },        radius = 12 },
        { op = "Z",      points = {} },
    }
    return { commands = c }
end

local WAVE = { { op = "M", points = { 0, 12 } } }
for k = 0, 7 do
    WAVE[#WAVE + 1] = { op = "Q", points = { k * 30 + 15, 12 + (k % 2 == 0 and -8 or 8), k * 30 + 30, 12 } }
end

local function glass(blur, extra)
    local backdrop = { blur = blur }
    for k, v in pairs(extra or {}) do backdrop[k] = v end
    return { radius = 14, background = "#FFFFFF1F", border_width = 1, border_color = "#FFFFFF33", effect = { backdrop = backdrop } }
end

local FAMILIES = {
    { "layered", function(s)
        return {
            radius = 12,
            background = DARK,
            animate = { shadows = 350 },
            shadows = swap(s,
                { { color = "#0000004D", blur = 2, offset = { y = 1 } }, { color = "#00000026", blur = 6, offset = { y = 2 }, spread = 2 } },
                { { color = "#00000099", blur = 10, offset = { y = 6 } }, { color = "#00000066", blur = 30, offset = { y = 16 }, spread = 4 } })
        }
    end },
    { "inset", function(s)
        return {
            radius = 12,
            background = DARK,
            shadow_mode = "box",
            animate = { shadows = 350 },
            shadows = swap(s,
                { { color = "#000000AA", blur = 10, offset = { y = 3 }, inset = true }, { color = "#0000004D", blur = 3, offset = { y = 1 } } },
                { { color = "#000000DD", blur = 18, offset = { y = 8 }, inset = true }, { color = "#00000099", blur = 12, offset = { y = 6 } } })
        }
    end },
    { "content shadow", function()
        return {
            radius = 12,
            border_width = 2,
            border_color = "#89B4FA",
            shadow_mode = "content",
            shadows = { { color = "#000000", blur = 4, offset = { x = 4, y = 5 } } }
        }
    end },
    { "glass", function(s)
        local props = glass(14)
        props.background, props.animate = swap(s, "#FFFFFF1F", "#FFFFFF59"), { background = 350 }
        return props
    end },
    { "glass filters", function() return glass(24, { saturate = 2, brightness = 1.1, contrast = 1.05 }) end },
    { "progressive", function()
        return {
            radius = 14,
            clip = "rounded",
            border_width = 1,
            border_color = "#FFFFFF33",
            children = { rect {
                width = "fill", height = 40, align_v = "end",
                effect = { backdrop = { blur = 10, mask = { gradient = "linear", angle = 0, stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFF00" } } } } },
            } }
        }
    end },
    { "blur", function(s)
        return { radius = 12, background = ramp("#F38BA8", "#89B4FA"), effect = swap(s, { blur = 0 }, { blur = 3 }), animate = { effect = 350 } }
    end },
    { "colour filter", function(s)
        return {
            radius = 12,
            background = ramp("#A6E3A1", "#CBA6F7"),
            animate = { effect = 350 },
            effect = swap(s, { saturate = 1 }, { saturate = 2.5, brightness = 1.3, contrast = 1.2 })
        }
    end },
    { "shader effect", function()
        local function at(p)
            return { shader = { source = DIR .. "outline.frag", params = { width = 3, tint = { 1, 0.8, 0.2, 1 } }, progress = p, padding = 4 } }
        end
        return { radius = 20, background = "#335577", effect = at(0), animate = loop(at) }
    end },
    { "backdrop shader", function()
        local function at(p)
            return {
                backdrop = { blur = 6 },
                shader = { source = DIR .. "lens.frag", input = "backdrop", images = { noise = DIR .. "noise.png" }, progress = p, padding = 8 }
            }
        end
        return { radius = 32, background = "#FFFFFF14", effect = at(0), animate = loop(at) }
    end },
    { "shader node", function()
        return {
            radius = 14,
            clip = "rounded",
            background = DARK,
            children = { shader {
                width = 100, height = 72, source = DIR .. "glow.frag", params = { tint = { 0.54, 0.71, 0.98 } },
                animate = { progress = { duration = 1600, keyframes = { 0, 1, 0 }, loops = "infinite" } },
            } }
        }
    end },
    { "conic border", function()
        return {
            radius = 14,
            background = DARK,
            border_width = 2,
            border_color = { gradient = "conic", stops = { { 0, "#CBA6F7" }, { 0.5, "#89B4FA" }, { 1, "#CBA6F7" } } }
        }
    end },
    { "linear border", function()
        return {
            radius = 14,
            border_width = 3,
            border_color = ramp("#F9E2AF", "#F38BA8"),
            background = { { gradient = "linear", stops = { { 0, "#FFFFFF33" }, { 1, "#FFFFFF00" } } }, "#313244" }
        }
    end },
    { "ring", function(s)
        return {
            radius = 12,
            background = "#313244",
            animate = { ring = 350 },
            ring = swap(s, { width = 0, color = "#89B4FA", offset = 2 }, { width = 3, color = "#89B4FA", offset = 3 })
        }
    end },
    { "mask", function()
        return {
            radius = 12,
            background = ramp("#F9E2AF", "#F38BA8"),
            mask = { gradient = "linear", angle = 90, stops = { { 0, "#000000" }, { 0.6, "#000000" }, { 1, "#00000000" } } }
        }
    end },
    { "node mask", function(_, i)
        return {
            background = "#3366FF",
            mask = { node = "shape" .. i },
            children = { rect { id = "shape" .. i, width = 60, height = "fill", radius = 24, background = "#FFFFFF" } }
        }
    end },
    { "rounded clip", function(s)
        return {
            radius = 28,
            clip = "rounded",
            background = DARK,
            children = { rect {
                width = 140, height = 40, background = ramp("#89B4FA", "#A6E3A1"),
                translate = swap(s, { x = -30, y = 0 }, { x = 30, y = 0 }), animate = { translate = 350 },
            } }
        }
    end },
    { "blend node",    function() return { radius = 12, background = ramp("#F9E2AF", "#89B4FA"), blend = "difference" } end },
    { "blend layers", function()
        return { radius = 12, background = { { fill = "#F38BA8", blend = "overlay" }, { fill = "#89B4FA", blend = "color_dodge" }, "#313244" } }
    end },
    { "blend shadow", function()
        return { radius = 12, background = "#FFFFFF40", blend = "plus_lighter", shadows = { { color = "#00000066", blur = 12, blend = "multiply" } } }
    end },
    { "opacity", function(s)
        return { radius = 12, background = ramp("#CBA6F7", "#89B4FA"), opacity = swap(s, 1, 0.3), animate = { opacity = 350 } }
    end },
    { "transform", function(s)
        return {
            radius = 12,
            background = DARK,
            border_width = 1,
            border_color = "#89B4FA",
            animate = { scale = 350, rotate = 350, translate = 350 },
            scale = swap(s, 1, 0.8),
            rotate = swap(s, -6, 6),
            translate = swap(s, { x = -4, y = 0 }, { x = 4, y = 0 })
        }
    end },
    { "smooth corners", function(s)
        return {
            background = ramp("#89B4FA", "#F38BA8"),
            border_width = 2,
            border_color = "#FFFFFF66",
            corner_smoothing = swap(s, 0, 0.8),
            animate = { radius = 350, corner_smoothing = 350 },
            radius = swap(s, { top_left = 4, top_right = 28, bottom_right = 4, bottom_left = 28 },
                { top_left = 28, top_right = 4, bottom_right = 28, bottom_left = 4 })
        }
    end },
    { "outline", function(s)
        return {
            background = DARK,
            border_width = 2,
            border_color = "#89B4FA",
            outline = swap(s, outline(30),
                outline(70)),
            animate = { outline = 350 }
        }
    end },
    { "spring loop", function()
        return {
            radius = 12,
            background = DARK,
            children = { rect {
                width = 28, height = 28, radius = 6, background = "#F9E2AF", align_h = "center", align_v = "center",
                animate = { rotate = { duration = 800, spring = { stiffness = 300, damping = 18 }, keyframes = { 0, 90, 180, 270, 360 }, loops = "infinite" } },
            } }
        }
    end },
    { "label width", function(s)
        return {
            radius = 12,
            background = DARK,
            children = { text {
                content = swap(s, "Hi", "A much longer label"), font_size = 11, foreground = TEXT, align_h = "center", align_v = "center",
                animate = { width = 350 },
            } }
        }
    end },
    { "path trim", function(s)
        return {
            radius = 12,
            background = DARK,
            children = { path {
                width = 100, height = 24, align_v = "center", stroke = "#89B4FA", stroke_width = 3, stroke_cap = "round",
                trim_axis = "x", trim_end = swap(s, 0.3, 1), commands = WAVE,
                animate = { trim_end = 350, shift = { duration = 1000, easing = "linear", keyframes = { { x = 0, y = 0 }, { x = -60, y = 0 } }, loops = "infinite" } },
            } }
        }
    end },
    { "hover lift", function(_, i)
        local lifted, at = hover("hs_fx_hover" .. i), pointer("hs_fx_pointer" .. i)
        return {
            radius = 12,
            background = "#313244",
            hover = lifted,
            pointer = at,
            shadows = lifted:map(function(h) return { { color = "#00000099", blur = h and 30 or 8, offset = { y = h and 14 or 4 } } } end),
            translate = lifted:map(function(h) return { x = 0, y = h and -4 or 0 } end),
            animate = { shadows = 200, translate = 200 },
            children = { rect { width = 8, height = 8, radius = 4, background = "#F38BA8",
                translate = at:map(function(p) return p and { x = p.x - 4, y = p.y - 4 } or { x = 0, y = 0 } end) } }
        }
    end },
}

local function card(i)
    local s = PHASE[i % 2 + 1]
    local family = FAMILIES[(i - 1) % #FAMILIES + 1]
    local props = family[2](s, i)
    for k = 0, #RECIPES - 1 do
        local recipe = RECIPES[(i + k) % #RECIPES + 1](s)
        local free = true
        for key in pairs(recipe) do free = free and (key == "animate" or props[key] == nil) end
        if free then
            props.animate = props.animate or {}
            for key, v in pairs(recipe) do
                if key == "animate" then
                    for name, entry in pairs(v) do props.animate[name] = entry end
                else
                    props[key] = v
                end
            end
            break
        end
    end
    for key, v in pairs(props.animate or {}) do
        if type(v) == "number" then props.animate[key] = { duration = v, delay = i % 4 * 40 } end
    end
    props.width, props.height, props.align_h, props.align_v = props.width or 100, 72, "center", "center"
    if not props.children then
        props.children = { text { content = family[1], font_size = 10, foreground = TEXT, align_h = "center", align_v = "center" } }
    end
    return rect { width = 112, height = 84, children = { rect(props) } }
end

local cells = {}
for i = 1, CARDS do cells[i] = card(i) end

local chips = {}
for i = 1, 12 do
    chips[i] = rect {
        width = 80, height = 28, radius = 8, background = "#31324499",
        children = { text { content = "chip " .. i, font_size = 11, foreground = TEXT, align_h = "center", align_v = "center" } },
    }
end

return {
    on = on,
    phase = PHASE,
    strip = STRIP,
    surface = function()
        return window {
            id = "hs_fx",
            title = "Heavy fx",
            app_id = "mantle.heavy.fx",
            min_size = { width = 704, height = 584 },
            max_size = { width = 704, height = 584 },
            visible = on,
            on_close = function() on:set(false) end,
            child = column {
                width = "fill",
                height = "fill",
                padding = 16,
                spacing = 8,
                background = BUSY,
                children = {
                    row { width = "fill", height = 28, spacing = 8, scroll = STRIP, animate = { scroll = 300 }, children = chips },
                    row { width = "fill", wrap = true, children = cells },
                },
            },
        }
    end,
}
