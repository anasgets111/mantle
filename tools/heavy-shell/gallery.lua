-- An image grid window over a watched icon folder, plus one animated GIF when the system has it.
local theme = require("theme")

local FOLDER = "/usr/share/icons/breeze/apps/48"
local GIF = "/usr/share/doc/satty/assets/usage.gif"
local PER_ROW = 6
local GRID = scroll("hs_gallery")

mantle.files:watch(FOLDER, { "svg", "png" })

local rows = mantle.files:map(function(files)
    local listing = files and files.folders[FOLDER]
    local entries = listing and listing.entries or {}
    local out = {}
    for i = 1, #entries, PER_ROW do
        local group = {}
        for j = i, math.min(i + PER_ROW - 1, #entries) do group[#group + 1] = entries[j].path end
        out[#out + 1] = group
    end
    return out
end)

local function row_key(group) return group[1] end

local function tile_row(group)
    theme.counters.gallery_row = theme.counters.gallery_row + 1
    local tiles = {}
    for i, path in ipairs(group) do
        tiles[i] = rect {
            padding = 4,
            radius = 6,
            background = theme.color("surface"),
            children = { image { source = path, async = true, fit = "contain", width = 40, height = 40 } },
        }
    end
    return row { spacing = 6, children = tiles }
end

return {
    scroll = GRID,
    surface = function(visible, close)
        return window {
            id = "hs_gallery",
            title = "Heavy gallery",
            app_id = "mantle.heavy.gallery",
            min_size = { width = 340, height = 420 },
            max_size = { width = 340, height = 420 },
            visible = visible,
            on_close = close,
            child = column {
                width = "fill",
                height = "fill",
                padding = 8,
                spacing = 8,
                background = theme.color("bg"),
                children = {
                    image { source = GIF, width = 160, height = 90, fit = "contain", visible = state("hs_gif", true) },
                    list {
                        width = "fill",
                        height = "fill",
                        spacing = 6,
                        scroll = GRID,
                        source = rows,
                        key = row_key,
                        itemfn = tile_row,
                    },
                },
            },
        }
    end,
}
