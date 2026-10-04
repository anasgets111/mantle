-- Demo: the director feeds `mock_updates` in `mantle.updates`' shape with `mantle set`, so the take
-- installs nothing. A real shell reads `mantle.updates` and calls `mantle.updates:install()`, which
-- asks polkit, and so the shell's own agent, for the password.
local theme = require("theme")
local target = require("targets")
local layout = require("layout")

local placed = mantle.screens:map(function(screens)
    return layout.dock(screens and screens[1], 620)
end)

local updates = state("mock_updates", { packages = {}, installing = false })
local open = state("updates_open", false)
local mapped = computed({ open, delay(open, 300) }, function(now, was) return now or was end)

local badge = rect {
    geometry = target("updates"),
    visible = updates:map(function(u) return #u.packages > 0 or u.installing end),
    margin = { right = 14 },
    height = 40,
    align_v = "center",
    padding = { left = 14, right = 18 },
    radius = 20,
    clip = "box",
    background = theme.surface,
    scale = 1,
    animate = {
        scale = { duration = 320, easing = "out_back", from = 0.5 },
        width = { duration = 260, easing = "out_cubic" },
    },
    children = {
        row {
            height = "fill",
            spacing = 10,
            children = {
                icon {
                    name = "software-update-available-symbolic",
                    size = 22,
                    align_v = "center",
                    foreground = theme.accent,
                },
                text {
                    content = updates:map(function(u)
                        if u.installing then return string.format("%d/%d", u.install_current_step, u.install_total_steps) end
                        return tostring(#u.packages)
                    end),
                    align_v = "center",
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
        width = "fill",
        spacing = 12,
        children = {
            text { content = p.name, font_size = 20, foreground = theme.text },
            rect { width = "fill" },
            text { content = p.old_version, font_size = 17, foreground = theme.muted },
            text { content = "→", font_size = 17, foreground = theme.muted },
            text { content = p.new_version, font_size = 17, foreground = theme.success },
        },
    }
end

local popover = panel {
    id = "updates",
    layer = "overlay",
    anchor = { top = true, left = true },
    margin = placed:map(function(p) return { top = p.top, left = p.left } end),
    visible = mapped,
    child = column {
        width = placed:map(function(p) return p.width end),
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
            translate = { spring = { stiffness = 260, damping = 17 }, from = { y = -20 } },
        },
        children = updates:map(function(u)
            local done = not u.installing and #u.packages == 0
            local out = {
                text {
                    content = u.installing and ("Installing " .. u.install_current_package)
                        or done and "Up to date"
                        or string.format("%d updates", #u.packages),
                    font_size = 28,
                    font_weight = 800,
                    foreground = done and theme.success or theme.text,
                },
            }
            if u.installing then
                out[#out + 1] = rect {
                    width = "fill",
                    height = 8,
                    radius = 4,
                    background = theme.surface,
                    children = {
                        rect {
                            height = "fill",
                            radius = 4,
                            background = theme.accent,
                            width = string.format("%d%%", u.install_current_step * 100 // math.max(1, u.install_total_steps)),
                            animate = { width = { duration = 500, easing = "out_cubic" } },
                        },
                    },
                }
            end
            for _, p in ipairs(u.packages) do
                out[#out + 1] = package_row(p)
            end
            if not u.installing and #u.packages > 0 then
                out[#out + 1] = rect {
                    geometry = target("updates:install"),
                    width = "fill",
                    height = 56,
                    radius = 16,
                    margin = { top = 6 },
                    background = theme.accent,
                    on_click = function() mantle.updates:install() end,
                    children = {
                        text {
                            content = "Update all",
                            align_h = "center",
                            align_v = "center",
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
