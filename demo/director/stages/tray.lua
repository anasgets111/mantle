-- Demo: the director feeds `mock_tray` in `mantle.tray`'s shape with `mantle set`, so no app of yours
-- shows in the take. A real shell reads `mantle.tray` and calls `mantle.tray:activate(id, x, y)`.
local theme = require("theme")
local tray = state("mock_tray", { items = {} })

return list {
    direction = "horizontal",
    spacing = 4,
    align_v = "center",
    margin = { right = 14 },
    source = tray:map(function(t) return t.items end),
    key = function(item) return item.id end,
    itemfn = function(item)
        local calling = item.status == "needs_attention"
        return rect {
            width = 40,
            height = 40,
            radius = 20,
            background = calling and theme.fade("danger", "33") or "#00000000",
            scale = 1,
            animate = {
                background = 250,
                scale = { duration = 360, easing = "out_back", from = 0.2 },
                move = { duration = 180, easing = "out_cubic" },
            },
            children = {
                icon { name = item.icon_name, size = 26, align_h = "center", align_v = "center" },
                rect {
                    visible = calling,
                    width = 12,
                    height = 12,
                    radius = 6,
                    align_h = "end",
                    align_v = "start",
                    margin = { top = 4, right = 4 },
                    background = theme.danger,
                    opacity = 1,
                    animate = { opacity = { duration = 900, keyframes = { 1, 0.3, 1 }, loops = "infinite" } },
                },
            },
        }
    end,
}
