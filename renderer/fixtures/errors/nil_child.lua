-- expect: children[2]: expected a node table, got nil
return panel { id = "bar", layer = "top", child = rect { children = { text { content = "a" }, nil, text { content = "b" } } } }
