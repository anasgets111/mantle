# App launcher

A centred search overlay for installed apps. Typing ranks desktop entries with fzf's scorer, the
arrow keys move the selection, Enter or a click launches, and Escape or a click outside closes it.
Open it from a compositor keybind with `mantle toggle launcher_open`.

```lua
local MAX_RESULTS = 50

local open = state("launcher_open", false)
local query = state("launcher_query", "")
local selected = state("launcher_selected", 1)
local results_scroll = scroll("launcher_results")

local function close()
    open:set(false)
    query:set("")
    selected:set(1)
end

-- Best score of the name, generic name and keywords; nil when none match.
local function score_of(entry, needle)
    local best, best_start = fuzzy(entry.name, needle)
    for _, field in ipairs({ entry.generic_name or "", table.concat(entry.keywords, " ") }) do
        local score, start = fuzzy(field, needle)
        if score and (best == nil or score > best) then
            best, best_start = score, start
        end
    end
    return best, best_start
end

local matches = computed({ mantle.applications, query }, function(applications, needle)
    local scored = {}
    for _, entry in ipairs(applications and applications.entries or {}) do
        local score, start = score_of(entry, needle)
        if score then
            scored[#scored + 1] = { entry = entry, score = score, start = start }
        end
    end
    table.sort(scored, function(left, right)
        if left.score ~= right.score then return left.score > right.score end
        if left.start ~= right.start then return left.start < right.start end
        return left.entry.id < right.entry.id -- unique, so the order is stable
    end)
    local out = {}
    for index = 1, math.min(#scored, MAX_RESULTS) do
        out[index] = scored[index].entry
    end
    return out
end)

local rows = computed({ matches, selected }, function(entries, current)
    local out = {}
    for index, entry in ipairs(entries) do
        out[index] = { entry = entry, selected = index == current }
    end
    return out
end)

local function launch(entry)
    if entry then
        mantle.applications:invoke("launch", entry.id)
    end
    close()
end

local function move(step)
    local count = #matches:get()
    if count == 0 then
        return
    end
    selected:set((selected:get() - 1 + step) % count + 1) -- wraps at both ends
    results_scroll:reveal(selected:get())
end

local search = rect {
    width = "Fill",
    padding = { left = 12, right = 12 },
    radius = 10,
    background = "#313244",
    children = {
        textfield {
            id = "search",
            width = "Fill",
            height = 40,
            font_size = 16,
            foreground = "#cdd6f4",
            placeholder = "Search apps",
            autofocus = true,
            on_change = function(text)
                query:set(text)
                selected:set(1)
                results_scroll:reveal(1)
            end,
            on_navigate = function(key)
                if key == "down" or key == "tab" then move(1) end
                if key == "up" or key == "backtab" then move(-1) end
            end,
            on_submit = function() launch(matches:get()[selected:get()]) end,
            on_cancel = function(cleared)
                if not cleared then close() end -- first Escape clears, second closes
            end,
        },
    },
}

local results = list {
    width = "Fill",
    max_height = 400,
    spacing = 2,
    scroll = results_scroll,
    source = rows,
    key = function(row_data) return row_data.entry.id end,
    itemfn = function(row_data)
        local entry = row_data.entry
        return button {
            width = "Fill",
            padding = 8,
            radius = 8,
            background = row_data.selected and "#45475a" or "#00000000",
            on_click = function() launch(entry) end,
            children = {
                row {
                    width = "Fill",
                    spacing = 12,
                    children = {
                        icon { name = entry.icon or "application-x-executable", size = 32, align_v = "Center" },
                        column {
                            width = "Fill",
                            align_v = "Center",
                            children = {
                                text { content = entry.name, width = "Fill", elide = "End", font_size = 14, foreground = "#cdd6f4" },
                                text {
                                    content = entry.comment or entry.generic_name or "",
                                    visible = (entry.comment or entry.generic_name) ~= nil,
                                    width = "Fill",
                                    elide = "End",
                                    font_size = 11,
                                    foreground = "#a6adc8",
                                },
                            },
                        },
                    },
                },
            },
        }
    end,
}

return {
    panel {
        id = "launcher",
        layer = "Overlay",
        monitor = "Active",
        anchor = { top = true, bottom = true, left = true, right = true },
        width = "Fill",
        height = "Fill",
        exclusive = "Ignore",
        visible = open,
        keyboard_interactivity = open:map(function(is_open) return is_open and "Exclusive" or "None" end),
        child = rect {
            width = "Fill",
            height = "Fill",
            background = "#11111b80",
            children = {
                -- Catches clicks outside the card.
                button { width = "Fill", height = "Fill", on_click = close },
                column {
                    width = 560,
                    align_h = "Center",
                    margin = { top = 160 },
                    padding = 12,
                    spacing = 8,
                    radius = 16,
                    background = "#1e1e2e",
                    border_width = 1,
                    border_color = "#45475a",
                    children = {
                        search,
                        results,
                        text {
                            content = "No matches",
                            visible = matches:map(function(entries) return #entries == 0 end),
                            align_h = "Center",
                            padding = 12,
                            foreground = "#6c7086",
                        },
                    },
                },
            },
        },
    },
}
```

Bind a key to `mantle toggle launcher_open`, for example `bind = SUPER, Space, exec, mantle toggle
launcher_open` on Hyprland or `Mod+Space { spawn "mantle" "toggle" "launcher_open"; }` on niri.

## How it works

- `applications.entries` holds every visible desktop entry; `launch` takes its `id` and runs it detached ([applications](../capabilities/applications.md)).
- `fuzzy` scores one candidate; ranking, the tiebreak and the cap stay in Lua ([fuzzy](../guide/scripting.md#fuzzy)).
- `computed` joins the capability with the query, and a second one marks the selected row ([derived signals](../guide/signals.md#derived-signals)).
- The `textfield` owns the typed text and reports it through `on_change`; `on_navigate` gets the arrow and Tab keys ([textfield](../nodes/textfield.md), [text fields](../guide/input.md#text-fields)).
- `scroll(name):reveal(index)` keeps the selected row in view inside the `max_height` list ([scroll](../guide/input.md#scroll), [list](../nodes/list.md)).
- `keyboard_interactivity` follows the same state as `visible`, so the field has the keyboard as soon as the panel maps ([keyboard focus](../surfaces/panel.md#keyboard-focus)).
- A full-size transparent `button` under the card closes it on an outside click ([close an overlay](../surfaces/panel.md#close-an-overlay-on-an-outside-click)).

## Variations

| Change | Edit |
| :--- | :--- |
| Pick up newly installed apps | `mantle.applications:invoke("refresh")` in an `action("launcher", ...)` that also opens it; bind `mantle call launcher` |
| Pointer focus on Hyprland | `"OnDemand"` instead of `"Exclusive"`, so other surfaces keep taking clicks ([panel gotchas](../surfaces/panel.md#gotchas)) |
| Fewer rows | `MAX_RESULTS = 8` and drop `max_height` |
| Debounce typing | Rank against `delay(query, 80)` instead of `query` ([debounce a search](../guide/signals.md#debounce-a-search)) |
| No dimmed backdrop | Remove the root `rect`'s `background` |
