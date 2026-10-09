-- Demo: the director feeds `mock_media` in `mantle.mpris`' shape with `mantle set`, so the take plays
-- nothing of yours. A real shell reads `mantle.mpris` and calls `mantle.mpris:next(id)` and friends.
local theme = require("theme")
local target = require("targets")
local layout = require("layout")

local placed = mantle.screens:map(function(screens)
    return layout.center(screens and screens[1], 760)
end)
local cover_px = placed:map(function(p) return math.floor(math.min(240, p.width * 240 / 760)) end)

local media = state("mock_media", { players = {} })
local open = state("media_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

local player = media:map(function(m) return m.players[1] end)
local function field(name, fallback)
    return player:map(function(p) return p and p[name] or fallback end)
end
local playing = player:map(function(p) return p ~= nil and p.play_state == "playing" end)

-- Pause and play are two 4-point subpaths each on a 24 px grid, so `animate.commands` morphs them.
-- The play halves overlap by 0.5 px so the seam does not show.
local BARS = { { 6, 5, 10, 5, 10, 19, 6, 19 }, { 14, 5, 18, 5, 18, 19, 14, 19 } }
local TRIANGLE = { { 6, 5, 13, 8.77, 13, 15.23, 6, 19 }, { 12.5, 8.5, 19, 12, 19, 12, 12.5, 15.5 } }

local function play_glyph(size, color)
    return path {
        width = size,
        height = size,
        align_h = "center",
        align_v = "center",
        fill = color,
        commands = playing:map(function(on)
            local commands = {}
            for _, quad in ipairs(on and BARS or TRIANGLE) do
                for k = 1, 4 do
                    local point = { quad[2 * k - 1] * size / 24, quad[2 * k] * size / 24 }
                    commands[#commands + 1] = { op = k == 1 and "M" or "L", points = point }
                end
                commands[#commands + 1] = { op = "Z", points = {} }
            end
            return commands
        end),
        animate = { commands = { duration = 220, easing = "out_cubic" } },
    }
end

local progress = player:map(function(p)
    if not (p and p.length and p.position) or p.length <= 0 then return 0 end
    return math.min(1, p.position / p.length)
end)

-- Half-waves of 30 px, so a 60 px `shift` loop repeats seamlessly; paused flattens the wave.
local function wave_commands(width, amplitude)
    local commands = { { op = "M", points = { 0, 12 } } }
    for i = 0, math.ceil(width / 30) + 2 do
        local crest = i % 2 == 0 and -2 * amplitude or 2 * amplitude
        commands[#commands + 1] = { op = "Q", points = { i * 30 + 15, 12 + crest, i * 30 + 30, 12 } }
    end
    return commands
end

-- Card width less padding 56, gap 30 and the cover.
local wave_px = computed({ placed, cover_px }, function(p, cover) return p.width - 86 - cover end)

local wave = path {
    width = wave_px,
    height = 24,
    margin = { top = 10 },
    stroke = theme.accent,
    stroke_width = 4,
    stroke_cap = "round",
    trim_axis = "x",
    --@ 09-motion
    trim_end = progress,
    --@ end
    commands = computed({ wave_px, playing }, function(width, on)
        return wave_commands(width, on and 5 or 0)
    end),
    animate = {
        commands = { duration = 300, easing = "out_cubic" },
        trim_end = { duration = 1000, easing = "linear" },
        shift = {
            duration = 1000,
            easing = "linear",
            keyframes = { { x = 0, y = 0 }, { x = -60, y = 0 } },
            loops = "infinite",
        },
    },
}

local function clock(us)
    local s = math.max(0, math.floor(us / 1000000))
    return string.format("%d:%02d", s // 60, s % 60)
end

local chip = rect {
    geometry = target("media"),
    visible = player:map(function(p) return p ~= nil end),
    margin = { right = 14 },
    height = 40,
    align_v = "center",
    padding = { left = 6, right = 16 },
    radius = 20,
    clip = "box",
    background = theme.surface,
    scale = 1,
    animate = {
        scale = { duration = 320, easing = "out_back", from = 0.5 },
        width = { duration = 260, easing = "out_cubic" },
        move = { duration = 180, easing = "out_cubic" },
    },
    children = {
        row {
            height = "fill",
            spacing = 10,
            children = {
                rect {
                    width = 36,
                    height = 36,
                    align_v = "center",
                    children = {
                        rect {
                            width = "fill",
                            height = "fill",
                            radius = 18,
                            background = computed({ theme.accent, theme.accent2 }, function(a, b)
                                return { gradient = "conic", stops = { { 0, a }, { 0.5, b }, { 1, a } } }
                            end),
                            animate = {
                                --@ 09-motion
                                rotate = {
                                    duration = 2400,
                                    easing = "linear",
                                    keyframes = { 0, 360 },
                                    loops = "infinite",
                                },
                                --@ end
                            },
                        },
                        image {
                            source = field("album_art_path", ""),
                            width = 30,
                            height = 30,
                            radius = 15,
                            align_h = "center",
                            align_v = "center",
                        },
                    },
                },
                play_glyph(20, theme.accent),
                text {
                    content = field("title", ""),
                    align_v = "center",
                    font_size = 18,
                    foreground = theme.text,
                },
            },
        },
    },
}

local function control(name, glyph, size, primary)
    return rect {
        geometry = target("media:" .. name),
        width = primary and 72 or 56,
        height = primary and 72 or 56,
        radius = 36,
        background = primary and theme.accent or theme.surface,
        animate = { background = 200 },
        children = {
            type(glyph) == "function" and glyph(size, primary and theme.crust or theme.text) or icon {
                name = glyph,
                size = size,
                align_h = "center",
                align_v = "center",
                foreground = primary and theme.crust or theme.text,
            },
        },
    }
end

local card = panel {
    id = "media",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = placed:map(function(p) return { top = p.top, left = p.left } end),
    visible = mapped,
    child = row {
        width = placed:map(function(p) return p.width end),
        padding = 28,
        spacing = 30,
        radius = 30,
        background = theme.fade("crust", "e6"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = open:map(function(on) return on and 1 or 0 end),
        translate = open:map(function(on) return { y = on and 0 or -20 } end),
        animate = {
            opacity = { duration = 200, from = 0 },
            translate = { spring = { stiffness = 260, damping = 17 }, from = { y = -20 } },
        },
        children = {
            rect {
                width = cover_px,
                height = cover_px,
                radius = 22,
                shadows = { { color = "#00000080", blur = 30, offset = { y = 10 } } },
                children = {
                    image {
                        id = "cover",
                        source = field("album_art_path", ""),
                        width = "fill",
                        height = "fill",
                        radius = 22,
                        transition = { duration = 500, easing = "in_out_sine" },
                    },
                },
            },
            column {
                width = "fill",
                spacing = 10,
                align_v = "center",
                children = {
                    text { content = field("identity", ""), font_size = 18, foreground = theme.muted },
                    text {
                        content = field("title", ""),
                        width = "fill",
                        elide = "end",
                        font_size = 36,
                        font_weight = 800,
                        foreground = theme.text,
                    },
                    text { content = field("artist", ""), font_size = 24, foreground = theme.subtext },
                    wave,
                    row {
                        width = "fill",
                        children = {
                            text {
                                content = player:map(function(p)
                                    return p and p.position and clock(p.position) or ""
                                end),
                                font_size = 16,
                                foreground = theme.muted,
                            },
                            rect { width = "fill" },
                            text {
                                content = player:map(function(p)
                                    return p and p.length and clock(p.length) or ""
                                end),
                                font_size = 16,
                                foreground = theme.muted,
                            },
                        },
                    },
                    row {
                        align_h = "center",
                        spacing = 18,
                        children = {
                            control("previous", "media-skip-backward-symbolic", 24),
                            control("play", play_glyph, 30, true),
                            control("next", "media-skip-forward-symbolic", 24),
                        },
                    },
                },
            },
        },
    },
}

return { chip = chip, card = card }
