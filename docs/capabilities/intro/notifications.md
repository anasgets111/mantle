```lua
button {
    on_click = function()
        local notifications = mantle.notifications:get()
        if notifications then
            mantle.notifications:invoke("set_dnd", not notifications.dnd)
        end
    end,
    children = {
        text {
            content = mantle.notifications:map(function(notifications)
                return (notifications and notifications.dnd) and "DND on" or "DND off"
            end),
        },
    },
}
```

<!-- reference -->

## Backend

Mantle is the `org.freedesktop.Notifications` server on the session bus.

| Contract | Behavior |
| :--- | :--- |
| Name | Requested with `DoNotQueue`. If `mako`, `dunst` or another daemon owns it, the server stays off for the run and `feed` stays empty |
| Retention | 100-entry FIFO; `feed` shows the newest 20 |
| Expiry | A negative `expire_timeout` means 5 s. Critical and `0` never expire |
| Close reasons | `NotificationClosed` sends 1 expired, 2 `dismiss` or `reply`, 3 `CloseNotification` or `invoke_action`, 4 evicted from the FIFO. `reply` also emits `NotificationReplied(id, text)` |
| Body markup | Keeps `<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`. Other tags lose their markup and keep their text; `script` and `style` lose both |
| Images | A path in `image-path` or `<img>` must be a regular file under `/usr/share/icons`, `/usr/share/pixmaps`, `$XDG_DATA_HOME/icons` or `~/.icons`; an `image-path` without `/` and every action icon are theme names. Raw `image-data` (8-bit RGB or RGBA, at most 128 px a side) is spooled as `notifications/notif-<id>.png` |
| Sound | Nothing plays for a tier until `set_sound` registers a file for it, except a client's own `sound-file`. Order: `suppress-sound` or `set_app_muted` silences, critical included; else the client's `sound-file`; else `sound-name` from the freedesktop theme, only for a registered tier; else the tier's file. Every file sits under `/usr/share`, `/usr/local/share`, `/opt` or `$XDG_DATA_HOME` and is Ogg Vorbis or 16-bit WAV, at most 4 MiB and 30 s |

## How do I…

### List notifications and dismiss one on click

```lua
list {
    spacing = 6,
    source = mantle.notifications:map(function(notifications)
        return notifications and notifications.feed or {}
    end),
    key = function(item) return tostring(item.id) end,
    itemfn = function(item)
        return button {
            width = 320,
            padding = 8,
            radius = 8,
            background = "#1E1E2E",
            on_click = function() mantle.notifications:invoke("dismiss", item.id) end,
            children = {
                column {
                    children = {
                        text { content = item.summary, font_size = 13, elide = "End", width = "Fill" },
                        text { content = item.app_name, font_size = 11, foreground = "#A6ADC8" },
                    },
                },
            },
        }
    end,
}
```

See also: [Notification popups](../cookbook/notifications.md) recipe.
