```lua
local player = mantle.mpris:map(function(mpris)
    return mpris and mpris.players[1]
end)

button {
    on_click = function()
        local current = player:get()
        if current then
            mantle.mpris:control(current.id, "play_pause")
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

Each player may also expose `track_list` and `playlists`. `track_list.tracks` contains at most 100
entries around the current track, each with an id, title, artist, and length. The full playlist
remains player-owned. `playlists.playlists` contains a page of at most 100 entries. Call
`playlists_get(id, index, count, order, reverse)` to fetch another page, and
`playlists_activate(id, playlist_id)` to activate one. `track_list_add_track`,
`track_list_remove_track`, and `track_list_go_to` call the corresponding TrackList methods.

Each player also exposes `album`, `album_artist`, and `genre`. Artist and genre arrays are joined
with `", "`; missing metadata is an empty string.

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Discovery | Session bus `ListNames` once, then `NameOwnerChanged` for `org.mpris.MediaPlayer2.*`. Skips `playerctld` and any player reporting `CanControl = false` |
| Pushes | On playback, metadata, control-property, TrackList or Playlists changes and on `Seeked`. A status change re-reads `Position` 100 ms later. Nothing polls |
| Seek | `seek` calls `SetPosition` with the cached `mpris:trackid`. A player without one gets a relative `Seek` from a live `Position` read |
| Controls | Read `can_*` before calling matching methods. Setters accept finite nonnegative volume, positive finite rate within advertised limits, and `None`, `Track`, or `Playlist` loop status |
| Artwork | Uses existing local `file://` paths. Remote artwork URLs are unsupported |

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
