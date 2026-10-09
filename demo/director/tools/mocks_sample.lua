-- Check only: `targets.lua` requires this under MANTLE_DEMO_MOCKS (`checkpoints.lua` copies it in), so `mantle check` lays out every
-- mock card with data in it and every popup open, as the take's feeds leave them.
-- Mock shapes follow the `state("mock_*", default)` of the module that reads each.
local PACKAGES = {
    { name = "linux", old_version = "7.2.7.arch1-1", new_version = "7.2.8.arch1-1" },
    { name = "mesa",  old_version = "1:26.1.2-1",    new_version = "1:26.1.3-1" },
}

local seeds = {
    mock_windows = {
        source = "hyprland",
        windows = {
            { id = "0xa1", app_id = "kitty",       title = "~/Work/mantle", focused = false },
            { id = "0xa2", app_id = "dev.zed.Zed", title = "overview.lua",  focused = true },
        },
    },
    mock_notifications = {
        dnd = false,
        feed = {
            {
                id = 1,
                app_name = "Telegram",
                app_icon = "org.telegram.desktop",
                summary = "Ahmed",
                body = {
                    { kind = "text", text = "v0.9 is out: " },
                    { kind = "text", text = "release notes", href = "https://example.com/notes" },
                },
                actions = { { key = "read", label = "Mark as read" } },
                has_default_action = true,
                has_reply = true,
                reply_placeholder = "Reply",
                urgency = "normal",
                expired = false,
                transient = false,
                timestamp = 0,
            },
        },
    },
    mock_media = {
        players = {
            {
                id = "spotify",
                identity = "Spotify",
                title = "Signals",
                artist = "Night",
                album = "Reload",
                album_art_path = "",
                length = 200000000,
                position = 61000000,
                play_state = "playing",
            },
        },
    },
    mock_tray = {
        items = { { id = "1", name = "Steam", icon_name = "steam", status = "active" } },
    },
    mock_network = { wifi_enabled = true, connected = true, ssid = "Home", strength = 82 },
    mock_bluetooth = { enabled = true, connected_devices = { { name = "WH-1000XM5", battery = 80 } } },
    mock_brightness = { percent = 85 },
    mock_privacy = {
        camera_users = { { app_name = "Meet" } },
        microphone_users = { { app_name = "Meet" } },
        screencast_users = { { app_name = "OBS" } },
    },
    mock_idle = { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } },
    mock_updates = { packages = PACKAGES, installing = false },
    mock_polkit = { active = true, user = "you", message = "Authentication is required." },
    mock_lock = { active = true, attempts = 1, error = "authentication failed", unlocking = false },
    notif_drag = { id = "1", x = 40 },
    volume_osd = { volume = 60, muted = false },
    cpu_history = { 20, 40, 35, 60, 45 },
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
