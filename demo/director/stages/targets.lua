-- Demo: where the director's drawn pointer glides before it clicks. `mantle call where <name>`
-- answers a named node's box in its surface's coordinates, or nothing before its first layout.
local boxes = {}

action("where", function(name)
    local box = boxes[name]
    return box and box:get() or nil
end)

return function(name)
    boxes[name] = boxes[name] or geometry(name)
    return boxes[name]
end
