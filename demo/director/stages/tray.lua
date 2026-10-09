-- Demo: fed `mock_tray` in `mantle.tray`'s shape, so no app of yours shows.
local tray = state("mock_tray", { items = {} })

return list {
    direction = "horizontal",
    spacing = 4,
    align_v = "center",
    margin = { right = 14 },
    source = tray:map(function(t) return t.items end),
    key = function(item) return item.id end,
    itemfn = function(item)
        return rect {
            width = 40,
            height = 40,
            scale = 1,
            animate = {
                scale = { duration = 360, easing = "out_back", from = 0.2 },
                move = { duration = 180, easing = "out_cubic" },
            },
            children = {
                icon { name = item.icon_name, size = 26, align_h = "center", align_v = "center" },
            },
        }
    end,
}
