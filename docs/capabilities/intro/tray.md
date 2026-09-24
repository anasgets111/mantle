```lua
list {
    direction = "Horizontal",
    spacing = 4,
    source = mantle.tray:map(function(tray)
        return tray and tray.items or {}
    end),
    key = function(item) return item.id end,
    itemfn = function(item)
        return button {
            on_click = function(_, which)
                if which == "left" and not item.item_is_menu then
                    mantle.tray:invoke("activate", item.id, 0, 0) -- screen x, y; most apps ignore them
                end
            end,
            children = { icon { name = item.icon_name or item.icon_path or "", size = 16 } },
        }
    end,
}
```

<!-- reference -->

## Backend

Hosts the watcher at `/StatusNotifierWatcher` and registers itself as a host. The name is requested
without `DoNotQueue`: if another host owns it, Mantle queues for it. At start it adopts items
already on the bus at `/StatusNotifierItem`, `/StatusNotifierItem/1` and
`/org/chromium/StatusNotifierItem/1`. Items are dropped, and their spooled PNGs deleted, on
`NameOwnerChanged`.

| Contract | Behavior |
| :--- | :--- |
| Icon | `IconName` found in the item's `IconThemePath`, then `IconName` as a theme name, then the largest valid pixmap |
| Pixmap | Square, 1–128 px, exactly `w × h × 4` ARGB bytes; spooled as PNG under `tray/` |
| Bounds | Strings 256 bytes; menus 1024 nodes, depth 32 |
| Activation | `ItemIsMenu` items get no `Activate`; secondary activate and scroll are unrestricted |
| Menus | `com.canonical.dbusmenu` layout; `menu_will_show` sends `AboutToShow`, `activate_menu_item` sends `Event("clicked")` |

See also: [System tray with menu](../cookbook/tray.md) recipe.
