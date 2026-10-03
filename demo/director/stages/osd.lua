-- The cookbook's volume OSD, centred on the stage the director's code pane leaves open.
local theme = require("theme")
local layout = require("layout")

local placed = mantle.screens:map(function(screens)
    return layout.center(screens and screens[1], 380)
end)
local osd = state("volume_osd", { volume = 0, muted = false })

mantle.audio:on_change(function(audio, previous)
    if previous == nil or audio.volume == nil then return end
    if audio.volume ~= previous.volume or audio.muted ~= previous.muted then
        osd:set({ volume = audio.volume, muted = audio.muted })
    end
end)

local shown = pulse(osd, 1500)
local mapped = pulse(osd, 1700)

local function percent(entry)
    return math.floor(math.min(entry.volume, 100) + 0.5)
end

return panel {
    id = "volume_osd",
    layer = "overlay",
    output = "active",
    anchor = { top = true, left = true },
    margin = placed:map(function(p) return { top = p.top, left = p.left } end),
    visible = mapped,
    child = row {
        width = placed:map(function(p) return p.width end),
        height = 64,
        spacing = 16,
        padding = { left = 22, right = 22 },
        align_v = "center",
        radius = 32,
        background = theme.fade("base", "f2"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = shown:map(function(on) return on and 1 or 0 end),
        translate = shown:map(function(on) return { y = on and 0 or -16 } end),
        animate = {
            opacity = { duration = 150, from = 0 },
            translate = { duration = 260, easing = "out_back", from = { y = -16 } },
        },
        children = {
            icon { name = "audio-volume-high-symbolic", size = 28, foreground = theme.text, align_v = "center" },
            rect {
                width = "fill",
                height = 8,
                radius = 4,
                align_v = "center",
                background = theme.overlay,
                children = {
                    rect {
                        height = "fill",
                        radius = 4,
                        background = theme.accent,
                        width = osd:map(function(entry) return percent(entry) .. "%" end),
                        animate = { width = { duration = 160, easing = "out_cubic" } },
                    },
                },
            },
            text {
                content = osd:map(function(entry) return percent(entry) .. "%" end),
                width = 60,
                text_align = "end",
                align_v = "center",
                font_size = 20,
                foreground = theme.text,
            },
        },
    },
}
