-- expect: mantle.audio:get().sinks[1] has no `volum`: did you mean `volume`?
local level = mantle.audio:map(function(audio) return tostring(audio.sinks[1].volum) end, "")
return panel { id = "bar", layer = "top", child = text { content = level } }
