-- expect: shape.parts[2]
return panel {
    id = "bar",
    layer = "top",
    child = rect {
        width = 40,
        height = 40,
        shape = { melt = 4, parts = {
            { left = 0, top = 0, right = 20, bottom = 20 },
            { left = { from = "top" }, top = 0, right = 40, bottom = 40 },
        } },
    },
}
