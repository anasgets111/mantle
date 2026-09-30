-- A month grid window: 42 day cells, each reading the theme and the selected day.
local theme = require("theme")

local offset = state("hs_cal_offset", 0) -- months from the evaluation's month
local selected = state("hs_cal_day", 1)

local NOW = os.date("*t")

local function month_of(shift)
    local t = os.time({ year = NOW.year, month = NOW.month + shift, day = 1, hour = 12 })
    local first = os.date("*t", t)
    local days = os.date("*t", os.time({ year = first.year, month = first.month + 1, day = 0, hour = 12 })).day
    return first, days
end

local function cell(day, in_month)
    theme.counters.cal_cell = theme.counters.cal_cell + 1
    return button {
        width = 40,
        height = 32,
        radius = 6,
        background = computed({ theme.mode, selected }, function(name, pick)
            if in_month and pick == day then return name == "dark" and "#89b4fa" or "#1e66f5" end
            return name == "dark" and "#313244" or "#ccd0da"
        end),
        animate = { background = 150 },
        on_click = function() if in_month then selected:set(day) end end,
        children = {
            text {
                content = tostring(day),
                align_h = "Center",
                align_v = "Center",
                font_size = 12,
                foreground = in_month and theme.color("fg") or theme.color("muted"),
            },
        },
    }
end

local weeks = offset:map(function(shift)
    local first, days = month_of(shift)
    local lead = (first.wday + 5) % 7 -- Monday first
    local _, previous_days = month_of(shift - 1)
    local rows = {}
    for week = 0, 5 do
        local cells = {}
        for weekday = 1, 7 do
            local n = week * 7 + weekday - lead
            if n < 1 then
                cells[weekday] = cell(previous_days + n, false)
            elseif n > days then
                cells[weekday] = cell(n - days, false)
            else
                cells[weekday] = cell(n, true)
            end
        end
        rows[week + 1] = row { spacing = 4, children = cells }
    end
    return rows
end)

local title = offset:map(function(shift)
    local first = month_of(shift)
    return os.date("%B %Y", os.time({ year = first.year, month = first.month, day = 1, hour = 12 }))
end)

return function(visible, close)
    return window {
        id = "hs_calendar",
        title = "Heavy calendar",
        app_id = "mantle.heavy.calendar",
        min_size = { width = 330, height = 300 },
        max_size = { width = 330, height = 300 },
        visible = visible,
        on_close = close,
        child = column {
            width = "Fill",
            height = "Fill",
            padding = 10,
            spacing = 4,
            background = theme.color("bg"),
            children = {
                text { content = title, font_size = 15, foreground = theme.color("accent") },
                column { spacing = 4, children = weeks },
            },
        },
    }
end
