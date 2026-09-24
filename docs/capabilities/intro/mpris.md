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
| Discovery | Session bus `ListNames` once, then `NameOwnerChanged` for `org.mpris.MediaPlayer2.*`. Skips `playerctld` and any player reporting `CanControl = false` |
| Pushes | On a `PlaybackStatus` or `Metadata` change and on `Seeked`. A status change re-reads `Position` 100 ms later. Nothing polls |
| Seek | `seek` calls `SetPosition` with the cached `mpris:trackid`. A player without one gets a relative `Seek` from a live `Position` read |

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a live progress bar | Stamp each new `position_updated_at` with `mantle.system.monotonic` in `on_change`, then add the seconds since: [Media player](../cookbook/media-player.md) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `position` stands still while playing | It is the offset at `position_updated_at`, not polled. Extrapolate, as above |
| `position_updated_at` compared with `mantle.system.monotonic` gives nonsense | Different clocks and units: `CLOCK_MONOTONIC` microseconds against seconds since `system` started. Only compare it with itself |

See also: [Media player](../cookbook/media-player.md) recipe.
