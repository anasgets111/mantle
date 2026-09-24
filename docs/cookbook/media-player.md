# Media player

A now-playing pill in the bar for any MPRIS player (Spotify, mpv, a browser tab). Click it for a
card with cover art, title, artist, a seekable progress bar and previous, play/pause and next
buttons. It prefers whichever player is playing and hides when none runs.

```lua,shot
local card_open = state("media_open", false)
local card_anchor = state("media_anchor", { x = 0, y = 0, width = 1, height = 1 })
-- Where `position` was last reported, in `mantle.system.monotonic` seconds.
local position_mark = state("media_position_mark", { key = "", at = 0 })

-- The playing player, else the longest-running one.
local function pick(mpris)
    local players = mpris and mpris.players or {}
    for _, candidate in ipairs(players) do
        if candidate.play_state == "Playing" then
            return candidate
        end
    end
    return players[1]
end

local player = mantle.mpris:map(pick)

local function report_key(current)
    return current.id .. ":" .. current.position_updated_at
end

-- Positions are not polled: stamp each new report so the bar can add the time since.
mantle.mpris:on_change(function(mpris)
    local current = pick(mpris)
    local system = mantle.system:get()
    if current and system then
        local key = report_key(current)
        if key ~= position_mark:get().key then
            position_mark:set({ key = key, at = system.monotonic })
        end
    end
end)

-- Live position in microseconds, or -1 when unknown.
local position = computed({ player, position_mark, mantle.system }, function(current, mark, system)
    if current == nil or current.position < 0 then
        return -1
    end
    local elapsed = 0
    -- An unstamped report, such as one before the first clock push, adds nothing.
    if current.play_state == "Playing" and system and mark.key == report_key(current) then
        elapsed = (system.monotonic - mark.at) * 1000000
    end
    local now = current.position + elapsed
    return current.length > 0 and math.min(now, current.length) or now
end)

local function clock(microseconds)
    local seconds = math.max(0, microseconds // 1000000)
    return string.format("%d:%02d", seconds // 60, seconds % 60)
end

local function control(command)
    local current = player:get()
    if current then
        mantle.mpris:invoke("control", current.id, command)
    end
end

local function control_button(glyph, command, size)
    return button {
        align_v = "Center",
        padding = 8,
        radius = 20,
        background = command == "play_pause" and "#89b4fa" or "#313244",
        on_click = function() control(command) end,
        children = { icon { name = glyph, size = size, foreground = command == "play_pause" and "#1e1e2e" or "#cdd6f4" } },
    }
end

local play_glyph = player:map(function(current)
    return current and current.play_state == "Playing" and "media-playback-pause-symbolic" or "media-playback-start-symbolic"
end)

local pill = button {
    align_v = "Center",
    padding = { left = 10, right = 10, top = 4, bottom = 4 },
    radius = 12,
    background = "#313244",
    visible = player:map(function(current) return current ~= nil end),
    on_click = function(rect, which)
        if which == "middle" then
            control("play_pause")
        else
            card_anchor:set(rect)
            card_open:set(not card_open:get())
        end
    end,
    on_wheel = function(_, steps) control(steps > 0 and "previous" or "next") end,
    children = {
        row {
            spacing = 6,
            children = {
                icon { name = play_glyph, size = 14, foreground = "#cdd6f4", align_v = "Center" },
                text {
                    max_width = 240,
                    elide = "End",
                    foreground = "#cdd6f4",
                    content = player:map(function(current)
                        if current == nil or current.title == "" then return "" end
                        return current.artist ~= "" and current.artist .. " — " .. current.title or current.title
                    end),
                },
            },
        },
    },
}

local progress = button {
    width = "Fill",
    height = 6,
    radius = 3,
    clip = "Rounded",
    background = "#45475a",
    -- Seek on release, to where the pointer let go.
    on_drag = function(rect, pointer, phase)
        local current = player:get()
        if phase == "end" and current and current.length > 0 then
            local fraction = math.max(0, math.min(1, pointer.x / rect.width))
            mantle.mpris:invoke("seek", current.id, math.floor(fraction * current.length))
        end
    end,
    children = {
        rect {
            height = "Fill",
            background = "#89b4fa",
            width = computed({ player, position }, function(current, now)
                if current == nil or current.length <= 0 or now < 0 then return "0%" end
                return string.format("%.1f%%", now / current.length * 100)
            end),
        },
    },
}

local card = column {
    width = 320,
    padding = 16,
    spacing = 12,
    children = {
        row {
            width = "Fill",
            spacing = 12,
            children = {
                rect {
                    width = 64,
                    height = 64,
                    radius = 8,
                    clip = "Rounded",
                    background = "#313244",
                    children = {
                        image {
                            width = "Fill",
                            height = "Fill",
                            fit = "cover",
                            source = player:map(function(current) return current and current.album_art_path or "" end),
                        },
                    },
                },
                column {
                    width = "Fill",
                    align_v = "Center",
                    spacing = 2,
                    children = {
                        text {
                            content = player:map(function(current) return current and current.title or "" end),
                            width = "Fill", elide = "End", font_size = 14, foreground = "#cdd6f4",
                        },
                        text {
                            content = player:map(function(current) return current and current.artist or "" end),
                            width = "Fill", elide = "End", foreground = "#a6adc8",
                        },
                        text {
                            content = player:map(function(current) return current and current.identity or "" end),
                            width = "Fill", elide = "End", font_size = 11, foreground = "#6c7086",
                        },
                    },
                },
            },
        },
        progress,
        row {
            width = "Fill",
            children = {
                text { content = position:map(function(now) return now < 0 and "" or clock(now) end), font_size = 11, foreground = "#a6adc8" },
                rect { width = "Fill" },
                text {
                    content = player:map(function(current) return current and current.length > 0 and clock(current.length) or "" end),
                    font_size = 11,
                    foreground = "#a6adc8",
                },
            },
        },
        row {
            align_h = "Center",
            spacing = 12,
            width = "Fill",
            children = {
                control_button("media-skip-backward-symbolic", "previous", 16),
                control_button(play_glyph, "play_pause", 20),
                control_button("media-skip-forward-symbolic", "next", 16),
            },
        },
    },
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        width = "Fill",
        height = 32,
        exclusive = true,
        child = row {
            width = "Fill",
            height = "Fill",
            padding = { left = 8, right = 8 },
            background = "#1e1e2e",
            children = { rect { width = "Fill" }, pill, rect { width = "Fill" } },
        },
    },
    popup {
        id = "media_card",
        parent = "bar",
        anchor_rect = card_anchor,
        anchor = "Bottom",
        gravity = "Bottom",
        offset = { y = 6 },
        visible = computed({ card_open, player }, function(open, current) return open and current ~= nil end),
        on_dismiss = function() card_open:set(false) end,
        radius = 14,
        background = "#1e1e2e",
        border_width = 1,
        border_color = "#45475a",
        child = card,
    },
}
```

## How it works

- `players` is longest-running first; the map prefers one that is playing ([mpris](../capabilities/mpris.md)).
- `position` is a snapshot, not polled. `on_change` stamps each new report with `mantle.system.monotonic`, and a `computed` adds the seconds since ([system](../capabilities/system.md), [derived signals](../guide/signals.md#derived-signals)).
- The fill is a `"NN%"` width in a rounded, clipped track; `on_drag` on the track seeks on release ([pointer](../guide/input.md#pointer), [clip](../guide/paint.md#clip)).
- `album_art_path` is a local file or `""`, and an `image` with `source = ""` draws nothing over the placeholder `rect` ([image](../nodes/image.md)).
- The card is a grabbing [popup](../surfaces/popup.md) anchored to the pill's click rect; it also closes when the last player quits.

## Variations

| Change | Edit |
| :--- | :--- |
| Always the first player | `pick` returns `mpris and mpris.players[1]` |
| Seek 10 s back and forward | Two more buttons whose `on_click` invokes `seek_relative` with the player's `id` and `-10000000` or `10000000` |
| Show the app icon | `icon { name = current.desktop_entry }` from the player's `desktop_entry` |
| Hide browsers | Skip players whose `desktop_entry` is `"firefox"` or `"chromium"` in `pick` |
| No popup, controls in the bar | Put the three `control_button`s in the bar row and drop the popup |
