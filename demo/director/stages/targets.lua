-- Demo: where the director's drawn pointer glides before it clicks. `mantle call where <name>`
-- answers a named node's box in its surface's coordinates, or nothing before its first layout.
-- `mantle call search <text>` types into the launcher's field, which no key can reach: `set_text`
-- calls no `on_change`, so it sets the query too.
local boxes = {}

action("where", function(name)
    local box = boxes[name]
    return box and box:get() or nil
end)

local query = state("launcher_query", "")

action("search", function(text)
    focus_target("search"):set_text(text)
    query:set(text)
end)

return function(name)
    boxes[name] = boxes[name] or geometry(name)
    return boxes[name]
end
