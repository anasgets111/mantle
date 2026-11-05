```lua
list {
    items = mantle.audio:map(function(audio)
        return audio.sinks
    end),
    key = function(sink) return tostring(sink.id) end,
    each = function(sink)
        return rect {
            padding = 6,
            background = sink.active and "#45475A" or "#1E1E2E",
            on_click = function() mantle.audio:set_default_sink(sink.id) end,
            children = { text { content = sink.name } },
        }
    end,
}
```

<!-- reference -->

## Backend

PipeWire's native API, on one thread shared with [`privacy`](privacy.md#backend).

| PipeWire object | Feeds |
| :--- | :--- |
| `Audio/Sink`, `Audio/Source` nodes and the `default` metadata's `default.audio.sink`/`source` | `sinks`, `sources`, their `active`, `channels`, `volume`, `muted` and `balance`; `card` is the node's `device.id` |
| `Stream/Output/Audio`, `Stream/Input/Audio` nodes | `apps`, minus the streams its field lists |
| ALSA and `bluez_card.*` devices and their profiles | `cards`; a BlueZ card's `mac` comes from its name, a profile's `codec` from its name, else its English description |
| Each ALSA and BlueZ device's `EnumRoute` | `cards[].ports`; `set_port` writes a `Route` on the profile device a sink or source of the active profile uses, so a port outside the active profile is dropped |
| Each ALSA and BlueZ device's active `Route` | `port`, and a hardware sink's or source's volume, mute and balance, Bluetooth microphones included. The node's `Props` only mirror the `Route`: they are read until it arrives, and a write before then is dropped. A virtual device has no `Route` and uses its node's `Props` |

The first push waits until PipeWire has reported every object and its volume, so a machine with no
audio hardware still gets one push of empty lists. An unreachable PipeWire is logged and `audio`
stays `nil` until it comes up. A lost connection pushes empty lists, the same as no hardware. Either
way the Supervisor retries after 1 s, doubling to every 30 s, and the first push after it reconnects
is a full one.

Writes are not optimistic: state changes when PipeWire reports the new value. An action with no
target (a stale `id`, or a volume write or mute toggle before that device's first report) is dropped
with a `debug` log. Volume, mute and balance writes go to the device named, which need not be the
default.

## How do I…

### Change volume on the scroll wheel, mute on middle click

A handler reads with `:get()`: it needs the value now, not a binding ([input](../guide/input.md)).
The default output is the `active` entry of `sinks`.

```lua
local function default_sink(audio)
    for _, sink in ipairs(audio and audio.sinks or {}) do
        if sink.active then
            return sink
        end
    end
end

rect {
    on_wheel = function(_, steps)
        local sink = default_sink(mantle.audio:get())
        if sink == nil or sink.volume == nil then
            return
        end
        mantle.audio:set_volume(sink.id, math.max(0, math.min(100, sink.volume + steps * 5)))
    end,
    on_click = function(_, which)
        local sink = default_sink(mantle.audio:get())
        if which == "middle" and sink then
            mantle.audio:toggle_mute(sink.id)
        end
    end,
    children = {
        text {
            content = mantle.audio:map(function(audio)
                local sink = default_sink(audio)
                if sink == nil or sink.volume == nil then
                    return "--"
                end
                return sink.muted and "muted" or string.format("%d%%", math.floor(sink.volume + 0.5))
            end, "--"),
        },
    },
}
```

### Show an OSD when volume changes

`on_change` writes [named state](../guide/signals.md#named-state) that a panel binds, and a
[`timer`](../guide/scripting.md#timer) hides the panel again:

```lua
local osd_text = state("osd_text", "")
local osd_visible = state("osd_visible", false)
local hide_timer

local function default_sink(audio)
    for _, sink in ipairs(audio.sinks) do
        if sink.active then
            return sink
        end
    end
end

mantle.audio:on_change(function(audio, previous)
    local sink, before = default_sink(audio), previous and default_sink(previous)
    if before == nil or sink == nil or sink.id ~= before.id or before.volume == nil or sink.volume == nil then
        return -- the first push, or a new default, is learned state, not a change
    end
    if sink.volume == before.volume and sink.muted == before.muted then
        return
    end
    osd_text:set(sink.muted and "Muted" or string.format("Volume %d%%", math.floor(sink.volume + 0.5)))
    osd_visible:set(true)
    if hide_timer then
        hide_timer:cancel()
    end
    hide_timer = timer(2000, function() osd_visible:set(false) end)
end)

local osd = panel {
    id = "osd",
    layer = "overlay",
    anchor = { bottom = true },
    margin = { bottom = 80 },
    visible = osd_visible,
    padding = 12,
    radius = 12,
    background = "#1E1E2ECC",
    child = text { content = osd_text, font_size = 16 },
}
return osd
```

See also: [Volume OSD](../cookbook/volume-osd.md) recipe.
