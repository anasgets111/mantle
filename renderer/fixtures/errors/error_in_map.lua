-- expect: attempt to index a number value (local 'v')
local n = state("n", 1)
return panel { id = "bar", layer = "top", child = text { content = n:map(function(v) return v.x.y end) } }
