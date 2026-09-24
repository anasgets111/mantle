local function note(fields)
    local base = { body = {}, actions = {}, expired = false, has_default_action = false, has_reply = false, timestamp = 0, transient = false, urgency = "normal" }
    for key, value in pairs(fields) do base[key] = value end
    return base
end
local saved = note { id = 1, app_name = "Photos", summary = "Wallpaper saved", image_path = "/tmp/c.jpg", urgency = "critical" }
local download = note { id = 2, app_name = "Downloads", app_icon = "folder", summary = "Download finished",
    body = { { kind = "text", text = "report.pdf " }, { kind = "text", text = "open", href = "https://example.org" } },
    actions = { { key = "open", label = "Open" }, { key = "show", label = "Show in folder" } }, has_default_action = true }

fakes = { notifications = { dnd = false, feed = { download, saved } } }
