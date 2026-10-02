-- Demo: the director feeds `mock_notifications` in `mantle.notifications`' shape and types into
-- `reply_draft` with `mantle set`, so no real notification of yours reaches the take. A real shell
-- reads `mantle.notifications` and answers with `mantle.notifications:reply(id, text)`.
local theme = require("theme")
local layout = require("layout")
local feed = state("mock_notifications", { dnd = false, feed = {} })
local draft = state("reply_draft", "")
local sent = state("reply_sent", false)

local placed = mantle.screens:map(function(screens)
    return layout.dock(screens and screens[1], 660)
end)

-- The first strong character decides a line's direction; Arabic's lead bytes are 0xD8 to 0xDB.
local function rtl(text)
    local arabic = text:find("[\216-\219]")
    local latin = text:find("%a")
    return arabic ~= nil and (latin == nil or arabic < latin)
end

local function body_text(entry)
    local out = {}
    for _, span in ipairs(entry.body or {}) do
        if span.kind == "text" then out[#out + 1] = span.text end
    end
    return table.concat(out)
end

local function avatar(name)
    local sum = 0
    for k = 1, #name do
        sum = sum + name:byte(k)
    end
    return rect {
        width = 64,
        height = 64,
        radius = 32,
        background = theme.avatar(sum % 5 + 1),
        children = {
            text {
                content = name:match("^[%z\1-\127\194-\244][\128-\191]*") or "?",
                align_h = "Center",
                align_v = "Center",
                font_size = 28,
                font_weight = 700,
                foreground = theme.crust,
            },
        },
    }
end

local function caret()
    return rect {
        width = 2,
        height = 26,
        align_v = "Center",
        background = theme.cursor,
        opacity = 1,
        animate = {
            opacity = { duration = 1000, keyframes = { 1, { value = 1, duration = 500 }, 0, 1 }, loops = "Infinite" },
        },
    }
end

local function reply_field(entry)
    return row {
        width = "Fill",
        height = 56,
        spacing = 10,
        children = {
            rect {
                width = "Fill",
                height = "Fill",
                radius = 14,
                background = theme.crust,
                border_width = 1,
                border_color = computed({ draft, theme.overlay, theme.accent }, function(d, idle, typing)
                    return d == "" and idle or typing
                end),
                animate = { border_color = 200 },
                padding = { left = 18, right = 18 },
                children = draft:map(function(d)
                    local typed = d ~= ""
                    local label = text {
                        content = typed and d or (entry.reply_placeholder or "Reply"),
                        align_v = "Center",
                        font_size = 22,
                        foreground = typed and theme.text or theme.muted,
                    }
                    local line = rtl(typed and d or body_text(entry))
                        and { rect { width = "Fill" }, caret(), label }
                        or { label, caret(), rect { width = "Fill" } }
                    return { row { width = "Fill", height = "Fill", spacing = 2, children = line } }
                end),
            },
            rect {
                width = 96,
                height = "Fill",
                radius = 14,
                background = computed({ sent, theme.success, theme.accent }, function(s, done, ready)
                    return s and done or
                        ready
                end),
                opacity = draft:map(function(d) return d == "" and 0.4 or 1 end),
                animate = { background = 200, opacity = 200 },
                children = {
                    icon {
                        name = "mail-send-symbolic",
                        size = 26,
                        align_h = "Center",
                        align_v = "Center",
                        foreground = theme.crust,
                    },
                },
            },
        },
    }
end

local function action_button(action)
    return rect {
        padding = { left = 18, right = 18, top = 10, bottom = 10 },
        radius = 12,
        background = theme.surface,
        children = { text { content = action.label, font_size = 20, foreground = theme.text } },
    }
end

local function card(entry)
    local actions = {}
    for k, action in ipairs(entry.actions or {}) do
        actions[k] = action_button(action)
    end
    return column {
        id = "notification:" .. entry.id,
        width = placed:map(function(p) return p.width end),
        padding = 22,
        spacing = 16,
        radius = 22,
        background = theme.fade("base", "f5"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = sent:map(function(s) return s and 0 or 1 end),
        translate = { x = 0, y = 0 },
        animate = {
            opacity = { duration = 300, from = 0 },
            translate = { duration = 450, easing = "OutBack", from = { x = 60, y = 0 } },
        },
        children = {
            row {
                width = "Fill",
                spacing = 10,
                children = {
                    icon { name = entry.app_icon or "dialog-information", size = 26, align_v = "Center" },
                    text { content = entry.app_name, align_v = "Center", font_size = 18, foreground = theme.subtext },
                    rect { width = "Fill" },
                    text { content = "now", align_v = "Center", font_size = 18, foreground = theme.muted },
                },
            },
            row {
                width = "Fill",
                spacing = 18,
                children = {
                    avatar(entry.summary),
                    column {
                        width = "Fill",
                        spacing = 6,
                        align_v = "Center",
                        children = {
                            text {
                                content = entry.summary,
                                width = "Fill",
                                text_align = "Start",
                                font_size = 26,
                                font_weight = 700,
                                foreground = theme.text,
                            },
                            text {
                                content = body_text(entry),
                                width = "Fill",
                                wrap = "Word",
                                text_align = "Start",
                                font_size = 22,
                                foreground = theme.subtext1,
                            },
                        },
                    },
                },
            },
            row { spacing = 10, visible = #actions > 0, children = actions },
            entry.has_reply and reply_field(entry) or rect {},
        },
    }
end

return panel {
    id = "notifications",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = placed:map(function(p) return { top = p.top, left = p.left } end),
    visible = feed:map(function(f) return #f.feed > 0 end),
    child = feed:map(function(f) return f.feed[1] and card(f.feed[1]) or rect {} end),
}
