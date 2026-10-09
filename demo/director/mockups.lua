-- Props: app windows the director draws in the open area, beside the code pane or across the
-- screen with it off, so the windows and privacy beats have something to react to. They are not
-- part of the demo shell.

local theme = require("theme")
local layout = require("layout")

local TITLE = 52

local app = state("mock_app", "")
local shown = state("mock_app_shown", false)

local size = layout.placed("mock")
local sidebar_width = size:map(function(s) return math.min(360, math.floor(s.width * 0.42)) end)

local function label(content, size_px, color, extra)
    local node = { content = content, font_size = size_px, foreground = color, align_v = "center" }
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
        align_h = "center",
        align_v = "center",
        children = {
            text {
                content = name:match("^[%z\1-\127\194-\244][\128-\191]*") or "?",
                align_h = "center",
                align_v = "center",
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
        layer = "top",
        anchor = { top = true, left = true },
        -- The window sits in a 64 px frame so its shadow is not cut at the surface edge.
        margin = size:map(function(s) return { top = s.top - 64, left = s.left - 64 } end),
        visible = app:map(function(a) return a == id end),
        child = column {
            width = size:map(function(s) return s.width + 128 end),
            height = size:map(function(s) return s.height + 128 end),
            padding = 64,
            children = { column {
                width = size:map(function(s) return s.width end),
                height = size:map(function(s) return s.height end),
                radius = 18,
                clip = "rounded",
                shadows = {
                    { color = "#00000055", blur = 48, offset = { y = 18 } },
                    { color = "#00000040", blur = 6,  offset = { y = 2 } },
                },
                background = theme.base,
                border_width = 1,
                border_color = theme.overlay,
                opacity = shown:map(function(on) return on and 1 or 0 end),
                scale = shown:map(function(on) return on and 1 or 0.97 end),
                animate = {
                    opacity = { duration = 280, from = 0 },
                    scale = { duration = 380, easing = "out_cubic", from = 0.94 },
                },
                children = {
                    row {
                        width = "fill",
                        height = TITLE,
                        padding = { left = 18, right = 18 },
                        spacing = 12,
                        background = theme.mantle,
                        children = {
                            icon { name = app_icon, size = 26, align_v = "center" },
                            label(title, 19, theme.subtext1),
                            rect { width = "fill" },
                            rect { width = 14, height = 14, radius = 7, align_v = "center", background = theme.overlay },
                            rect { width = 14, height = 14, radius = 7, align_v = "center", background = theme.overlay },
                            rect { width = 14, height = 14, radius = 7, align_v = "center", background = theme.danger },
                        },
                    },
                    body,
                },
            } },
        },
    }
end

-- Chat ----------------------------------------------------------------------------------------

-- The conversation the taskbar click opens.
local CHAT = {
    name = "Sarah",
    messages = { { mine = false, text = "Still on for tonight?" }, { mine = true, text = "Yes! See you at 8." } },
}

local CHATS = {
    { name = "Sarah", preview = "Still on for tonight?", color = theme.avatar(1) },
    { name = "أحمد", preview = "وصلت؟", color = theme.avatar(2) },
    { name = "Mantle devs", preview = "v0.9 is out", color = theme.avatar(3) },
    { name = "Family", preview = "Photos from Sunday", color = theme.avatar(4) },
}

local function sidebar()
    local rows = {}
    for k, entry in ipairs(CHATS) do
        rows[k] = row {
            width = "fill",
            padding = 12,
            spacing = 14,
            radius = 14,
            background = entry.name == CHAT.name and theme.surface or "#00000000",
            children = {
                initial_avatar(entry.name, entry.color, 52),
                column {
                    width = "fill",
                    spacing = 4,
                    align_v = "center",
                    children = {
                        label(entry.name, 21, theme.text, { width = "fill", text_align = "start" }),
                        label(entry.preview, 18, theme.subtle, { width = "fill", text_align = "start" }),
                    },
                },
            },
        }
    end
    return column {
        width = sidebar_width,
        height = "fill",
        padding = 14,
        spacing = 6,
        background = theme.mantle,
        children = rows,
    }
end

local function bubble(message)
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
            translate = { duration = 350, easing = "out_cubic", from = { x = 0, y = 16 } },
        },
        children = {
            label(message.text .. (mine and "   ✓✓" or ""), 22, mine and theme.crust or theme.text,
                { width = "fill", wrap = "word" }),
        },
    }
    return row {
        id = "bubble:" .. message.text,
        width = "fill",
        children = mine and { rect { width = "fill" }, node } or { node, rect { width = "fill" } },
    }
end

local chat_window = window("chat", "Telegram", "org.telegram.desktop", row {
    width = "fill",
    height = "fill",
    children = {
        sidebar(),
        column {
            width = "fill",
            height = "fill",
            children = {
                row {
                    width = "fill",
                    height = 76,
                    padding = { left = 24, right = 24 },
                    spacing = 14,
                    children = {
                        initial_avatar(CHAT.name, CHATS[1].color, 48),
                        column {
                            align_v = "center",
                            spacing = 2,
                            children = {
                                label(CHAT.name, 22, theme.text, { font_weight = 700 }),
                                label("online", 17, theme.success),
                            },
                        },
                    },
                },
                rect { width = "fill", height = 1, background = theme.surface },
                column {
                    width = "fill",
                    height = "fill",
                    padding = 28,
                    spacing = 14,
                    children = { rect { height = "fill" }, bubble(CHAT.messages[1]), bubble(CHAT.messages[2]) },
                },
                row {
                    width = "fill",
                    padding = 18,
                    children = {
                        rect {
                            width = "fill",
                            height = 54,
                            radius = 27,
                            background = theme.mantle,
                            padding = { left = 24, right = 24 },
                            children = {
                                label("Write a message", 20, theme.muted,
                                    { width = "fill", text_align = "start", height = "fill" }),
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
        width = "fill",
        height = "fill",
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
                loops = "infinite",
            },
        } or nil,
        children = {
            initial_avatar(person.name, person.color, 120),
            text {
                content = person.name,
                align_h = "start",
                align_v = "end",
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
            icon { name = icon_name, size = 28, align_h = "center", align_v = "center", foreground = theme.text },
        },
    }
end

local call_window = window("call", "Meet · Weekly sync", "camera-web-symbolic", column {
    width = "fill",
    height = "fill",
    padding = 18,
    spacing = 14,
    background = theme.crust,
    children = {
        row {
            width = "fill",
            height = "fill",
            spacing = 14,
            children = theme.success:map(function(c) return { tile(PEOPLE[1]), tile(PEOPLE[2], c) } end),
        },
        row {
            width = "fill",
            height = "fill",
            spacing = 14,
            children = { tile(PEOPLE[3]), tile(PEOPLE[4]) },
        },
        row {
            align_h = "center",
            spacing = 18,
            children = {
                control("audio-input-microphone-symbolic", theme.surface),
                control("camera-web-symbolic", theme.surface),
                control("screen-shared-symbolic", theme.surface),
                control("call-stop-symbolic", theme.danger),
            },
        },
    },
})

-- Browser -------------------------------------------------------------------------------------

local function browser_window(wallpaper)
    return window("browser", "Zen Browser", "zen-browser", column {
        width = "fill",
        height = "fill",
        children = {
            row {
                width = "fill",
                height = 58,
                padding = { left = 16, right = 16 },
                spacing = 12,
                background = theme.mantle,
                children = {
                    rect {
                        width = 360,
                        height = 40,
                        radius = 10,
                        align_v = "center",
                        background = theme.surface,
                        padding = { left = 16, right = 16 },
                        children = { label("Aurora timelapse · 4K", 18, theme.text, { height = "fill" }) },
                    },
                    rect {
                        width = "fill",
                        height = 40,
                        radius = 20,
                        align_v = "center",
                        background = theme.crust,
                        padding = { left = 20, right = 20 },
                        children = {
                            label("videos.example/watch?v=aurora", 18, theme.subtext, {
                                width = "fill",
                                height = "fill",
                                font = "CaskaydiaCove Nerd Font Mono",
                                elide = "end",
                            }),
                        },
                    },
                },
            },
            rect {
                width = "fill",
                height = "fill",
                background = "#000000",
                clip = "box",
                children = {
                    image { source = wallpaper, width = "fill", height = "fill", fit = "cover" },
                    rect {
                        width = "fill",
                        height = 8,
                        align_v = "end",
                        margin = { left = 24, right = 24, bottom = 24 },
                        radius = 4,
                        background = "#ffffff40",
                        children = {
                            rect {
                                height = "fill",
                                radius = 4,
                                background = theme.danger,
                                width = "100%",
                                animate = {
                                    width = { duration = 9000, easing = "linear", from = "20%" },
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
    open = open,
    panels = function(wallpaper) return { chat_window, call_window, browser_window(wallpaper) } end,
}
