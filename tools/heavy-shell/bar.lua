-- A bottom bar per output: workspaces, sysinfo/audio/network readings, a 1 s clock.
local theme = require("theme")

local bar_shown = state("hs_bar", true)

mantle.sysinfo:configure({ cpu_interval = 1, ram_interval = 2, net_interval = 1 })

local function output_of(workspaces, name)
    for _, output in ipairs(workspaces and workspaces.outputs or {}) do
        if output.name == name then return output end
    end
end

local function workspace_item(item)
    return rect {
        width = item.active and 32 or 20,
        height = 18,
        radius = 9,
        background = item.active and theme.color("accent") or theme.color("surface"),
        animate = { width = { duration = 180, easing = "out_cubic" } },
        on_click = function() mantle.workspaces:focus(item.id) end,
        children = {
            text {
                content = item.label,
                align_h = "center",
                align_v = "center",
                font_size = 10,
                foreground = theme.color("fg"),
            },
        },
    }
end

local function workspace_key(item) return tostring(item.id) end

local function workspaces(name)
    return list {
        direction = "horizontal",
        spacing = 4,
        align_v = "center",
        source = mantle.workspaces:map(function(all)
            local output = output_of(all, name)
            local items = {}
            for index, workspace in ipairs(output and output.workspaces or {}) do
                items[index] = {
                    id = workspace.id,
                    label = tostring(workspace.number or workspace.name),
                    active = workspace.id == output.active_workspace,
                }
            end
            return items
        end),
        key = workspace_key,
        itemfn = workspace_item,
    }
end

local function reading(content)
    return text { content = content, align_v = "center", font_size = 12, foreground = theme.color("fg") }
end

local cpu = mantle.sysinfo:map(function(s) return s and string.format("CPU %d%%", s.cpu_percent) or "CPU --" end)
local ram = mantle.sysinfo:map(function(s) return s and string.format("RAM %d%%", s.ram_percent) or "RAM --" end)
local net = mantle.sysinfo:map(function(s)
    if not s then return "" end
    return string.format("down %dK up %dK", s.net_rx_bytes_sec // 1024, s.net_tx_bytes_sec // 1024)
end)
local volume = mantle.audio:map(function(a)
    if not a or not a.volume then return "VOL --" end
    return a.muted and "VOL muted" or string.format("VOL %d%%", math.floor(a.volume + 0.5))
end)
local wifi = mantle.network:map(function(n) return n and (n.ssid or "offline") or "NET --" end)
local clock = mantle.system:map(function(s) return os.date("%H:%M:%S", s and s.time) end)

return panel {
    id = "hs_bar",
    layer = "top",
    anchor = { bottom = true, left = true, right = true },
    width = "fill",
    height = 26,
    visible = bar_shown,
    child = function(output)
        theme.counters.bar_child = theme.counters.bar_child + 1
        return row {
            width = "fill",
            height = "fill",
            spacing = 12,
            padding = { left = 10, right = 10 },
            background = theme.counted("bg", "bar_color"),
            children = {
                workspaces(output),
                rect { width = "fill" },
                reading(cpu),
                reading(ram),
                reading(net),
                reading(volume),
                reading(wifi),
                text { content = clock, align_v = "center", font_size = 12, foreground = theme.color("accent") },
            },
        }
    end,
}
