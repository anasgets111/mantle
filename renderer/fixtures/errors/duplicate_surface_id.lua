-- expect: two surfaces declare `bar` (first at shell.lua:3)
return {
  panel { id = "bar", layer = "top" },
  panel { id = "bar", layer = "bottom" },
}
