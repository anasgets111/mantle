-- A launcher-style list window: `hs_rows` keyed rows, fuzzy-filtered by `hs_query`, scrollable,
-- with a per-row selection highlight the way a launcher draws one.
local theme = require("theme")

local query = state("hs_query", "")
local count = state("hs_rows", 1000)
local selected = state("hs_selected", 1)
local LIST = scroll("hs_list")

local WORDS = { "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango" }

-- Built once per evaluation; `hs_rows` only picks how many of them the list sees.
local ALL = {}
for i = 1, 2000 do
    ALL[i] = { id = i, name = string.format("%s %s %04d", WORDS[i % 20 + 1], WORDS[(i * 7) % 20 + 1], i) }
end

local results = computed({ query, count }, function(needle, n)
    local found = {}
    if needle == nil or needle == "" then
        for i = 1, n do found[i] = ALL[i] end
        return found
    end
    for i = 1, n do
        local item = ALL[i]
        local score = fuzzy(item.name, needle)
        if score then found[#found + 1] = { id = item.id, name = item.name, score = score } end
    end
    table.sort(found, function(a, b)
        if a.score ~= b.score then return a.score > b.score end
        return a.id < b.id
    end)
    return found
end)

local function row_key(item) return tostring(item.id) end

local function row_of(item)
    theme.counters.list_item = theme.counters.list_item + 1
    return row {
        width = "Fill",
        height = 22,
        padding = { left = 8, right = 8 },
        spacing = 8,
        radius = 4,
        background = computed({ results, selected, theme.mode }, function(found, index, name)
            if found[index] == item then return name == "dark" and "#45475a" or "#bcc0cc" end
            return nil
        end),
        children = {
            text { content = item.name, width = "Fill", align_v = "Center", font_size = 12, foreground = theme.color("fg") },
            text {
                content = item.score and tostring(item.score) or "",
                align_v = "Center",
                font_size = 11,
                foreground = theme.color("muted"),
            },
        },
    }
end

local function pick(index)
    selected:set(index)
    LIST:reveal(index)
end

return {
    pick = pick,
    query = query,
    surface = function(visible, close)
        return window {
            id = "hs_list",
            title = "Heavy list",
            app_id = "mantle.heavy.list",
            min_size = { width = 360, height = 420 },
            max_size = { width = 360, height = 420 },
            visible = visible,
            on_close = close,
            child = column {
                width = "Fill",
                height = "Fill",
                padding = 8,
                spacing = 6,
                background = theme.color("bg"),
                children = {
                    rect {
                        width = "Fill",
                        padding = { left = 8, right = 8 },
                        radius = 6,
                        background = theme.color("surface"),
                        children = {
                            textfield {
                                width = "Fill",
                                height = 28,
                                font_size = 13,
                                foreground = theme.color("fg"),
                                placeholder = "Filter",
                                on_change = function(text)
                                    query:set(text)
                                    pick(1)
                                end,
                                on_navigate = function(key)
                                    local step = ({ up = -1, down = 1 })[key]
                                    if step then pick(math.max(1, math.min(#results:get(), selected:get() + step))) end
                                end,
                            },
                        },
                    },
                    text {
                        content = results:map(function(found) return string.format("%d matches", #found) end),
                        font_size = 11,
                        foreground = theme.color("muted"),
                    },
                    list {
                        width = "Fill",
                        height = "Fill",
                        spacing = 1,
                        scroll = LIST,
                        source = results,
                        key = row_key,
                        itemfn = row_of,
                    },
                },
            },
        }
    end,
}
