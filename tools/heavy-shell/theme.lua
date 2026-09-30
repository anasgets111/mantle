-- The theme every surface reads. `hs_theme` flips between the two palettes.
local theme = state("hs_theme", "dark")

local PALETTES = {
    dark = {
        bg = "#1e1e2e",
        surface = "#313244",
        overlay = "#45475a",
        fg = "#cdd6f4",
        muted = "#a6adc8",
        accent = "#89b4fa",
    },
    light = {
        bg = "#eff1f5",
        surface = "#ccd0da",
        overlay = "#bcc0cc",
        fg = "#4c4f69",
        muted = "#6c6f85",
        accent = "#1e66f5",
    },
}

-- Run counts the engine never sees: `driver.lua` logs them to show which surface re-ran what.
local M = { mode = theme, counters = { bar_child = 0, bar_color = 0, list_item = 0, cal_cell = 0, gallery_row = 0 } }

-- One signal per key, shared by every caller: a `map` made per row is re-run per row on a flip.
local colors = {}

--- A signal answering one palette key, e.g. `M.color("bg")`.
function M.color(key)
    colors[key] = colors[key] or theme:map(function(name) return (PALETTES[name] or PALETTES.dark)[key] end)
    return colors[key]
end

--- `M.color`, counting its runs under `counters[counter]`.
function M.counted(key, counter)
    return theme:map(function(name)
        M.counters[counter] = M.counters[counter] + 1
        return (PALETTES[name] or PALETTES.dark)[key]
    end)
end

return M
