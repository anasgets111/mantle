```lua
mantle.audio:on_change(function(audio)
    local ids = {}
    for _, sink in ipairs(audio.sinks) do
        if sink.active then
            ids[#ids + 1] = sink.id
        end
    end
    mantle.audio:set_level_meters(ids)
end)

return rect {
    height = 6,
    background = "#A6E3A1",
    width = mantle.audio_levels:map(function(levels)
        local level = levels.levels[1]
        return level and level.peak * 2 or 0
    end, 0),
}
```

<!-- reference -->

## Backend

| Source | Feeds |
| :--- | :--- |
| One PipeWire capture stream per id `mantle.audio:set_level_meters` names: a sink's monitor or a source, stereo (FL and FR) at 60 samples a second with `resample.peaks`, so each sample is the peak of the graph samples it replaces | `levels`: each buffer's loudest sample across both channels, held until the next push |

Meters run only between a `set_level_meters` call naming ids and the next call, an empty one stops
them all, and none runs before the first call: reading `audio_levels` alone opens no stream. A
reload keeps them until the next call; a replaced Renderer's stop. The streams share
[`audio`'s thread](audio.md#backend), and a push goes out at most every 33 ms, only when a level
moved by 1 point or more, so a silent or steady device costs no pushes.

## Gotchas

- An id that is not a current sink or source is skipped, not waited for, and a meter ends with its
  device. A device that comes back, or PipeWire restarting, gives new ids: call
  `set_level_meters` again from `mantle.audio:on_change`, which runs no meter twice.
- A metered device keeps running: a sink no longer suspends when nothing plays, since its monitor
  is being read.
- Metering a microphone reads it, so `mantle.privacy` lists the Supervisor among
  `microphone_users`. A sink's monitor is not listed.
- `peak` is linear in full scale, while `volume` is cubic. A sink's monitor carries its streams
  before the sink's own volume, unless the sink sets `monitor.channel-volumes` as Mantle's null
  sinks do, so turning a hardware sink down need not lower its peak.
