-- expect: `spin` is not a property of a `text` node
return panel { id = "bar", layer = "top", child = text { content = "a", animate = { spin = 0.2 } } }
