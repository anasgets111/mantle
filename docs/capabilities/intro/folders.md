```lua
-- `~/` also works; `watched` is keyed by the expanded path
local folder = (os.getenv("HOME") or "") .. "/Pictures/Wallpapers"
mantle.folders:watch(folder, { "jpg", "png" })

list {
    items = mantle.folders:map(function(folders)
        local listing = folders.watched[folder]
        return listing and listing.entries or {}
    end),
    key = function(entry) return entry.path end,
    each = function(entry)
        return text { content = entry.name }
    end,
}
```

<!-- reference -->

## Backend

One inotify watch per folder, on the folder only: subfolders are neither listed nor watched. A new
generation drops every watch, so call `watch` at top level and each evaluation asks again; repeating
a `watch` with the same `extensions` only re-pushes the listing.

A missing folder is watched through its nearest existing ancestor: `ready` is `true` with `error`
set until the folder appears, then it lists with `error` cleared. A watched folder that is deleted
or moved away does the same, and lists again when it comes back.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `watched["~/Pictures"]` is `nil` after `watch("~/Pictures")` | `watched` is keyed by the expanded absolute path: `watched[os.getenv("HOME") .. "/Pictures"]` |
| `watched[path]` is `nil` after a `watch` | The key drops trailing slashes: `watch("/walls/")` lands at `watched["/walls"]` |
| `error` is set but the folder exists | It is set while the folder is missing and clears on the next listing, up to 200 ms after the folder appears |

See also: [`file.read`](../guide/scripting.md#file) to read a file's contents.
