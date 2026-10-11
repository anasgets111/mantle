```lua
local player = mantle.mpris:map(function(mpris)
    return mpris.players[1]
end)

rect {
    on_click = function()
        local current = player:get()
        if current then
            mantle.mpris:control(current.id, "play_pause")
        end
    end,
    children = {
        text {
            max_width = 240,
            elide = "end",
            content = player:map(function(current)
                return current and (current.play_state .. ": " .. current.title) or ""
            end, ""),
        },
    },
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Discovery | Session bus `ListNames` once, then `NameOwnerChanged` for `org.mpris.MediaPlayer2.*`. Skips `playerctld` and any player reporting `CanControl = false` |
| Pushes | On playback, metadata, control-property, TrackList or Playlists changes and on `Seeked`. A status change re-reads `Position` 100 ms later. Nothing polls |
| Seek | `seek` calls `SetPosition` with the cached `mpris:trackid`. A player without one gets a relative `Seek` from a live `Position` read |
| Controls | Read `can_*` before calling matching methods. Setters accept a finite nonnegative volume percent, positive finite rate within advertised limits, and `"none"`, `"track"`, or `"playlist"` loop status |
| Artwork | Uses existing local `file://` paths. Remote artwork URLs are unsupported |

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a live progress bar | Compare `position_updated_at / 1e6` with `mantle.clock:monotonic()`, or stamp each new report in `on_change` and add the seconds since: [Media player](../cookbook/media-player.md) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `position` stands still while playing | It is the offset at `position_updated_at`, not polled. Extrapolate, as above |
| `position_updated_at` compared with `mantle.clock.monotonic` is off by a million | Same clock, different unit: microseconds against seconds. Divide by `1e6`; use `mantle.clock:monotonic()` for a live reading |

See also: [Media player](../cookbook/media-player.md) recipe.
