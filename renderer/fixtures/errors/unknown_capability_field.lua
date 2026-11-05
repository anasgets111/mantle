-- expect: mantle.audio has no `sinkss`: did you mean `mantle.audio:get().sinks`?
return panel { id = "bar", layer = "top", child = text { content = mantle.audio.sinkss } }
