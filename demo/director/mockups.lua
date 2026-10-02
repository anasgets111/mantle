-- Props: app windows the director draws on the open 56% of the screen, so the notification,
-- privacy and idle beats have something to react to. They are not part of the demo shell.

local theme = require("theme")
local layout = require("layout")

local TITLE = 52

local app = state("mock_app", "")
local shown = state("mock_app_shown", false)
local chat = state("mock_chat", { name = "", rtl = false, messages = {} })
local sharing = state("mock_sharing", false)
local playing = state("mock_playing", true)

local size = mantle.screens:map(function(screens)
    return layout.mock(screens and screens[1])
end)
local sidebar_width = size:map(function(s) return math.min(360, math.floor(s.width * 0.42)) end)

local function label(content, size_px, color, extra)
    local node = { content = content, font_size = size_px, foreground = color, align_v = "Center" }
    for k, v in pairs(extra or {}) do
        node[k] = v
    end
    return text(node)
end

local function initial_avatar(name, color, px)
    return rect {
        width = px,
        height = px,
        radius = px / 2,
        background = color,
        align_h = "Center",
        align_v = "Center",
        children = {
            text {
                content = name:match("^[%z\1-\127\194-\244][\128-\191]*") or "?",
                align_h = "Center",
                align_v = "Center",
                font_size = math.floor(px * 0.42),
                font_weight = 700,
                foreground = theme.crust,
            },
        },
    }
end

local function window(id, title, app_icon, body)
    return panel {
        id = "mock:" .. id,
        layer = "Top",
        anchor = { top = true, left = true },
        margin = size:map(function(s) return { top = s.top, left = s.left } end),
        visible = app:map(function(a) return a == id end),
        child = column {
            width = size:map(function(s) return s.width end),
            height = size:map(function(s) return s.height end),
            radius = 18,
            clip = "Box",
            background = theme.base,
            border_width = 1,
            border_color = theme.overlay,
            opacity = shown:map(function(on) return on and 1 or 0 end),
            scale = shown:map(function(on) return on and 1 or 0.97 end),
            animate = {
                opacity = { duration = 280, from = 0 },
                scale = { duration = 380, easing = "OutCubic", from = 0.94 },
            },
            children = {
                row {
                    width = "Fill",
                    height = TITLE,
                    padding = { left = 18, right = 18 },
                    spacing = 12,
                    background = theme.mantle,
                    children = {
                        icon { name = app_icon, size = 26, align_v = "Center" },
                        label(title, 19, theme.subtext1),
                        rect { width = "Fill" },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = theme.overlay },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = theme.overlay },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = theme.danger },
                    },
                },
                body,
            },
        },
    }
end

-- Chat ----------------------------------------------------------------------------------------

local CHATS = {
    { name = "Sarah", preview = "Still on for tonight?", color = theme.avatar(1) },
    { name = "أحمد", preview = "وصلت؟", color = theme.avatar(2) },
    { name = "Mantle devs", preview = "v0.9 is out", color = theme.avatar(3) },
    { name = "Family", preview = "Photos from Sunday", color = theme.avatar(4) },
}

local function sidebar()
    return column {
        width = sidebar_width,
        height = "Fill",
        padding = 14,
        spacing = 6,
        background = theme.mantle,
        children = chat:map(function(c)
            local rows = {}
            for k, entry in ipairs(CHATS) do
                rows[k] = row {
                    width = "Fill",
                    padding = 12,
                    spacing = 14,
                    radius = 14,
                    background = entry.name == c.name and theme.surface or "#00000000",
                    children = {
                        initial_avatar(entry.name, entry.color, 52),
                        column {
                            width = "Fill",
                            spacing = 4,
                            align_v = "Center",
                            children = {
                                label(entry.name, 21, theme.text, { width = "Fill", text_align = "Start" }),
                                label(entry.preview, 18, theme.subtle, { width = "Fill", text_align = "Start" }),
                            },
                        },
                    },
                }
            end
            return rows
        end),
    }
end

local function bubble(message, rtl)
    local mine = message.mine
    local node = rect {
        max_width = computed({ size, sidebar_width }, function(s, sidebar) return s.width - sidebar - 56 end),
        padding = { left = 20, right = 20, top = 12, bottom = 12 },
        radius = 20,
        background = mine and theme.accent or theme.surface,
        opacity = 1,
        translate = { x = 0, y = 0 },
        animate = {
            opacity = { duration = 250, from = 0 },
            translate = { duration = 350, easing = "OutCubic", from = { x = 0, y = 16 } },
        },
        children = {
            label(message.text .. (mine and "   ✓✓" or ""), 22, mine and theme.crust or theme.text,
                { width = "Fill", wrap = "Word" }),
        },
    }
    -- A right-to-left chat mirrors: your own messages sit on the left.
    local at_end = mine ~= rtl
    return row {
        id = "bubble:" .. message.text,
        width = "Fill",
        children = at_end and { rect { width = "Fill" }, node } or { node, rect { width = "Fill" } },
    }
end

local chat_window = window("chat", "Telegram", "org.telegram.desktop", row {
    width = "Fill",
    height = "Fill",
    children = {
        sidebar(),
        column {
            width = "Fill",
            height = "Fill",
            children = {
                row {
                    width = "Fill",
                    height = 76,
                    padding = { left = 24, right = 24 },
                    spacing = 14,
                    children = chat:map(function(c)
                        local color = CHATS[1].color
                        for _, entry in ipairs(CHATS) do
                            if entry.name == c.name then color = entry.color end
                        end
                        return {
                            initial_avatar(c.name, color, 48),
                            column {
                                align_v = "Center",
                                spacing = 2,
                                children = {
                                    label(c.name, 22, theme.text, { font_weight = 700 }),
                                    label(c.rtl and "متصل الآن" or "online", 17, theme.success),
                                },
                            },
                        }
                    end),
                },
                rect { width = "Fill", height = 1, background = theme.surface },
                column {
                    width = "Fill",
                    height = "Fill",
                    padding = 28,
                    spacing = 14,
                    children = chat:map(function(c)
                        local out = { rect { height = "Fill" } }
                        for _, message in ipairs(c.messages) do
                            out[#out + 1] = bubble(message, c.rtl)
                        end
                        return out
                    end),
                },
                row {
                    width = "Fill",
                    padding = 18,
                    children = {
                        rect {
                            width = "Fill",
                            height = 54,
                            radius = 27,
                            background = theme.mantle,
                            padding = { left = 24, right = 24 },
                            children = {
                                label(
                                    chat:map(function(c) return c.rtl and "اكتب رسالة" or "Write a message" end),
                                    20,
                                    theme.muted,
                                    { width = "Fill", text_align = "Start", height = "Fill" }
                                ),
                            },
                        },
                    },
                },
            },
        },
    },
})

-- Call ----------------------------------------------------------------------------------------

local PEOPLE = {
    { name = "You",   color = theme.avatar(5), tint = theme.avatar_dim(5) },
    { name = "Sarah", color = theme.avatar(1), tint = theme.avatar_dim(1) },
    { name = "Omar",  color = theme.avatar(2), tint = theme.avatar_dim(2) },
    { name = "Lina",  color = theme.avatar(4), tint = theme.avatar_dim(4) },
}

-- `speaking` is the colour its border pulses in: keyframes take literals, so a re-theme rebuilds it.
local function tile(person, speaking)
    return rect {
        width = "Fill",
        height = "Fill",
        radius = 16,
        padding = 18,
        background = person.tint,
        border_width = speaking and 3 or 0,
        border_color = speaking or "#00000000",
        opacity = 1,
        animate = speaking and {
            border_color = {
                duration = 1400,
                keyframes = { speaking, speaking .. "00", speaking },
                loops = "Infinite",
            },
        } or nil,
        children = {
            initial_avatar(person.name, person.color, 120),
            text {
                content = person.name,
                align_h = "Start",
                align_v = "End",
                font_size = 20,
                foreground = theme.text,
            },
        },
    }
end

local function control(icon_name, background)
    return rect {
        width = 64,
        height = 64,
        radius = 32,
        background = background,
        animate = { background = 200 },
        children = {
            icon { name = icon_name, size = 28, align_h = "Center", align_v = "Center", foreground = theme.text },
        },
    }
end

local call_window = window("call", "Meet · Weekly sync", "camera-web-symbolic", column {
    width = "Fill",
    height = "Fill",
    padding = 18,
    spacing = 14,
    background = theme.crust,
    children = {
        row {
            width = "Fill",
            height = "Fill",
            spacing = 14,
            children = theme.success:map(function(c) return { tile(PEOPLE[1]), tile(PEOPLE[2], c) } end),
        },
        row {
            width = "Fill",
            height = "Fill",
            spacing = 14,
            children = { tile(PEOPLE[3]), tile(PEOPLE[4]) },
        },
        row {
            align_h = "Center",
            spacing = 18,
            children = {
                control("audio-input-microphone-symbolic", theme.surface),
                control("camera-web-symbolic", theme.surface),
                control(
                    "screen-shared-symbolic",
                    computed({ sharing, theme.accent, theme.surface }, function(s, on, off) return s and on or off end)
                ),
                control("call-stop-symbolic", theme.danger),
            },
        },
    },
})

-- Browser -------------------------------------------------------------------------------------

local function browser_window(wallpaper)
    return window("browser", "Zen Browser", "zen-browser", column {
        width = "Fill",
        height = "Fill",
        children = {
            row {
                width = "Fill",
                height = 58,
                padding = { left = 16, right = 16 },
                spacing = 12,
                background = theme.mantle,
                children = {
                    rect {
                        width = 360,
                        height = 40,
                        radius = 10,
                        align_v = "Center",
                        background = theme.surface,
                        padding = { left = 16, right = 16 },
                        children = { label("Aurora timelapse · 4K", 18, theme.text, { height = "Fill" }) },
                    },
                    rect {
                        width = "Fill",
                        height = 40,
                        radius = 20,
                        align_v = "Center",
                        background = theme.crust,
                        padding = { left = 20, right = 20 },
                        children = {
                            label("videos.example/watch?v=aurora", 18, theme.subtext, {
                                width = "Fill",
                                height = "Fill",
                                font = "CaskaydiaCove Nerd Font Mono",
                                elide = "End",
                            }),
                        },
                    },
                },
            },
            rect {
                width = "Fill",
                height = "Fill",
                background = "#000000",
                clip = "Box",
                children = {
                    image { source = wallpaper, width = "Fill", height = "Fill", fit = "cover" },
                    rect {
                        width = 120,
                        height = 120,
                        radius = 60,
                        align_h = "Center",
                        align_v = "Center",
                        background = theme.fade("crust", "cc"),
                        visible = playing:map(function(p) return not p end),
                        children = {
                            icon {
                                name = "media-playback-start-symbolic",
                                size = 56,
                                align_h = "Center",
                                align_v = "Center",
                                foreground = theme.text,
                            },
                        },
                    },
                    rect {
                        width = "Fill",
                        height = 8,
                        align_v = "End",
                        margin = { left = 24, right = 24, bottom = 24 },
                        radius = 4,
                        background = "#ffffff40",
                        children = {
                            rect {
                                height = "Fill",
                                radius = 4,
                                background = theme.danger,
                                width = playing:map(function(p) return p and "100%" or "46%" end),
                                animate = {
                                    width = { duration = 9000, easing = "Linear", from = "20%" },
                                },
                            },
                        },
                    },
                },
            },
        },
    })
end

-- Opens `id` over whatever is up; "" closes it.
local function open(id)
    return function(next)
        local function swap()
            app:set(id)
            shown:set(id ~= "")
            next()
        end
        if app:get() == "" then return swap() end
        shown:set(false)
        timer(300, swap)
    end
end

return {
    app = app,
    chat = chat,
    sharing = sharing,
    playing = playing,
    open = open,
    panels = function(wallpaper) return { chat_window, call_window, browser_window(wallpaper) } end,
}
