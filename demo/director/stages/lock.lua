-- Demo: a lock screen's look, fed `mock_lock` in `mantle.lock`'s shape and the typed length in
-- `lock_typed` with `mantle set`, so the take needs no password. A real one is a `lock` surface:
-- ext-session-lock holds the screen and PAM checks what its secure field hands `mantle.lock`.
local theme = require("theme")

local lock = state("mock_lock", { active = false, attempts = 0, error = "", unlocking = false })
local typed = state("lock_typed", 0)
local wallpaper = state("wallpaper", "")
local up = lock:map(function(l) return l.active and not l.unlocking end)
local mapped = computed({ up, delay(up, 700) }, function(now, was) return now or was end)

local function dots(count)
    local out = {}
    for k = 1, count do
        out[k] = rect {
            id = "dot:" .. k,
            width = 16,
            height = 16,
            radius = 8,
            align_v = "center",
            background = theme.text,
            scale = 1,
            animate = { scale = { duration = 220, easing = "out_back", from = 0 } },
        }
    end
    return out
end

-- A rejected password shakes the field: each attempt is a new id, so the keyframes play once.
local field = lock:map(function(l)
    local failed = l.error ~= ""
    return { rect {
        id = "field:" .. l.attempts,
        width = 420,
        height = 64,
        radius = 32,
        align_h = "center",
        padding = { left = 26, right = 26 },
        background = theme.fade("crust", "cc"),
        border_width = 2,
        border_color = failed and theme.danger or theme.overlay,
        translate = { x = 0, y = 0 },
        animate = failed and {
            translate = {
                duration = 420,
                keyframes = { { x = 0, y = 0 }, { x = -18, y = 0 }, { x = 16, y = 0 }, { x = -10, y = 0 }, { x = 0, y = 0 } },
            },
        } or nil,
        children = { row { height = "fill", align_h = "center", spacing = 12, children = typed:map(dots) } },
    } }
end)

return panel {
    id = "lock",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    visible = mapped,
    child = rect {
        width = "fill",
        height = "fill",
        background = theme.crust,
        opacity = up:map(function(on) return on and 1 or 0 end),
        scale = up:map(function(on) return on and 1 or 1.04 end),
        animate = {
            opacity = { duration = 500, easing = "out_cubic", from = 0 },
            scale = { duration = 700, easing = "out_cubic", from = 1.04 },
        },
        children = {
            image {
                source = wallpaper:map(function(name)
                    return name ~= "" and mantle.config_dir .. "/wallpapers/thumbs/" .. name or ""
                end),
                source_blur = 6,
                fit = "cover",
                width = "fill",
                height = "fill",
            },
            rect { width = "fill", height = "fill", background = theme.fade("crust", "99") },
            column {
                align_h = "center",
                align_v = "center",
                spacing = 22,
                children = {
                    text {
                        content = mantle.system:map(function(s) return os.date("%H:%M", s and s.time) end),
                        align_h = "center",
                        font_size = 200,
                        font_weight = 200,
                        foreground = theme.text,
                    },
                    text {
                        content = mantle.system:map(function(s) return os.date("%A, %d %B", s and s.time) end),
                        align_h = "center",
                        font_size = 36,
                        foreground = theme.subtext,
                        margin = { bottom = 40 },
                    },
                    rect {
                        width = 96,
                        height = 96,
                        radius = 48,
                        align_h = "center",
                        background = theme.accent,
                        children = {
                            icon {
                                name = "avatar-default-symbolic",
                                size = 52,
                                align_h = "center",
                                align_v = "center",
                                foreground = theme.crust,
                            },
                        },
                    },
                    rect { align_h = "center", children = field },
                    text {
                        content = lock:map(function(l)
                            return l.error ~= "" and l.error or "Type your password to unlock"
                        end),
                        align_h = "center",
                        font_size = 20,
                        foreground = computed({ lock, theme.danger, theme.muted }, function(l, failed, calm)
                            return l.error ~= "" and failed or calm
                        end),
                    },
                },
            },
        },
    },
}
