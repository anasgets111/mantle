-- Check only: `targets.lua` requires this under MANTLE_DEMO_MOCKS (`checkpoints.lua` copies it
-- in), so `mantle check` lays out every mock card with the take's data in it and every popup open.
local feeds = require("feeds")

local windows = {}
for k, w in ipairs(feeds.WINDOWS) do
    windows[k] = { id = w.id, app_id = w.app_id, title = w.title, focused = k == 2 }
end

local seeds = {
    mock_windows = { source = "hyprland", windows = windows },
    mock_notifications = { dnd = false, feed = { feeds.notification(feeds.SARAH) } },
    mock_media = feeds.player(61, "playing", mantle.config_dir),
    mock_tray = feeds.TRAY,
    mock_network = { wifi_enabled = true, connected = true, ssid = "Home", strength = 82 },
    mock_bluetooth = { enabled = true, connected_devices = { { name = "WH-1000XM5", battery = 80 } } },
    mock_brightness = { percent = 85 },
    mock_privacy = feeds.privacy(true, true, true),
    mock_idle = { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } },
    mock_updates = feeds.updates(),
    mock_polkit = { active = true, user = "you", message = "Authentication is required." },
    mock_lock = feeds.lock({ attempts = 1, error = "authentication failed" }),
    notif_drag = { id = "1", x = 40 },
    volume_osd = { volume = 60, muted = false },
    cpu_history = { 20, 40, 35, 60, 45 },
    -- Popups as the take shows them, with the code pane off.
    stage_full = true,
}

for _, name in ipairs { "control", "media", "picker", "updates", "overview" } do
    seeds[name .. "_open"] = true
end

-- A state takes one seed per name, so the sample replaces the module's own where it declares it.
-- ponytail: only modules loaded after `targets` are seeded; shell.lua's `launcher_open` stays closed.
local declare = state
function state(name, init, ...)
    if seeds[name] ~= nil then init = seeds[name] end
    return declare(name, init, ...)
end
