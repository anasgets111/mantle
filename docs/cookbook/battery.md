# Battery indicator

A bar pill with a battery icon and percentage that turns red when low, a tooltip with the charge
state and time left, and a desktop notification when the charge drops past 15%. On a desktop with
no battery the pill hides itself.

```lua,shot
local LOW = 15
local pill_hover = hover("battery")

local STATES = {
    Charging = "Charging",
    Discharging = "On battery",
    FullyCharged = "Fully charged",
    PendingCharge = "Plugged in, not charging",
    Empty = "Empty",
}

local function duration(seconds)
    return string.format("%d h %02d min", seconds // 3600, seconds % 3600 // 60)
end

-- Adwaita-style names: battery-level-0 to battery-level-100 in steps of 10.
local glyph = mantle.battery:map(function(battery)
    if battery == nil or not battery.present then
        return "battery-missing-symbolic"
    end
    local step = math.floor(battery.percent / 10 + 0.5) * 10
    if battery.state == "FullyCharged" or (step == 100 and battery.state == "Charging") then
        return "battery-level-100-charged-symbolic"
    end
    return string.format("battery-level-%d%s-symbolic", step, battery.state == "Charging" and "-charging" or "")
end)

local colour = mantle.battery:map(function(battery)
    local low = battery and battery.present and battery.percent <= LOW and battery.state == "Discharging"
    return low and "#f38ba8" or "#cdd6f4"
end)

local details = mantle.battery:map(function(battery)
    if battery == nil or not battery.present then
        return "No battery"
    end
    local line = string.format("%d%% · %s", battery.percent, STATES[battery.state] or battery.state)
    if battery.state == "Discharging" and battery.time_to_empty then
        line = line .. "\n" .. duration(battery.time_to_empty) .. " left"
    elseif battery.state == "Charging" and battery.time_to_full then
        line = line .. "\n" .. duration(battery.time_to_full) .. " to full"
    end
    return line
end)

-- Warn once each time the charge falls past LOW while draining.
mantle.battery:on_change(function(battery, previous)
    if previous == nil or not battery.present then
        return
    end
    if battery.state == "Discharging" and battery.percent <= LOW and previous.percent > LOW then
        process.detach("notify-send", { "-u", "critical", "Battery low", battery.percent .. "% remaining" })
    end
end)

local pill = row {
    align_v = "Center",
    spacing = 4,
    padding = { left = 8, right = 10, top = 3, bottom = 3 },
    radius = 12,
    background = pill_hover:map(function(on) return on and "#45475a" or "#313244" end),
    hover = pill_hover,
    visible = mantle.battery:map(function(battery) return battery ~= nil and battery.present end),
    children = {
        icon { name = glyph, size = 16, foreground = colour, align_v = "Center" },
        text {
            content = mantle.battery:map(function(battery)
                return battery and battery.present and battery.percent .. "%" or ""
            end),
            foreground = colour,
            align_v = "Center",
        },
    },
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        width = "Fill",
        height = 32,
        exclusive = true,
        child = row {
            width = "Fill",
            height = "Fill",
            padding = { left = 8, right = 8 },
            background = "#1e1e2e",
            children = { rect { width = "Fill" }, pill },
        },
    },
    popup {
        id = "battery_tooltip",
        parent = "bar",
        anchor_rect = hover_rect("battery"),
        anchor = "Bottom",
        gravity = "Bottom",
        offset = { y = 6 },
        grab = false,
        visible = pill_hover,
        padding = 8,
        radius = 8,
        background = "#1e1e2e",
        border_width = 1,
        border_color = "#45475a",
        child = text { content = details, foreground = "#cdd6f4", wrap = "Word" },
    },
}
```

## How it works

- `mantle.battery` is `nil` before its first push and reads `present = false` on a desktop; every map checks both ([battery](../capabilities/battery.md)).
- `state` names UPower's charge state; `time_to_empty` and `time_to_full` are optional and need their own guard.
- The icon is picked by name from the icon theme and tinted with `foreground` ([icon](../nodes/icon.md)).
- `on_change` compares with the previous push to fire only on the downward crossing, and `process.detach` runs `notify-send` outside the shell ([capabilities](../capabilities/index.md), [process.detach](../guide/processes.md#processdetach)).
- The tooltip is a non-grabbing [popup](../surfaces/popup.md#tooltip) anchored to `hover_rect` ([hover](../guide/input.md#hover)).

## Variations

| Change | Edit |
| :--- | :--- |
| Charge as a bar instead of an icon | A 24 × 10 `rect` track with a child `rect { width = battery.percent .. "%", height = "Fill" }` ([sizes](../nodes/index.md#sizes)) |
| Cycle the power profile on click | Make the pill a `button` whose `on_click` picks the next entry of `mantle.power:get().profiles` and invokes `set_profile` ([power](../capabilities/power.md)) |
| Show the wattage | Add `text { content = mantle.power:map(function(power) return power and power.energy_rate and string.format("%.1f W", power.energy_rate) or "" end) }` |
| Different threshold | `LOW = 20` |
| Hide the pill on mains at full charge | `visible` returns `battery ~= nil and battery.present and battery.state ~= "FullyCharged"` |
