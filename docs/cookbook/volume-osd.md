# Volume OSD

A card near the bottom of the focused monitor that shows the volume for a moment whenever it
changes, whether from a media key, `wpctl` or a mixer. It fades and slides in, then out.

```lua
-- The last change worth showing. A fresh table on every set, so each change counts as new.
local osd = state("volume_osd", { volume = 0, muted = false })

mantle.audio:on_change(function(audio, previous)
    if previous == nil or audio.volume == nil then
        return -- the first push is learned state, not a change
    end
    if audio.volume ~= previous.volume or audio.muted ~= previous.muted then
        osd:set({ volume = audio.volume, muted = audio.muted })
    end
end)

local shown = pulse(osd, 1500) -- true for 1.5 s after each change
local mapped = pulse(osd, 1700) -- keeps the surface up while the card fades out

local function percent(entry)
    return math.floor(math.min(entry.volume, 1) * 100 + 0.5)
end

local glyph = osd:map(function(entry)
    if entry.muted or entry.volume == 0 then
        return "audio-volume-muted-symbolic"
    end
    local level = percent(entry)
    return level < 34 and "audio-volume-low-symbolic"
        or level < 67 and "audio-volume-medium-symbolic"
        or "audio-volume-high-symbolic"
end)

return {
    panel {
        id = "volume_osd",
        layer = "Overlay",
        monitor = "Active",
        anchor = { bottom = true }, -- no left/right: centred, width measured
        margin = { bottom = 80 },
        visible = mapped,
        child = row {
            width = 280,
            height = 48,
            spacing = 12,
            padding = { left = 16, right = 16 },
            align_v = "Center",
            radius = 24,
            background = "#1e1e2ee6",
            border_width = 1,
            border_color = "#45475a",
            opacity = shown:map(function(on) return on and 1 or 0 end),
            translate = shown:map(function(on) return { y = on and 0 or 12 } end),
            animate = {
                opacity = { duration = 150, from = 0 },
                translate = { duration = 200, easing = "OutCubic", from = { y = 12 } },
            },
            children = {
                icon { name = glyph, size = 20, foreground = "#cdd6f4", align_v = "Center" },
                rect {
                    width = "Fill",
                    height = 6,
                    radius = 3,
                    align_v = "Center",
                    background = "#45475a",
                    children = {
                        rect {
                            height = "Fill",
                            radius = 3,
                            background = osd:map(function(entry) return entry.muted and "#6c7086" or "#89b4fa" end),
                            width = osd:map(function(entry) return percent(entry) .. "%" end),
                            animate = { width = { duration = 120 } },
                        },
                    },
                },
                text {
                    content = osd:map(function(entry) return entry.muted and "Muted" or percent(entry) .. "%" end),
                    width = 44,
                    text_align = "End",
                    align_v = "Center",
                    font_size = 13,
                    foreground = "#cdd6f4",
                },
            },
        },
    },
}
```

Bind the volume keys to anything that changes the default sink, for example
`wpctl set-volume -l 1 @DEFAULT_AUDIO_SINK@ 5%+`; the OSD follows the push.

## How it works

- `on_change` reacts to each audio push and skips the first one, which is learned state ([capabilities](../capabilities/index.md), [audio](../capabilities/audio.md)).
- It writes a fresh table into a [named state](../guide/signals.md#named-state); `pulse` reads `true` for a while after each change ([pulse](../guide/signals.md#pulse-mark-a-change)).
- A longer second `pulse` keeps the surface mapped while the card fades, since hiding a surface plays no exit ([delay](../guide/signals.md#delay-hold-a-value) is the general form).
- `monitor = "Active"` shows it on the focused output, and a bottom-only anchor centres it ([panel monitor](../surfaces/panel.md#monitor), [OSD](../surfaces/panel.md#osd)).
- The fill is a `"NN%"` width inside a fixed track ([sizes](../nodes/index.md#sizes)); `translate` and `opacity` animate without re-laying out ([animation](../guide/animation.md)).
- The glyph comes from the icon theme by name ([icon](../nodes/icon.md)).

## Variations

| Change | Edit |
| :--- | :--- |
| Brightness too | A second `on_change` on `mantle.brightness` writing `{ volume = brightness.percent / 100, muted = false }` into the same state |
| Show above 100% | Drop `math.min(entry.volume, 1)` and give the track `width = 300` with the fill at `entry.volume / 1.5` |
| Top of the screen | `anchor = { top = true }`, `margin = { top = 80 }` and `from = { y = -12 }` |
| Every monitor | Remove `monitor = "Active"` |
| Longer on screen | `pulse(osd, 3000)` and `pulse(osd, 3200)` |
