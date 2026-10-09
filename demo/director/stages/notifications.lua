-- Demo: fed `mock_notifications` in `mantle.notifications`' shape; replies typed via `mantle call reply`.
local theme = require("theme")
local layout = require("layout")
local target = require("targets")
local feed = state("mock_notifications", { dnd = false, feed = {} })
local draft = state("reply_draft", "")
local sent = state("reply_sent", false)
local reply_focus = focus_target("reply")
-- The swipe: `{ id, x }` offsets that card by x px; past 40% of its width it is dismissed.
local drag = state("notif_drag", { id = "", x = 0 })

-- `set_text` does not call `on_change`, so the send button's dim follows this write.
action("reply", function(text)
    reply_focus:set_text(text or "")
    draft:set(text or "")
end)

local placed = layout.placed("notifications")

local function body_text(entry)
    local out = {}
    for _, span in ipairs(entry.body or {}) do
        if span.kind == "text" then out[#out + 1] = span.text end
    end
    return table.concat(out)
end

local function runs(entry)
    local out = {}
    for _, span in ipairs(entry.body or {}) do
        if span.kind == "text" then
            local link = span.href ~= nil
            out[#out + 1] = {
                text = span.text,
                bold = span.bold,
                underline = span.underline or link,
                color = link and theme.accent or nil,
                href = span.href,
            }
        end
    end
    return out
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
                align_h = "center",
                align_v = "center",
                font_size = 28,
                font_weight = 700,
                foreground = theme.crust,
            },
        },
    }
end

local function reply_field(entry)
    return row {
        width = "fill",
        height = 56,
        spacing = 10,
        children = {
            rect {
                width = "fill",
                height = "fill",
                radius = 14,
                background = theme.crust,
                border_width = 1,
                border_color = computed({ draft, theme.overlay, theme.accent }, function(d, idle, typing)
                    return d == "" and idle or typing
                end),
                animate = { border_color = 200 },
                padding = { left = 18, right = 18 },
                children = {
                    textfield {
                        focus_target = reply_focus,
                        width = "fill",
                        height = "fill",
                        font_size = 22,
                        foreground = theme.text,
                        placeholder = entry.reply_placeholder or "Reply",
                        placeholder_color = theme.muted,
                        caret = { color = theme.cursor },
                        autofocus = true,
                        -- A field without `on_change` or `on_submit` takes no keys and no `set_text`.
                        on_change = function(text) draft:set(text) end,
                    },
                },
            },
            rect {
                width = 96,
                height = "fill",
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
                        align_h = "center",
                        align_v = "center",
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

local function threshold(p) return p.width * 0.4 end

local function card(entry)
    local grab = 0
    local actions = {}
    for k, action in ipairs(entry.actions or {}) do
        actions[k] = action_button(action)
    end
    return column {
        id = "notification:" .. entry.id,
        geometry = target("notification"),
        width = placed:map(function(p) return p.width end),
        padding = 22,
        spacing = 16,
        radius = 22,
        background = theme.fade("base", "f5"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = sent:map(function(s) return s and 0 or 1 end),
        translate = drag:map(function(d) return { x = d.id == entry.id and d.x or 0, y = 0 } end),
        on_drag = function(_, pointer, phase)
            local d = drag:get()
            local x = d.id == entry.id and d.x or 0
            if phase == "start" then
                grab = pointer.x
            elseif phase == "move" then
                drag:set({ id = entry.id, x = math.max(0, x + pointer.x - grab) })
            elseif x < threshold(placed:get()) then
                drag:set({ id = entry.id, x = 0 })
            end
        end,
        animate = {
            opacity = { duration = 300, from = 0 },
            translate = { spring = { stiffness = 260, damping = 17 }, from = { x = 60, y = 0 } },
            exit = { duration = 260, easing = "in_cubic", opacity = 0, translate = { x = 420, y = 0 } },
        },
        children = {
            row {
                width = "fill",
                spacing = 10,
                children = {
                    icon { name = entry.app_icon or "dialog-information", size = 26, align_v = "center" },
                    text {
                        content = entry.app_name,
                        align_v = "center",
                        font_size = 18,
                        foreground = theme.subtext,
                    },
                    rect { width = "fill" },
                    text { content = "now", align_v = "center", font_size = 18, foreground = theme.muted },
                },
            },
            row {
                width = "fill",
                spacing = 18,
                children = {
                    avatar(entry.summary),
                    column {
                        width = "fill",
                        spacing = 6,
                        align_v = "center",
                        children = {
                            text {
                                content = entry.summary,
                                width = "fill",
                                text_align = "start",
                                font_size = 26,
                                font_weight = 700,
                                foreground = theme.text,
                            },
                            text {
                                --@ 11-links
                                content = runs(entry),
                                --@ else
                                content = body_text(entry),
                                --@ end
                                width = "fill",
                                wrap = "word",
                                text_align = "start",
                                font_size = 22,
                                foreground = theme.subtext1,
                                --@ 11-links
                                on_link = function(href)
                                    if href:match("^https://") then process.detach("xdg-open", { href }) end
                                end,
                                --@ end
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
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = placed:map(function(p) return { top = p.top, left = p.left } end),
    visible = feed:map(function(f) return #f.feed > 0 end),
    keyboard_interactivity = computed({ feed, drag, placed }, function(f, d, p)
        local top = f.feed[1]
        local gone = top and d.id == top.id and d.x >= threshold(p)
        return top and top.has_reply and not gone and "exclusive" or "none"
    end),
    -- To the screen's edge, so a swiped card leaves through it.
    width = placed:map(function(p) return p.width + p.right end),
    child = column {
        width = "fill",
        children = computed({ feed, drag, placed }, function(f, d, p)
            local top = f.feed[1]
            if not top or (d.id == top.id and d.x >= threshold(p)) then return {} end
            return { card(top) }
        end),
    },
}
