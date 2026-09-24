```lua
local folder = (os.getenv("HOME") or "") .. "/Pictures/Wallpapers"
mantle.files:invoke("watch", folder, { "jpg", "png" })

list {
    source = mantle.files:map(function(files)
        local listing = files and files.folders[folder]
        return listing and listing.entries or {}
    end),
    key = function(entry) return entry.path end,
    itemfn = function(entry)
        return text { content = entry.name }
    end,
}
```

<!-- reference -->

## Backend

One inotify watch per watched absolute folder. It relists 200 ms after the last event; a deleted
folder is not re-watched.
