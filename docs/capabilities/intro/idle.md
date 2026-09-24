`mantle.idle` reads like the others (`:get`, `:map`, `:on_change`) but has no `:invoke`. Its
thresholds take Lua callbacks, which cannot cross to the Supervisor, so it has [methods](#methods)
instead. Any method, `:get` included, starts it.

```lua
local dimmed = state("dimmed", false)

mantle.idle:register_threshold(300, function()
    dimmed:set(true)
end, function()
    dimmed:set(false)
end)
```

<!-- reference -->

## Methods

| Method | Contract |
| :--- | :--- |
| `:register_threshold(seconds, on_idle, on_resume)` | Runs `on_idle` after `seconds` without input and `on_resume` when input returns. Returns an integer handle. If this idle period already passed `seconds` for another registration, `on_idle` runs at once |
| `:cancel_threshold(handle)` | Drops one registration. An unknown or cancelled handle is a no-op |
| `:inhibit(reason)` | Takes one hold on a logind `idle` block inhibitor. Counted: two calls need two releases |
| `:release_inhibit()` | Releases one hold; with none held, a no-op |

| Event | Thresholds | Inhibit holds |
| :--- | :--- | :--- |
| In-place reload | Dropped before the config evaluates again; top-level `register_threshold` calls re-register. The same `seconds` keeps its timer and does not re-run `on_idle` this idle period | Kept |
| Renderer replacement | Dropped | Dropped |

## Backend

| Part | Behaviour |
| :--- | :--- |
| Thresholds | `ext_idle_notifier_v1` on the Supervisor's own Wayland connection. Missing protocol, or setup over 5 s: thresholds never fire, logged once |
| Inhibit | Every hold, from any generation, shares one logind `Inhibit("idle", "block")` fd, closed when the last hold goes |
| ScreenSaver | Hosts `org.freedesktop.ScreenSaver` when the name is free. A browser's video hold arrives here, directly or through xdg-desktop-portal, and takes the same fd. A client that leaves the bus loses its holds |
| Gate | Mantle, not logind, acts on idle, so it honours inhibitors itself. While logind's `BlockInhibited` names `idle`, idled thresholds get `on_resume` and none fire; on release, ones still idle get `on_idle` again |
| Compositor holds | A Wayland idle inhibitor shows when the shortest threshold's input-only twin fires and the normal notification does not, so it needs a registered threshold and an idle seat. It sets `inhibited` and adds one holder with an empty `who` |

## How do I…

### Keep the screen awake (caffeine)

The hold survives reloads, so [named state](../guide/signals.md#named-state) records it:

```lua
local caffeine = state("caffeine", false)

button {
    on_click = function()
        if caffeine:get() then
            mantle.idle:release_inhibit()
        else
            mantle.idle:inhibit("caffeine")
        end
        caffeine:set(not caffeine:get())
    end,
    children = {
        text {
            content = computed({ caffeine, mantle.idle }, function(held, idle)
                if held then
                    return "awake (held)"
                elseif idle and idle.inhibited then
                    return "awake (another app)"
                end
                return "idle allowed"
            end),
        },
    },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `register_threshold` inside `on_change`, a timer or a click handler | Each call adds a registration that lives until the next reload. Register once at top level, or keep the handle and `cancel_threshold` it |
| An `inhibit` hold that never ends | Holds survive reloads and are counted. Record the hold in a `state` and release once per `inhibit` |
| A threshold never fires while a video plays | Something holds idle off and `inhibited` is `true`. Draw `inhibitors` to show who |

See also: [Lock screen](../cookbook/lock-screen.md) recipe, which locks after an idle threshold.
