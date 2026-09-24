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
                    mantle.tray:activate(item.id, 0, 0) -- screen x, y; most apps ignore them
                end
            end,
            children = { icon { name = item.icon_name or item.icon_path or "", size = 16 } },
        }
    end,
}
```

<!-- reference -->

## Backend

Mantle hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` on the session bus and
registers itself as a host.

| Contract | Behavior |
| :--- | :--- |
| Name | Requested without `DoNotQueue`: if another watcher owns it, Mantle queues behind it |
| Adoption | At start, adopts items already on the bus at `/StatusNotifierItem`, `/StatusNotifierItem/1` or `/org/chromium/StatusNotifierItem/1`, for apps that never re-register |
| Removal | An item leaves, and its spooled PNGs are deleted, when its bus name loses its owner |
| Icon | `IconName` found in the item's `IconThemePath`, then `IconName` as a theme name, then the largest valid pixmap: square, 1 to 128 px, exactly `w × h × 4` ARGB bytes, spooled as a PNG under `tray/` |
| Bounds | Strings 256 bytes; menus 1024 nodes, depth 32 |
| Menus | `com.canonical.dbusmenu`. `menu_will_show` sends `AboutToShow`, `activate_menu_item` sends `Event("clicked")` |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `activate` does nothing on some items | The item set `item_is_menu`, and Mantle skips `Activate` for it. Open `menu` on left click |
| A submenu is empty | Some apps fill submenus only after `AboutToShow`. Send `menu_will_show` with the submenu's `id` before drawing it |

See also: [System tray with menu](../cookbook/tray.md) recipe.
