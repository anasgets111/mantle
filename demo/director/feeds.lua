-- The mock data the director feeds the demo shell, in each capability's shape. `mantle check`
-- lays the stages out with the same data through `tools/mocks_sample.lua`.

local M = {}

-- `mantle.windows`' shape: a terminal and an editor, then the mock apps.
M.WINDOWS = {
    { id = "0xa1", app_id = "kitty",                title = "~/Work/mantle" },
    { id = "0xa2", app_id = "dev.zed.Zed",          title = "overview.lua" },
    { id = "0xa3", app_id = "org.telegram.desktop", title = "Telegram" },
    { id = "0xa4", app_id = "zen",                  title = "Zen Browser" },
}

M.TRAY = {
    items = {
        { id = "1", name = "Steam",    icon_name = "steam",                status = "active" },
        { id = "2", name = "Vesktop",  icon_name = "vesktop",              status = "active" },
        { id = "3", name = "Telegram", icon_name = "org.telegram.desktop", status = "active" },
    },
}

local PACKAGES = {
    { name = "linux",      old_version = "7.2.7.arch1-1", new_version = "7.2.8.arch1-1" },
    { name = "mesa",       old_version = "1:26.1.2-1",    new_version = "1:26.1.3-1" },
    { name = "mantle-git", old_version = "r1830",         new_version = "r1842" },
}

-- The list before an install, package `step` of it installing, or nothing left past the last.
function M.updates(step)
    if step == nil then return { packages = PACKAGES, installing = false } end
    if step > #PACKAGES then return { packages = {}, installing = false } end
    return {
        packages = {},
        installing = true,
        install_current_step = step,
        install_total_steps = #PACKAGES,
        install_current_package = PACKAGES[step].name,
    }
end

-- One track `at` seconds in, its cover from the demo directory `dir`.
function M.player(at, play_state, dir)
    return {
        players = {
            {
                id = "spotify",
                identity = "Spotify",
                title = "Night Signals",
                artist = "Low Orbit",
                album = "Chevrons",
                album_art_path = dir .. "/covers/night-signals.svg",
                length = 214 * 1000000,
                position = at * 1000000,
                play_state = play_state,
            },
        },
    }
end

M.SARAH = {
    id = 1,
    from = "Sarah",
    body = {
        { kind = "text", text = "Still on for tonight? 8 pm at " },
        { kind = "text", text = "Luigi's",                       href = "https://maps.example.org/luigis" },
    },
    placeholder = "Reply to Sarah",
    read = "Mark as read",
}

-- A Telegram message from `n.from` in `mantle.notifications`' entry shape.
function M.notification(n)
    return {
        id = n.id,
        app_name = "Telegram",
        app_icon = "org.telegram.desktop",
        desktop_entry = "org.telegram.desktop",
        summary = n.from,
        body = n.body or { { kind = "text", text = n.text } },
        actions = { { key = "read", label = n.read } },
        has_default_action = true,
        has_reply = true,
        reply_placeholder = n.placeholder,
        urgency = "normal",
        expired = false,
        transient = false,
        timestamp = 0,
    }
end

function M.privacy(camera, mic, screen)
    local function users(on) return on and { { app_name = "Meet" } } or {} end
    return { camera_users = users(camera), microphone_users = users(mic), screencast_users = users(screen) }
end

function M.lock(fields)
    local out = { active = true, attempts = 0, error = "", unlocking = false }
    for k, v in pairs(fields or {}) do
        out[k] = v
    end
    return out
end

return M
