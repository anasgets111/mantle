-- Demo: the director feeds `mock_tray` in `mantle.tray`'s shape with `mantle set`, so no app of yours
-- shows in the take. A real shell reads `mantle.tray` and calls `mantle.tray:activate(id, x, y)`.
local theme = require("theme")
local tray = state("mock_tray", { items = {} })

return list {
    direction = "Horizontal",
    spacing = 4,
    align_v = "Center",
    margin = { right = 14 },
    source = tray:map(function(t) return t.items end),
    key = function(item) return item.id end,
    itemfn = function(item)
        local calling = item.status == "NeedsAttention"
        return rect {
            width = 40,
            height = 40,
            radius = 20,
            background = calling and theme.fade("danger", "33") or "#00000000",
            scale = 1,
            animate = { background = 250, scale = { duration = 360, easing = "OutBack", from = 0.2 } },
            children = {
                icon { name = item.icon_name, size = 26, align_h = "Center", align_v = "Center" },
                rect {
                    visible = calling,
                    width = 12,
                    height = 12,
                    radius = 6,
                    align_h = "End",
                    align_v = "Start",
                    margin = { top = 4, right = 4 },
                    background = theme.danger,
                    opacity = 1,
                    animate = { opacity = { duration = 900, keyframes = { 1, 0.3, 1 }, loops = "Infinite" } },
                },
            },
        }
    end,
}
