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
local play_icon = playing:map(function(on)
    return on and "media-playback-pause-symbolic" or "media-playback-start-symbolic"
end)

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
    background = theme.surface,
    scale = 1,
    animate = { scale = { duration = 320, easing = "out_back", from = 0.5 } },
    children = {
        row {
            height = "fill",
            spacing = 10,
            children = {
                image {
                    source = field("album_art_path", ""),
                    width = 30,
                    height = 30,
                    radius = 15,
                    align_v = "center",
                },
                icon { name = play_icon, size = 20, align_v = "center", foreground = theme.accent },
                text { content = field("title", ""), align_v = "center", font_size = 18, foreground = theme.text },
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
            icon {
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
            translate = { duration = 360, easing = "out_back", from = { y = -20 } },
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
                    rect {
                        width = "fill",
                        height = 6,
                        radius = 3,
                        margin = { top = 14 },
                        background = theme.surface,
                        children = {
                            rect {
                                height = "fill",
                                radius = 3,
                                background = theme.accent,
                                width = player:map(function(p)
                                    if not (p and p.length and p.position) or p.length <= 0 then return "0%" end
                                    return string.format("%.1f%%", math.min(1, p.position / p.length) * 100)
                                end),
                                animate = { width = { duration = 1000, easing = "linear" } },
                            },
                        },
                    },
                    row {
                        width = "fill",
                        children = {
                            text {
                                content = player:map(function(p) return p and p.position and clock(p.position) or "" end),
                                font_size = 16,
                                foreground = theme.muted,
                            },
                            rect { width = "fill" },
                            text {
                                content = player:map(function(p) return p and p.length and clock(p.length) or "" end),
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
                            control("play", play_icon, 30, true),
                            control("next", "media-skip-forward-symbolic", 24),
                        },
                    },
                },
            },
        },
    },
}

return { chip = chip, card = card }
