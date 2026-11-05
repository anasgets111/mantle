-- expect: with sample capability data: shell.lua:2: mantle.audio:get() has no `volume`: expected one of `apps`, `cards`
mantle.audio:on_change(function(audio) print(audio.volume) end)
return panel { id = "bar", layer = "top", child = text { content = "x" } }
