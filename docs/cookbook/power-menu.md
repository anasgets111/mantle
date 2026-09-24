# Power menu

A full-screen overlay with lock, suspend, log out, restart and power off. The last three ask for a
second click before they run, and a click outside the buttons closes the menu. A bar button opens
it, and so does `mantle toggle power_menu_open` from a keybind.

```lua,shot
local open = state("power_menu_open", false)
local pending = state("power_menu_pending", "") -- the action waiting for its second click

local function close()
    open:set(false)
    pending:set("")
end

local function log_out()
    local workspaces = mantle.workspaces:get()
    local compositor = workspaces and workspaces.compositor
    if compositor == "niri" then
        process.detach("niri", { "msg", "action", "quit", "--skip-confirmation" })
    elseif compositor == "hyprland" then
        process.detach("hyprctl", { "dispatch", "exit" })
    end
end

local ACTIONS = {
    { key = "lock", label = "Lock", glyph = "system-lock-screen-symbolic",
      run = function() mantle.lock:invoke("lock") end },
    { key = "suspend", label = "Suspend", glyph = "weather-clear-night-symbolic",
      run = function() process.detach("systemctl", { "suspend" }) end },
    { key = "logout", label = "Log out", glyph = "system-log-out-symbolic", confirm = true, run = log_out },
    { key = "reboot", label = "Restart", glyph = "system-reboot-symbolic", confirm = true,
      run = function() process.detach("systemctl", { "reboot" }) end },
    { key = "poweroff", label = "Power off", glyph = "system-shutdown-symbolic", confirm = true,
      run = function() process.detach("systemctl", { "poweroff" }) end },
}

local function action_button(action)
    local over = hover("power_" .. action.key)
    local armed = pending:map(function(key) return key == action.key end)
    return button {
        width = 120,
        height = 120,
        radius = 20,
        hover = over,
        background = computed({ over, armed }, function(hovered, is_armed)
            if is_armed then return "#f38ba8" end
            return hovered and "#45475a" or "#313244"
        end),
        animate = { background = 120, scale = { duration = 120, easing = "OutCubic" } },
        scale = over:map(function(hovered) return hovered and 1.05 or 1 end),
        on_click = function()
            if action.confirm and pending:get() ~= action.key then
                pending:set(action.key)
                return
            end
            close()
            action.run()
        end,
        children = {
            column {
                align_h = "Center",
                align_v = "Center",
                spacing = 10,
                children = {
                    icon {
                        name = action.glyph,
                        size = 36,
                        align_h = "Center",
                        foreground = armed:map(function(is_armed) return is_armed and "#1e1e2e" or "#cdd6f4" end),
                    },
                    text {
                        content = armed:map(function(is_armed) return is_armed and "Click again" or action.label end),
                        align_h = "Center",
                        foreground = armed:map(function(is_armed) return is_armed and "#1e1e2e" or "#cdd6f4" end),
                    },
                },
            },
        },
    }
end

local buttons = {}
for index, action in ipairs(ACTIONS) do
    buttons[index] = action_button(action)
end

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
            children = {
                rect { width = "Fill" },
                button {
                    align_v = "Center",
                    padding = 6,
                    radius = 6,
                    on_click = function() open:set(true) end,
                    children = { icon { name = "system-shutdown-symbolic", size = 16, foreground = "#f38ba8" } },
                },
            },
        },
    },
    panel {
        id = "power_menu",
        layer = "Overlay",
        monitor = "Active",
        anchor = { top = true, bottom = true, left = true, right = true },
        width = "Fill",
        height = "Fill",
        exclusive = "Ignore",
        visible = open,
        keyboard_interactivity = open:map(function(is_open) return is_open and "OnDemand" or "None" end),
        child = rect {
            width = "Fill",
            height = "Fill",
            background = "#11111bcc",
            opacity = 1, -- `from` needs the property set
            animate = { opacity = { duration = 150, from = 0 } },
            children = {
                button { width = "Fill", height = "Fill", on_click = close }, -- outside click
                row { align_h = "Center", align_v = "Center", spacing = 16, children = buttons },
            },
        },
    },
}
```

## How it works

- `process.detach` runs `systemctl` and the compositor's quit command as programs that outlive the shell, so a shutdown is never cut short by the shell exiting ([process.detach](../guide/processes.md#processdetach)).
- Log out branches on `mantle.workspaces`' `compositor` field ([workspaces](../capabilities/workspaces.md)).
- Lock goes through the lock capability, which needs a declared lock screen ([lock screen](lock-screen.md), [lock](../capabilities/lock.md)).
- One named state, `pending`, holds the armed action; a second click on the same button runs it ([named state](../guide/signals.md#named-state)).
- A full-size `button` under the row closes the menu; the row is declared after it, so it is on top ([close an overlay](../surfaces/panel.md#close-an-overlay-on-an-outside-click)).
- `hover` drives the tint and a `scale` tween, which does not re-lay out the row ([hover](../guide/input.md#hover), [animation](../guide/animation.md)).

## Variations

| Change | Edit |
| :--- | :--- |
| No confirmation | Remove `confirm = true` from the entries |
| Hibernate | Add `{ key = "hibernate", label = "Hibernate", glyph = "drive-harddisk-symbolic", confirm = true, run = function() process.detach("systemctl", { "hibernate" }) end }` |
| Hyprland with a Lua config | `process.detach("hyprctl", { "dispatch", "hl.dsp.exit()" })` |
| Vertical list | `column` instead of `row`, and `width = 240, height = 56` on each button with a `row` inside |
| Open from a keybind | Bind `mantle toggle power_menu_open` |
