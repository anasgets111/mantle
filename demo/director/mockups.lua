-- Props: app windows the director draws on the open 56% of the screen, so the notification,
-- privacy and idle beats have something to react to. They are not part of the demo shell.

local BAR = 56
local TITLE = 52

local app = state("mock_app", "")
local shown = state("mock_app_shown", false)
local chat = state("mock_chat", { name = "", rtl = false, messages = {} })
local sharing = state("mock_sharing", false)
local playing = state("mock_playing", true)

local size = mantle.screens:map(function(screens)
    local screen = screens[1] or { width = 1920, height = 1080 }
    local open = math.floor(screen.width * 0.56)
    local width = math.min(1400, open - 200)
    local height = math.min(840, screen.height - BAR - 460)
    return { width = width, height = height, left = math.floor((open - width) / 2), top = 70 }
end)

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
        align_v = "Center",
        children = {
            text {
                content = name:match("^[%z\1-\127\194-\244][\128-\191]*") or "?",
                align_h = "Center",
                align_v = "Center",
                font_size = math.floor(px * 0.42),
                font_weight = 700,
                foreground = "#11111b",
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
            background = "#1e1e2e",
            border_width = 1,
            border_color = "#45475a",
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
                    background = "#181825",
                    children = {
                        icon { name = app_icon, size = 26, align_v = "Center" },
                        label(title, 19, "#bac2de"),
                        rect { width = "Fill" },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = "#45475a" },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = "#45475a" },
                        rect { width = 14, height = 14, radius = 7, align_v = "Center", background = "#f38ba8" },
                    },
                },
                body,
            },
        },
    }
end

-- Chat ----------------------------------------------------------------------------------------

local CHATS = {
    { name = "Sarah", preview = "Still on for tonight?", color = "#f5c2e7" },
    { name = "أحمد", preview = "وصلت؟", color = "#94e2d5" },
    { name = "Mantle devs", preview = "v0.9 is out", color = "#89b4fa" },
    { name = "Family", preview = "Photos from Sunday", color = "#fab387" },
}

local function sidebar()
    return column {
        width = 360,
        height = "Fill",
        padding = 14,
        spacing = 6,
        background = "#181825",
        children = chat:map(function(c)
            local rows = {}
            for k, entry in ipairs(CHATS) do
                rows[k] = row {
                    width = "Fill",
                    padding = 12,
                    spacing = 14,
                    radius = 14,
                    background = entry.name == c.name and "#313244" or "#00000000",
                    children = {
                        initial_avatar(entry.name, entry.color, 52),
                        column {
                            width = "Fill",
                            spacing = 4,
                            align_v = "Center",
                            children = {
                                label(entry.name, 21, "#cdd6f4", { width = "Fill", text_align = "Start" }),
                                label(entry.preview, 18, "#7f849c", { width = "Fill", text_align = "Start" }),
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
        padding = { left = 20, right = 20, top = 12, bottom = 12 },
        radius = 20,
        background = mine and "#89b4fa" or "#313244",
        opacity = 1,
        translate = { x = 0, y = 0 },
        animate = {
            opacity = { duration = 250, from = 0 },
            translate = { duration = 350, easing = "OutCubic", from = { x = 0, y = 16 } },
        },
        children = {
            label(message.text .. (mine and "   ✓✓" or ""), 22, mine and "#11111b" or "#cdd6f4"),
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
                        local color = "#f5c2e7"
                        for _, entry in ipairs(CHATS) do
                            if entry.name == c.name then color = entry.color end
                        end
                        return {
                            initial_avatar(c.name, color, 48),
                            column {
                                align_v = "Center",
                                spacing = 2,
                                children = {
                                    label(c.name, 22, "#cdd6f4", { font_weight = 700 }),
                                    label(c.rtl and "متصل الآن" or "online", 17, "#a6e3a1"),
                                },
                            },
                        }
                    end),
                },
                rect { width = "Fill", height = 1, background = "#313244" },
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
                            background = "#181825",
                            padding = { left = 24, right = 24 },
                            children = {
                                label(
                                    chat:map(function(c) return c.rtl and "اكتب رسالة" or "Write a message" end),
                                    20,
                                    "#6c7086",
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
    { name = "You",   color = "#cba6f7", tint = "#2a2340" },
    { name = "Sarah", color = "#f5c2e7", tint = "#3a2433" },
    { name = "Omar",  color = "#94e2d5", tint = "#1f3533" },
    { name = "Lina",  color = "#fab387", tint = "#3a2c22" },
}

local function tile(person, speaking)
    return rect {
        width = "Fill",
        height = "Fill",
        radius = 16,
        background = person.tint,
        border_width = speaking and 3 or 0,
        border_color = "#a6e3a1",
        opacity = 1,
        animate = speaking and {
            border_color = {
                duration = 1400,
                keyframes = { "#a6e3a1", "#a6e3a100", "#a6e3a1" },
                loops = "Infinite",
            },
        } or nil,
        children = {
            initial_avatar(person.name, person.color, 120),
            text {
                content = person.name,
                align_h = "Start",
                align_v = "End",
                margin = { left = 18, bottom = 14 },
                font_size = 20,
                foreground = "#cdd6f4",
            },
        },
    }
end

local function control(icon_name, background, active)
    return rect {
        width = 64,
        height = 64,
        radius = 32,
        background = active or background,
        animate = { background = 200 },
        children = {
            icon { name = icon_name, size = 28, align_h = "Center", align_v = "Center", foreground = "#cdd6f4" },
        },
    }
end

local call_window = window("call", "Meet · Weekly sync", "camera-web-symbolic", column {
    width = "Fill",
    height = "Fill",
    padding = 18,
    spacing = 14,
    background = "#11111b",
    children = {
        row {
            width = "Fill",
            height = "Fill",
            spacing = 14,
            children = { tile(PEOPLE[1]), tile(PEOPLE[2], true) },
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
                control("audio-input-microphone-symbolic", "#313244"),
                control("camera-web-symbolic", "#313244"),
                control("screen-shared-symbolic", "#313244", sharing:map(function(s) return s and "#89b4fa" or "#313244" end)),
                control("call-stop-symbolic", "#e64553"),
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
                background = "#181825",
                children = {
                    rect {
                        width = 360,
                        height = 40,
                        radius = 10,
                        align_v = "Center",
                        background = "#313244",
                        padding = { left = 16, right = 16 },
                        children = { label("Aurora timelapse · 4K", 18, "#cdd6f4", { height = "Fill" }) },
                    },
                    rect {
                        width = "Fill",
                        height = 40,
                        radius = 20,
                        align_v = "Center",
                        background = "#11111b",
                        padding = { left = 20, right = 20 },
                        children = {
                            label("videos.example/watch?v=aurora", 18, "#a6adc8", {
                                height = "Fill",
                                font = "CaskaydiaCove Nerd Font Mono",
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
                        background = "#11111bcc",
                        visible = playing:map(function(p) return not p end),
                        children = {
                            icon {
                                name = "media-playback-start-symbolic",
                                size = 56,
                                align_h = "Center",
                                align_v = "Center",
                                foreground = "#cdd6f4",
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
                                background = "#f38ba8",
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
