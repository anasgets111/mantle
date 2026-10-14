-- expect: must be within [0, 1], got integer 5
return panel { id = "bar", layer = "top", child = rect { opacity = 5 } }
