-- expect: mantle.audio has no `volumee`: did you mean `mantle.audio:get().volume`?
return panel { id = "bar", layer = "top", child = text { content = mantle.audio.volumee } }
