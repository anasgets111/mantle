-- expect: shell.lua:2: no sinks
local label = computed({ mantle.audio }, function(audio) return audio and error("no sinks") or "" end)
return panel { id = "bar", layer = "top", child = text { content = label } }
