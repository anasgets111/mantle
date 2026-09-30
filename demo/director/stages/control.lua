-- Quick settings. Demo: the director feeds `mock_network`, `mock_bluetooth` and `mock_brightness` in
-- the shapes of `mantle.network`, `mantle.bluetooth` and `mantle.brightness`, so the take touches no
-- radio of yours. The switches are real: a `persistent_table` keeps them on disk across restarts.
local theme = require("theme")
local target = require("targets")

local network = state("mock_network", { wifi_enabled = false, connected = false, strength = 0 })
local bluetooth = state("mock_bluetooth", { enabled = false, connected_devices = {} })
local brightness = state("mock_brightness", { percent = 40 })
local open = state("control_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

local settings = persistent_table {
    path = mantle.config_dir .. "/state",
    name = "settings.json",
    defaults = { dnd = false, night_light = false },
}

-- `mantle call setting dnd` flips one switch, as a click on its tile does.
local function flip(key)
    settings:set(key, not settings[key]:get())
    return key
end
action("setting", flip)

local function wifi_icon(n)
    if not n.wifi_enabled then return "network-wireless-offline-symbolic" end
    if not n.connected then return "network-wireless-acquiring-symbolic" end
    local level = n.strength > 75 and "excellent" or n.strength > 50 and "good" or "ok"
    return "network-wireless-signal-" .. level .. "-symbolic"
end

local tiles = {
    {
        key = "wifi",
        glyph = network:map(wifi_icon),
        title = "Wi-Fi",
        on = network:map(function(n) return n.wifi_enabled end),
        detail = network:map(function(n)
            if not n.wifi_enabled then return "Off" end
            return n.connected and n.ssid or "Searching…"
        end),
    },
    {
        key = "bluetooth",
        glyph = "bluetooth-active-symbolic",
        title = "Bluetooth",
        on = bluetooth:map(function(b) return b.enabled end),
        detail = bluetooth:map(function(b)
            local device = b.connected_devices[1]
            if not b.enabled then return "Off" end
            return device and device.name or "No devices"
        end),
    },
    {
        key = "dnd",
        persisted = true,
        glyph = "notifications-disabled-symbolic",
        title = "Do not disturb",
        on = settings.dnd,
        detail = settings.dnd:map(function(on) return on and "Silenced" or "Off" end),
    },
    {
        key = "night_light",
        persisted = true,
        glyph = "night-light-symbolic",
        title = "Night light",
        on = settings.night_light,
        detail = settings.night_light:map(function(on) return on and "Until 07:00" or "Off" end),
    },
}

local function tile(spec)
    local on = spec.on:map(function(v) return v == true end)
    local function pick(a, b)
        return computed({ on, a, b }, function(v, yes, no) return v and yes or no end)
    end
    return row {
        geometry = target("control:" .. spec.key),
        width = "Fill",
        height = 96,
        padding = { left = 20, right = 20 },
        radius = 24,
        background = pick(theme.accent, theme.surface),
        scale = on:map(function(v) return v and 1 or 0.97 end),
        animate = { background = 220, scale = { spring = { stiffness = 380, damping = 20 } } },
        -- The radios are mocks: a click on them must not reach your real Wi-Fi or Bluetooth.
        on_click = spec.persisted and function() flip(spec.key) end or nil,
        spacing = 16,
        children = {
            icon { name = spec.glyph, size = 30, align_v = "Center", foreground = pick(theme.crust, theme.text) },
            column {
                align_v = "Center",
                spacing = 2,
                children = {
                    text { content = spec.title, font_size = 20, font_weight = 700, foreground = pick(theme.crust, theme.text) },
                    text { content = spec.detail, font_size = 16, foreground = pick(theme.base, theme.subtext) },
                },
            },
        },
    }
end

local function slider(glyph, value)
    return row {
        width = "Fill",
        height = 48,
        spacing = 16,
        children = {
            icon { name = glyph, size = 26, align_v = "Center", foreground = theme.text },
            rect {
                width = "Fill",
                height = 10,
                radius = 5,
                align_v = "Center",
                background = theme.surface,
                children = {
                    rect {
                        height = "Fill",
                        radius = 5,
                        background = theme.accent,
                        width = value:map(function(v) return math.floor(v) .. "%" end),
                        animate = { width = { duration = 400, easing = "OutCubic" } },
                    },
                },
            },
        },
    }
end

local WIDTH = 620

local panel_node = panel {
    id = "control",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = mantle.screens:map(function(screens)
        local width = screens[1] and screens[1].width or 1920
        return { top = 24, left = math.floor(width * 0.56) - WIDTH - 40 }
    end),
    visible = mapped,
    child = column {
        width = WIDTH,
        padding = 24,
        spacing = 16,
        radius = 30,
        background = theme.fade("crust", "e6"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = open:map(function(on) return on and 1 or 0 end),
        translate = open:map(function(on) return { y = on and 0 or -20 } end),
        animate = {
            opacity = { duration = 200, from = 0 },
            translate = { duration = 360, easing = "OutBack", from = { y = -20 } },
        },
        children = {
            row { width = "Fill", spacing = 14, children = { tile(tiles[1]), tile(tiles[2]) } },
            row { width = "Fill", spacing = 14, children = { tile(tiles[3]), tile(tiles[4]) } },
            slider("display-brightness-symbolic", brightness:map(function(b) return b.percent end)),
            slider("audio-volume-high-symbolic", mantle.audio:map(function(a)
                return a and a.volume and math.min(a.volume, 1) * 100 or 0
            end)),
        },
    },
}

-- The bar's radio icons, lit while each radio is on.
local status = row {
    spacing = 12,
    align_v = "Center",
    margin = { right = 18 },
    children = {
        icon {
            name = network:map(wifi_icon),
            size = 24,
            align_v = "Center",
            foreground = computed({ network, theme.text, theme.muted }, function(n, lit, dim)
                return n.connected and lit or dim
            end),
        },
        icon {
            name = "bluetooth-active-symbolic",
            size = 24,
            align_v = "Center",
            foreground = computed({ bluetooth, theme.accent, theme.muted }, function(b, lit, dim)
                return #b.connected_devices > 0 and lit or dim
            end),
        },
    },
}

return { panel = panel_node, status = status }
