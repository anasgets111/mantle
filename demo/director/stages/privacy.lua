-- Demo: fed `mock_privacy` in `mantle.privacy`'s shape, so no real camera or microphone opens.
local theme = require("theme")
local target = require("targets")
local privacy = state("mock_privacy", { camera_users = {}, microphone_users = {}, screencast_users = {} })

local KINDS = {
    { field = "camera_users",     icon = "camera-web-symbolic" },
    { field = "microphone_users", icon = "audio-input-microphone-symbolic" },
    { field = "screencast_users", icon = "screen-shared-symbolic" },
}

local function in_use(p)
    return #p.camera_users + #p.microphone_users + #p.screencast_users > 0
end

return rect {
    geometry = target("privacy"),
    visible = privacy:map(in_use),
    margin = { right = 14 },
    height = 40,
    align_v = "center",
    padding = { left = 16, right = 18 },
    radius = 20,
    background = theme.danger,
    clip = "box",
    scale = 1,
    animate = {
        scale = { duration = 320, easing = "out_back", from = 0.5 },
        width = { duration = 260, easing = "out_cubic" },
    },
    children = privacy:map(function(p)
        local children, names, seen = {}, {}, {}
        for _, kind in ipairs(KINDS) do
            if #p[kind.field] > 0 then
                children[#children + 1] = icon {
                    name = kind.icon,
                    size = 24,
                    align_v = "center",
                    foreground = theme.crust,
                }
            end
            for _, user in ipairs(p[kind.field]) do
                if not seen[user.app_name] then
                    seen[user.app_name] = true
                    names[#names + 1] = user.app_name
                end
            end
        end
        children[#children + 1] = text {
            content = table.concat(names, ", "),
            margin = { left = 4 },
            align_v = "center",
            font_size = 20,
            font_weight = 700,
            foreground = theme.crust,
        }
        return { row { height = "fill", spacing = 8, children = children } }
    end),
}
