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

One inotify watch per folder, on the folder only: subfolders are neither listed nor watched. A new
generation drops every watch, so call `watch` at top level and each evaluation asks again; repeating
a `watch` with the same `extensions` only re-pushes the listing.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `watch("~/Pictures")` does nothing | Only absolute paths pass; `~` is not expanded. Build the path from `os.getenv("HOME")` |
| `folders[path]` is `nil` after a `watch` | The key drops trailing slashes: `watch("/walls/")` lands at `folders["/walls"]` |
| A folder created after `watch` never lists | A missing folder records `error` and stays unwatched. `unwatch`, then `watch` again once it exists |

See also: [`process.run`](../guide/processes.md#processrun) to read a file's contents.
