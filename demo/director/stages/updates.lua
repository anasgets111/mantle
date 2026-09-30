-- Demo: the director feeds `mock_updates` in `mantle.updates`' shape with `mantle set`, so the take
-- installs nothing. A real shell reads `mantle.updates` and calls `mantle.updates:install()`, which
-- asks polkit, and so the shell's own agent, for the password.
local theme = require("theme")
local target = require("targets")

local updates = state("mock_updates", { count = 0, packages = {}, installing = false })
local open = state("updates_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

local badge = rect {
    geometry = target("updates"),
    visible = updates:map(function(u) return u.count > 0 or u.installing end),
    margin = { right = 14 },
    height = 40,
    align_v = "Center",
    padding = { left = 14, right = 18 },
    radius = 20,
    background = theme.surface,
    scale = 1,
    animate = { scale = { duration = 320, easing = "OutBack", from = 0.5 } },
    children = {
        row {
            height = "Fill",
            spacing = 10,
            children = {
                icon {
                    name = "software-update-available-symbolic",
                    size = 22,
                    align_v = "Center",
                    foreground = theme.accent,
                },
                text {
                    content = updates:map(function(u)
                        if u.installing then return string.format("%d/%d", u.install_current_step, u.install_total_steps) end
                        return tostring(u.count)
                    end),
                    align_v = "Center",
                    font_size = 18,
                    font_weight = 700,
                    foreground = theme.text,
                },
            },
        },
    },
}

local function package_row(p)
    return row {
        width = "Fill",
        spacing = 12,
        children = {
            text { content = p.name, font_size = 20, foreground = theme.text },
            rect { width = "Fill" },
            text { content = p.old_version, font_size = 17, foreground = theme.muted },
            text { content = "→", font_size = 17, foreground = theme.muted },
            text { content = p.new_version, font_size = 17, foreground = theme.success },
        },
    }
end

local WIDTH = 620

local popover = panel {
    id = "updates",
    layer = "Overlay",
    anchor = { top = true, left = true },
    margin = mantle.screens:map(function(screens)
        local width = screens[1] and screens[1].width or 1920
        return { top = 24, left = math.floor(width * 0.56) - WIDTH - 40 }
    end),
    visible = mapped,
    child = column {
        width = WIDTH,
        padding = 26,
        spacing = 16,
        radius = 28,
        background = theme.fade("crust", "e6"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = open:map(function(on) return on and 1 or 0 end),
        translate = open:map(function(on) return { y = on and 0 or -20 } end),
        animate = {
            opacity = { duration = 200, from = 0 },
            translate = { duration = 360, easing = "OutBack", from = { y = -20 } },
        },
        children = updates:map(function(u)
            local done = not u.installing and u.count == 0
            local out = {
                text {
                    content = u.installing and ("Installing " .. u.install_current_package)
                        or done and "Up to date"
                        or string.format("%d updates", u.count),
                    font_size = 28,
                    font_weight = 800,
                    foreground = done and theme.success or theme.text,
                },
            }
            if u.installing then
                out[#out + 1] = rect {
                    width = "Fill",
                    height = 8,
                    radius = 4,
                    background = theme.surface,
                    children = {
                        rect {
                            height = "Fill",
                            radius = 4,
                            background = theme.accent,
                            width = string.format("%d%%", u.install_current_step * 100 // math.max(1, u.install_total_steps)),
                            animate = { width = { duration = 500, easing = "OutCubic" } },
                        },
                    },
                }
            end
            for _, p in ipairs(u.packages) do
                out[#out + 1] = package_row(p)
            end
            if not u.installing and u.count > 0 then
                out[#out + 1] = rect {
                    geometry = target("updates:install"),
                    width = "Fill",
                    height = 56,
                    radius = 16,
                    margin = { top = 6 },
                    background = theme.accent,
                    on_click = function() mantle.updates:install() end,
                    children = {
                        text {
                            content = "Update all",
                            align_h = "Center",
                            align_v = "Center",
                            font_size = 22,
                            font_weight = 700,
                            foreground = theme.crust,
                        },
                    },
                }
            end
            return out
        end),
    },
}

return { badge = badge, popover = popover }
