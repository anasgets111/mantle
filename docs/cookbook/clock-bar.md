# Clock bar

A bar across the top of every monitor with a clock in the centre. Clicking the clock switches
between the time and the full date, and hovering it shows the date in a tooltip.

```lua
local show_date = state("clock_show_date", false)
local clock_hover = hover("clock")

local label = computed({ mantle.system, show_date }, function(system, date)
    if system == nil then
        return "--:--" -- nil until the first push
    end
    return os.date(date and "%A %d %B" or "%H:%M", system.time)
end)

local tooltip_text = mantle.system:map(function(system)
    return system and os.date("%A, %d %B %Y", system.time) or ""
end)

local clock = button {
    align_v = "Center",
    padding = { left = 10, right = 10, top = 4, bottom = 4 },
    radius = 6,
    hover = clock_hover,
    background = clock_hover:map(function(on) return on and "#313244" or "#00000000" end),
    animate = { background = 120 },
    on_click = function() show_date:set(not show_date:get()) end,
    children = { text { content = label, font_size = 14, foreground = "#cdd6f4" } },
}

local bar = panel {
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
        children = { rect { width = "Fill" }, clock, rect { width = "Fill" } },
    },
}

local tooltip = popup {
    id = "clock_tooltip",
    parent = "bar",
    anchor_rect = hover_rect("clock"),
    anchor = "Bottom",
    gravity = "Bottom",
    offset = { y = 6 },
    grab = false,
    visible = clock_hover,
    padding = 8,
    radius = 8,
    background = "#1e1e2e",
    border_width = 1,
    border_color = "#45475a",
    child = text { content = tooltip_text, foreground = "#cdd6f4" },
}

return { bar, tooltip }
```

## How it works

- `mantle.system` pushes the time once a second, and `os.date` formats it ([system](../capabilities/system.md)).
- `computed` combines the clock with a [named state](../guide/signals.md#named-state) the click toggles ([derived signals](../guide/signals.md#derived-signals)).
- Both `"Fill"` spacers take an equal share of the row, so the clock sits at the exact centre ([sizes](../nodes/index.md#sizes)).
- The background is on the `row`, not the panel, so the whole bar takes clicks ([input region](../surfaces/index.md#input-region)).
- `hover` and `hover_rect` drive a non-grabbing [popup](../surfaces/popup.md) as a tooltip ([hover](../guide/input.md#hover)).

## Variations

| Change | Edit |
| :--- | :--- |
| Seconds | `"%H:%M:%S"` |
| 12-hour clock | `"%I:%M %p"` |
| Clock on the right | Drop the second spacer: `children = { rect { width = "Fill" }, clock }` |
| One monitor only | `monitor = "DP-1"` on the panel |
| Bottom bar | `anchor = { bottom = true, left = true, right = true }` and `anchor = "Top", gravity = "Top", offset = { y = -6 }` on the tooltip |
