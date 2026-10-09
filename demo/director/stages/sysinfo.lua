-- Real, unlike the mocks: this machine's CPU and memory, read by `mantle.sysinfo` once a second.
local theme = require("theme")

mantle.sysinfo:configure({ cpu_interval = 1, ram_interval = 2 })

local SAMPLES = 24
local history = state("cpu_history", {})

mantle.sysinfo:on_change(function(now)
    local out = {}
    local past = history:get()
    for k = math.max(1, #past - SAMPLES + 1), #past do
        out[#out + 1] = past[k]
    end
    out[#out + 1] = now.cpu_percent
    history:set(out)
end)

-- A fixed 0..100 scale, so an idle machine is a flat line on the floor, not a graph of noise.
-- SAMPLES + 1 points, the first one left of the box: each reading shifts the line one STEP right
-- and lets it slide back, while `trim_axis = "x"` hides what lies outside the box.
local W, H = 96, 28
local STEP = (W - 4) / (SAMPLES - 1)

local function spark(samples)
    local commands = {}
    for k = 1, SAMPLES + 1 do
        local value = samples[#samples - SAMPLES - 1 + k] or 0
        local x, y = 2 + (k - 2) * STEP, H - 2 - value * (H - 4) / 100
        commands[k] = { op = k == 1 and "M" or "L", points = { x, y } }
    end
    return commands
end

local function chip(label, value)
    return row {
        spacing = 8,
        align_v = "center",
        children = {
            text {
                content = label,
                align_v = "center",
                font_size = 16,
                font_weight = 700,
                foreground = theme.muted,
            },
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
    animate = { move = { duration = 180, easing = "out_cubic" } },
    visible = mantle.sysinfo:map(function(s) return s ~= nil end),
    children = {
        row {
            height = "fill",
            spacing = 14,
            children = {
                rect {
                    width = W,
                    height = H,
                    align_v = "center",
                    radius = 6,
                    background = theme.fade("overlay", "80"),
                    children = {
                        path {
                            width = W,
                            height = H,
                            stroke = theme.accent,
                            stroke_width = 2,
                            stroke_cap = "round",
                            stroke_join = "round",
                            trim_axis = "x",
                            commands = history:map(spark),
                            animate = pulse(history, 950):map(function(on)
                                if not on then return {} end
                                return {
                                    shift = {
                                        duration = 1000,
                                        easing = "linear",
                                        keyframes = { { x = STEP, y = 0 }, { x = 0, y = 0 } },
                                    },
                                }
                            end),
                        },
                    },
                },
                chip("CPU", mantle.sysinfo:map(function(s) return s and (s.cpu_percent .. "%") or "" end)),
                chip("RAM", mantle.sysinfo:map(function(s) return s and (s.ram_percent .. "%") or "" end)),
            },
        },
    },
}
