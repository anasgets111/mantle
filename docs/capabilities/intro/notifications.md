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

| Contract | Behavior |
| :--- | :--- |
| Name | Requested with `DoNotQueue`; if another daemon owns it, the D-Bus server stays off for the run |
| Retention | 100-entry FIFO. Eviction emits `NotificationClosed` reason 4 |
| Text bounds | App name 64 bytes, summary 128, body 512, 8 actions with 64-byte labels; UTF-8-safe truncation |
| Body markup | Allowlist: `<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`. Other tags are stripped and their text kept; `script`/`style` content is dropped |
| Images | `image-path`, action icons and `<img>` must be regular files under `/usr/share/icons`, `/usr/share/pixmaps`, `$XDG_DATA_HOME/icons` or `~/.icons`. Raw `image-data` (8-bit, ≤ 128 px) is validated and spooled as `notifications/notif-<id>.png` |
| Expiry | A negative timeout means 5 s. A retained entry is marked expired, a `transient` one removed. Critical and `0` never expire. `hold_expiry` pauses countdowns, capped at 300 s |
| Removal | `invoke_action` and `reply` remove the entry unless `resident`; `dismiss` always does. Close reasons: 1 expired, 2 `dismiss` or `reply`, 3 `CloseNotification` or `invoke_action`, 4 evicted |
| Reply | Emits `NotificationReplied(id, text)` |
| DND, quiet | Gate sound only; critical plays through both |
| Sound | Per-urgency Ogg Vorbis or 16-bit WAV (≤ 4 MiB, ≤ 30 s). `suppress-sound` or a muted app (by app name or desktop entry) silences, critical included; else the client's `sound-file`; else `sound-name` from the freedesktop theme, only where a tier sound is registered; else the tier sound. Roots: `/usr/share`, `/usr/local/share`, `/opt`, `$XDG_DATA_HOME` |

## How do I…

### List notifications and dismiss one on click

A click dismisses by the snapshot's `id`:

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
