# Notification popups

Notification cards stacked in the top-right corner, newest first. Each card shows the app, the
summary, the formatted body and the sender's action buttons; clicking it runs the default action,
the × dismisses it, and hovering the stack pauses every countdown.

<!-- shot: frames=0..210/30 -->
```lua,shot
local MAX_CARDS = 4
local stack_hover = hover("notification_stack")

-- Cards to show: not expired, not silenced by do-not-disturb, newest first.
local cards = mantle.notifications:map(function(notifications)
    local out = {}
    for _, item in ipairs(notifications and notifications.feed or {}) do
        local quiet = notifications.dnd and item.urgency ~= "critical"
        if not item.expired and not quiet and #out < MAX_CARDS then
            out[#out + 1] = item
        end
    end
    return out
end)

-- A text span is a text run as it is; links also get a colour. Image spans are skipped.
local function body_runs(spans)
    local runs = {}
    for _, span in ipairs(spans) do
        if span.kind == "text" then
            -- A copy: the span belongs to the pushed snapshot every other reader shares.
            local run = {}
            for field, value in pairs(span) do run[field] = value end
            if run.href then
                run.underline, run.color = true, "#89b4fa"
            end
            runs[#runs + 1] = run
        end
    end
    return runs
end

local function artwork(item)
    if item.image_path then
        return rect {
            width = 40,
            height = 40,
            radius = 8,
            clip = "Rounded",
            children = { image { source = item.image_path, fit = "cover", width = "Fill", height = "Fill" } },
        }
    end
    return icon { name = item.app_icon or "dialog-information-symbolic", size = 32 }
end

local function action_buttons(item)
    local buttons = {}
    for index, action in ipairs(item.actions) do
        buttons[index] = button {
            width = "Fill",
            padding = 6,
            radius = 6,
            background = "#313244",
            on_click = function() mantle.notifications:invoke("invoke_action", item.id, action.key) end,
            children = {
                text { content = action.label, align_h = "Center", elide = "End", foreground = "#cdd6f4" },
            },
        }
    end
    return row { width = "Fill", spacing = 6, visible = #buttons > 0, children = buttons }
end

local function card(item)
    local critical = item.urgency == "critical"
    local runs = body_runs(item.body)
    return button {
        width = "Fill",
        padding = 12,
        radius = 12,
        background = "#1e1e2ef2",
        border_width = 1,
        border_color = critical and "#f38ba8" or "#45475a",
        opacity = 1, -- `from` needs the property set
        translate = { x = 0 },
        animate = {
            opacity = { duration = 150, from = 0 },
            translate = { duration = 200, easing = "OutCubic", from = { x = 40 } },
            exit = { duration = 150, opacity = 0 },
        },
        on_click = function()
            if item.has_default_action then
                mantle.notifications:invoke("invoke_action", item.id, "default")
            else
                mantle.notifications:invoke("dismiss", item.id)
            end
        end,
        children = {
            row {
                width = "Fill",
                spacing = 10,
                children = {
                    artwork(item),
                    column {
                        width = "Fill",
                        spacing = 4,
                        children = {
                            row {
                                width = "Fill",
                                spacing = 6,
                                children = {
                                    text { content = item.app_name, width = "Fill", elide = "End", font_size = 11, foreground = "#a6adc8" },
                                    button {
                                        padding = { left = 4, right = 4 },
                                        radius = 4,
                                        on_click = function() mantle.notifications:invoke("dismiss", item.id) end,
                                        children = { text { content = "×", font_size = 14, foreground = "#a6adc8" } },
                                    },
                                },
                            },
                            text {
                                content = { { text = item.summary, bold = true } },
                                width = "Fill",
                                elide = "End",
                                font_size = 13,
                                foreground = "#cdd6f4",
                            },
                            text {
                                content = runs,
                                visible = #runs > 0,
                                width = "Fill",
                                wrap = "Word",
                                max_lines = 4,
                                elide = "End",
                                foreground = "#bac2de",
                                on_link = function(href) mantle.applications:invoke("open_url", href) end,
                            },
                            action_buttons(item),
                        },
                    },
                },
            },
        },
    }
end

return {
    panel {
        id = "notifications",
        layer = "Overlay",
        anchor = { top = true, right = true },
        margin = { top = 8, right = 8 },
        width = 380,
        visible = cards:map(function(shown) return #shown > 0 end),
        child = column {
            width = "Fill",
            hover = stack_hover,
            -- Pause every countdown while the pointer is over the stack.
            on_hover = function(inside) mantle.notifications:invoke("hold_expiry", inside and 300 or 0) end,
            children = {
                list {
                    width = "Fill",
                    spacing = 8,
                    source = cards,
                    key = function(item) return tostring(item.id) end,
                    itemfn = card,
                },
            },
        },
    },
}
```

## How it works

- `feed` is the newest 20, expired ones included; the map keeps the live ones and caps them ([notifications](../capabilities/notifications.md)).
- `dnd` only mutes sounds, so hiding popups during it is the config's filter; critical ones still show.
- `key` by `id` keeps each card's node when a newer one arrives above it, so only the new one animates in; a dismissed card fades out through `animate.exit` ([identity](../nodes/index.md#identity-and-reconciliation), [exit](../guide/animation.md#exit)).
- A body's text spans pass to `text` as runs unchanged; `on_link` hands a clicked `href` to `open_url` ([text runs](../nodes/text.md#runs), [applications](../capabilities/applications.md)).
- The × is a `button` inside the card's `button`: the innermost one with a handler takes the click ([pointer](../guide/input.md#pointer)).
- `on_hover` on the stack calls `hold_expiry`, so a card cannot expire while being read ([hover](../guide/input.md#hover)).
- The panel is anchored to two edges, so it measures its content and grows with the stack ([corner stack](../surfaces/panel.md#corner-stack)).

## Variations

| Change | Edit |
| :--- | :--- |
| Play sounds | Once at top level: `mantle.notifications:invoke("set_sound", "normal", "/usr/share/sounds/freedesktop/stereo/message.oga")` |
| Bottom-right corner | `anchor = { bottom = true, right = true }`, `margin = { bottom = 8, right = 8 }` |
| Hide everything under do-not-disturb | `local quiet = notifications.dnd` |
| Relative time | Add `text { content = mantle.system:map(function(system) return system and math.floor((system.time - item.timestamp) / 60) .. " min ago" or "" end) }` |
| Mute one app | `mantle.notifications:invoke("set_app_muted", "discord", true)` |
