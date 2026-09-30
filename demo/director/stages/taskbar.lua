-- Demo: the director feeds `mock_windows` in `mantle.windows`' shape with `mantle set`, so none of
-- your window titles reach the take. A real shell reads `mantle.windows` itself.
local theme = require("theme")
local target = require("targets")
local windows = state("mock_windows", { source = "hyprland", windows = {} })

-- A desktop entry names the app's icon; an app without one falls back to its app_id. Nothing
-- until the entries arrive, or the first frame looks up a name no theme has.
local function icon_of(app_id)
    return mantle.applications:map(function(a)
        if not a then return "" end
        local index = a.by_app_id[app_id]
        return index and a.entries[index].icon or app_id
    end)
end

local bar = list {
    direction = "Horizontal",
    spacing = 6,
    align_v = "Center",
    margin = { left = 18 },
    source = windows:map(function(w) return w.windows end),
    key = function(w) return w.id end,
    itemfn = function(w)
        return rect {
            geometry = target("task:" .. w.id),
            height = 44,
            padding = { left = 10, right = w.focused and 16 or 10 },
            radius = 22,
            background = w.focused and theme.surface or "#00000000",
            scale = 1,
            animate = { background = 200, scale = { duration = 360, easing = "OutBack", from = 0.3 } },
            on_click = function() mantle.windows:focus(w.id) end,
            children = {
                row {
                    height = "Fill",
                    spacing = 10,
                    children = {
                        icon { name = icon_of(w.app_id), size = 28, align_v = "Center" },
                        text {
                            content = w.title,
                            visible = w.focused,
                            align_v = "Center",
                            font_size = 18,
                            foreground = theme.text,
                        },
                    },
                },
                rect {
                    width = w.focused and 24 or 6,
                    height = 4,
                    radius = 2,
                    align_h = "Center",
                    align_v = "End",
                    background = w.focused and theme.accent or theme.overlay2,
                    animate = { width = { duration = 260, easing = "OutCubic" }, background = 200 },
                },
            },
        }
    end,
}

return { bar = bar, windows = windows, icon_of = icon_of }
