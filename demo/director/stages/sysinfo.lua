-- Real, unlike the mocks: this machine's CPU and memory, read by `mantle.sysinfo` once a second.
local theme = require("theme")

mantle.sysinfo:configure({ cpu_interval = 1, ram_interval = 2 })

local SAMPLES = 24
local history = state("cpu_history", {})

mantle.sysinfo:on_change(function(now)
    local out = {}
    local past = history:get()
    for k = math.max(1, #past - SAMPLES + 2), #past do
        out[#out + 1] = past[k]
    end
    out[#out + 1] = now.cpu_percent
    history:set(out)
end)

local function spark(samples)
    local bars = {}
    for k, value in ipairs(samples) do
        bars[k] = rect {
            width = 4,
            height = math.max(2, math.floor(value * 26 / 100)),
            radius = 2,
            align_v = "end",
            background = theme.accent,
        }
    end
    return bars
end

local function chip(label, value)
    return row {
        spacing = 8,
        align_v = "center",
        children = {
            text { content = label, align_v = "center", font_size = 16, font_weight = 700, foreground = theme.muted },
            text { content = value, align_v = "center", font_size = 18, foreground = theme.text },
        },
    }
end

return rect {
    margin = { right = 18 },
    height = 40,
    align_v = "center",
    padding = { left = 14, right = 16 },
    radius = 20,
    background = theme.surface,
    visible = mantle.sysinfo:map(function(s) return s ~= nil end),
    children = {
        row {
            height = "fill",
            spacing = 14,
            children = {
                row { height = 28, align_v = "center", spacing = 2, children = history:map(spark) },
                chip("CPU", mantle.sysinfo:map(function(s) return s and (s.cpu_percent .. "%") or "" end)),
                chip("RAM", mantle.sysinfo:map(function(s) return s and (s.ram_percent .. "%") or "" end)),
            },
        },
    },
}
