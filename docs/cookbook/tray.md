# System tray with menu

Tray icons in the bar. A left click activates the app, a middle click sends its secondary action,
the wheel scrolls it and a right click opens its menu in a dropdown. Submenus expand in place,
check marks and radio dots follow the app, and disabled entries are drawn greyed out.

```lua,shot
local menu_open = state("tray_menu_open", false)
local menu_anchor = state("tray_menu_anchor", { x = 0, y = 0, width = 1, height = 1 })
local menu_item = state("tray_menu_item", "") -- the tray item whose menu is open
local expanded = state("tray_menu_expanded", {}) -- open submenu ids, as strings

local function close_menu()
    menu_open:set(false)
end

local function open_menu(item, rect)
    menu_item:set(item.id)
    expanded:set({})
    menu_anchor:set(rect)
    menu_open:set(true)
end

local function artwork(item)
    local name = item.icon_name
    local path = item.icon_path
    if item.status == "NeedsAttention" and (item.attention_icon_name or item.attention_icon_path) then
        name, path = item.attention_icon_name, item.attention_icon_path
    end
    if path then
        return image { source = path, width = 16, height = 16, fit = "contain", align_v = "Center" }
    end
    return icon { name = name or "application-x-executable", size = 16, align_v = "Center" }
end

local tray_items = list {
    direction = "Horizontal",
    spacing = 2,
    align_v = "Center",
    source = mantle.tray:map(function(tray)
        local shown = {}
        for _, item in ipairs(tray and tray.items or {}) do
            if item.status ~= "Passive" then -- Passive asks to be hidden
                shown[#shown + 1] = item
            end
        end
        return shown
    end),
    key = function(item) return item.id end,
    itemfn = function(item)
        return button {
            padding = 4,
            radius = 6,
            on_click = function(rect, which)
                if item.menu and (which == "right" or item.item_is_menu) then
                    open_menu(item, rect)
                elseif which == "left" then
                    mantle.tray:invoke("activate", item.id, 0, 0)
                elseif which == "middle" then
                    mantle.tray:invoke("secondary_activate", item.id, 0, 0)
                end
            end,
            on_wheel = function(_, steps)
                mantle.tray:invoke("scroll", item.id, steps > 0 and 1 or -1, "vertical")
            end,
            children = { artwork(item) },
        }
    end,
}

-- The open item's menu as flat rows, depth-first, with open submenus inlined.
local function flatten(entries, depth, open, out)
    for _, entry in ipairs(entries or {}) do
        out[#out + 1] = { entry = entry, depth = depth }
        if #entry.children > 0 and open[tostring(entry.id)] then
            flatten(entry.children, depth + 1, open, out)
        end
    end
    return out
end

local menu_rows = computed({ mantle.tray, menu_item, expanded }, function(tray, id, open)
    for _, item in ipairs(tray and tray.items or {}) do
        if item.id == id then
            return flatten(item.menu, 0, open, {})
        end
    end
    return {}
end)

-- "_Quit" -> "Quit"; "__" is a literal underscore.
local function strip_mnemonic(label)
    return ((label or ""):gsub("__", "\0"):gsub("_", ""):gsub("%z", "_"))
end

local function marker(entry)
    if #entry.children > 0 then
        return "›"
    elseif entry.toggle_state == 1 then
        return entry.toggle_type == "radio" and "●" or "✓"
    end
    return ""
end

local function menu_row(row_data)
    local entry = row_data.entry
    local indent = 8 + row_data.depth * 12
    if entry.menu_type == "separator" then
        return rect {
            width = "Fill",
            padding = { top = 4, bottom = 4, left = indent, right = 8 },
            children = { rect { width = "Fill", height = 1, background = "#45475a" } },
        }
    end
    local mark = marker(entry)
    local row_hover = hover("tray_menu_row_" .. entry.id .. "_" .. row_data.depth)
    return button {
        width = "Fill",
        padding = { top = 6, bottom = 6, left = indent, right = 8 },
        radius = 6,
        hover = row_hover,
        background = row_hover:map(function(on) return on and entry.enabled and "#313244" or "#00000000" end),
        opacity = entry.enabled and 1 or 0.4,
        on_click = function()
            if #entry.children > 0 then
                local key = tostring(entry.id)
                local open = {}
                for id, value in pairs(expanded:get()) do open[id] = value end
                open[key] = not open[key] or nil
                expanded:set(open)
                if open[key] then
                    mantle.tray:invoke("menu_will_show", menu_item:get(), entry.id)
                end
            elseif entry.enabled then
                mantle.tray:invoke("activate_menu_item", menu_item:get(), entry.id)
                close_menu()
            end
        end,
        children = {
            row {
                width = "Fill",
                spacing = 8,
                children = {
                    icon { name = entry.icon_name or "", size = 14, visible = entry.icon_name ~= nil, align_v = "Center" },
                    text { content = strip_mnemonic(entry.label), width = "Fill", elide = "End", foreground = "#cdd6f4" },
                    text { content = mark, visible = mark ~= "", foreground = "#a6adc8" },
                },
            },
        },
    }
end

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        width = "Fill",
        height = 32,
        exclusive = true,
        child = row {
            width = "Fill",
            height = "Fill",
            padding = { left = 8, right = 8 },
            background = "#1e1e2e",
            children = { rect { width = "Fill" }, tray_items },
        },
    },
    popup {
        id = "tray_menu",
        parent = "bar",
        anchor_rect = menu_anchor,
        anchor = "Bottom",
        gravity = "BottomLeft",
        offset = { y = 4 },
        constraint_adjustment = { "SlideX", "FlipY" },
        visible = menu_open,
        on_dismiss = close_menu,
        width = 260,
        padding = 6,
        radius = 10,
        background = "#1e1e2e",
        border_width = 1,
        border_color = "#45475a",
        child = list {
            width = "Fill",
            max_height = 480,
            scroll = scroll("tray_menu"),
            source = menu_rows,
            -- The same entry at two depths is two rows.
            key = function(row_data) return row_data.entry.id .. ":" .. row_data.depth end,
            itemfn = menu_row,
        },
    },
}
```

## How it works

- `tray.items` arrive in registration order with their whole DBusMenu tree in `menu` ([tray](../capabilities/tray.md)).
- `on_click` gets the button's rect and the mouse button; the rect goes straight into the popup's `anchor_rect` ([pointer](../guide/input.md#pointer), [popup](../surfaces/popup.md)).
- The popup grabs the pointer, so an outside click dismisses it and `on_dismiss` clears the state ([dismissal](../surfaces/popup.md#dismissal)).
- The menu tree is flattened into one `list` with an indent per depth, so a submenu opens in place rather than in a second popup ([list](../nodes/list.md), [nested menus](../surfaces/popup.md#nested-menus) for the other way).
- `menu_will_show` lets apps that fill submenus lazily send them before the row expands.
- Tray actions take the item's opaque `id`; menu actions also take the entry's integer `id` ([capabilities](../capabilities/index.md)).

## Variations

| Change | Edit |
| :--- | :--- |
| Show hidden (Passive) items | Return `tray and tray.items or {}` from the source map |
| Tooltips | Give each button `hover = hover("tray_" .. item.id)` and add a `grab = false` popup showing `item.tooltip` ([tooltip](../surfaces/popup.md#tooltip)) |
| Bigger icons | `size = 20` and `width = 20, height = 20` |
| Open the menu rightwards, for a tray on the left | `gravity = "BottomRight"` |
| Attention badge | Stack an `icon { name = item.overlay_icon_name }` over the artwork in a `rect`, aligned `"End"` both ways |
