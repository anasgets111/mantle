```lua
list {
    source = mantle.audio:map(function(audio)
        return audio and audio.sinks or {}
    end),
    key = function(sink) return tostring(sink.id) end,
    itemfn = function(sink)
        return button {
            padding = 6,
            background = sink.active and "#45475A" or "#1E1E2E",
            on_click = function() mantle.audio:invoke("set_default_sink", sink.id) end,
            children = { text { content = sink.name } },
        }
    end,
}
```

<!-- reference -->

## Backend

| Source | Feeds |
| :--- | :--- |
| Sinks, sources, default routing | `audio` devices, master and input volume/mute, balance |
| `Stream/Output/Audio`, `Stream/Input/Audio` with an `application.process.id` | `audio.apps` per-app volume/mute by node ID; excludes pid-less (portal) streams, notification sounds, peak meters and monitor captures |

`audio` and [`privacy`](privacy.md#backend) share one PipeWire thread. An unreachable PipeWire is
logged and the thread exits; nothing reconnects.

## How do I…

### Change volume on the scroll wheel, mute on middle click

`:get()` inside a handler is the right read: it wants the value now, not a binding. Input handlers
are covered in [input](../guide/input.md).

```lua
button {
    on_wheel = function(_, steps)
        local audio = mantle.audio:get()
        if audio == nil or audio.volume == nil then
            return
        end
        mantle.audio:invoke("set_volume", math.max(0, math.min(1, audio.volume + steps * 0.05)))
    end,
    on_click = function(_, which)
        if which == "middle" then
            mantle.audio:invoke("toggle_mute")
        end
    end,
    children = {
        text {
            content = mantle.audio:map(function(audio)
                if audio == nil or audio.volume == nil then
                    return "--"
                end
                return audio.muted and "muted" or string.format("%d%%", math.floor(audio.volume * 100 + 0.5))
            end),
        },
    },
}
```

### Show an OSD when volume changes

`on_change` reacts to a push rather than drawing it, so it writes
[named state](../guide/signals.md#named-state) that a panel binds; [`timer`](../guide/scripting.md#timer)
hides it again:

```lua
local osd_text = state("osd_text", "")
local osd_visible = state("osd_visible", false)
local hide_timer

mantle.audio:on_change(function(audio, previous)
    if previous == nil or audio.volume == nil then
        return -- the first push is learned state, not a change
    end
    if audio.volume == previous.volume and audio.muted == previous.muted then
        return
    end
    osd_text:set(audio.muted and "Muted" or string.format("Volume %d%%", math.floor(audio.volume * 100 + 0.5)))
    osd_visible:set(true)
    if hide_timer then
        hide_timer:cancel()
    end
    hide_timer = timer(2000, function() osd_visible:set(false) end)
end)

local osd = panel {
    id = "osd",
    layer = "Overlay",
    anchor = { bottom = true },
    margin = { bottom = 80 },
    visible = osd_visible,
    padding = 12,
    radius = 12,
    background = "#1E1E2ECC",
    child = text { content = osd_text, font_size = 16 },
}
```

See also: [Volume OSD](../cookbook/volume-osd.md) recipe.
