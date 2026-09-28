-- The cookbook's volume OSD, sized for the recording and centred on the 56% of the screen the
-- director's code pane leaves open.
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
    return math.floor(math.min(entry.volume, 1) * 100 + 0.5)
end

return panel {
    id = "volume_osd",
    layer = "Overlay",
    monitor = "Active",
    anchor = { top = true, left = true },
    margin = mantle.screens:map(function(screens)
        local width = screens[1] and screens[1].width or 1920
        return { top = 24, left = math.floor((width * 0.56 - 380) / 2) }
    end),
    visible = mapped,
    child = row {
        width = 380,
        height = 64,
        spacing = 16,
        padding = { left = 22, right = 22 },
        align_v = "Center",
        radius = 32,
        background = "#1e1e2ef2",
        border_width = 1,
        border_color = "#45475a",
        opacity = shown:map(function(on) return on and 1 or 0 end),
        translate = shown:map(function(on) return { y = on and 0 or -16 } end),
        animate = {
            opacity = { duration = 150, from = 0 },
            translate = { duration = 260, easing = "OutBack", from = { y = -16 } },
        },
        children = {
            icon { name = "audio-volume-high-symbolic", size = 28, foreground = "#cdd6f4", align_v = "Center" },
            rect {
                width = "Fill",
                height = 8,
                radius = 4,
                align_v = "Center",
                background = "#45475a",
                children = {
                    rect {
                        height = "Fill",
                        radius = 4,
                        background = "#89b4fa",
                        width = osd:map(function(entry) return percent(entry) .. "%" end),
                        animate = { width = { duration = 160, easing = "OutCubic" } },
                    },
                },
            },
            text {
                content = osd:map(function(entry) return percent(entry) .. "%" end),
                width = 60,
                text_align = "End",
                align_v = "Center",
                font_size = 20,
                foreground = "#cdd6f4",
            },
        },
    },
}
