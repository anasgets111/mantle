-- expect: mantle.folders:get().watched["sample"] has no `entires`: did you mean `entries`?
local first = mantle.folders:map(function(folders)
    for _, folder in pairs(folders.watched) do
        return tostring(folder.entires)
    end
    return ""
end, "")
return panel { id = "bar", layer = "top", child = text { content = first } }
