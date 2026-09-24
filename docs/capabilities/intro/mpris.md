```lua
local player = mantle.mpris:map(function(mpris)
    return mpris and mpris.players[1]
end)

button {
    on_click = function()
        local current = player:get()
        if current then
            mantle.mpris:invoke("control", current.id, "play_pause")
        end
    end,
    children = {
        text {
            max_width = 240,
            elide = "End",
            content = player:map(function(current)
                return current and (current.play_state .. ": " .. current.title) or ""
            end),
        },
    },
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Discovery | `ListNames` once, then `NameOwnerChanged` for `org.mpris.MediaPlayer2.*`, excluding `playerctld` |
| Refresh | `PlaybackStatus` and `Metadata` changes and `Seeked` refresh the cache; a status change also re-reads `Position` once 100 ms later |
| Position | Sampled with a `CLOCK_MONOTONIC` timestamp for the config to extrapolate; nothing polls |
| Controls | Play, pause, play/pause, next, previous |
| Seek | Absolute seek is `SetPosition` with the cached track id, else a relative `Seek` from the last known position. |

## How do I…

| Task | Answer |
| :--- | :--- |
| Play or pause whatever is playing | `control` on `players[1].id`, as in the example above |

See also: [Media player](../cookbook/media-player.md) recipe.
