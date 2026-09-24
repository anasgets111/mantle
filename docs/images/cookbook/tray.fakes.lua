fakes = {}
local function entry(id, label, extra)
    local fields = { id = id, label = label, enabled = true, menu_type = "standard", children = {} }
    for key, value in pairs(extra or {}) do fields[key] = value end
    return fields
end
fakes.tray = { items = {
    { id = "1.2/SNI", name = "nm", icon_name = "network-wireless-symbolic", item_is_menu = true, status = "Active", menu = {
        entry(1, "_Wi-Fi settings"),
        entry(2, "", { menu_type = "separator" }),
        entry(3, "Networks", { children = { entry(5, "Home"), entry(6, "Office") } }),
        entry(4, "Airplane mode", { toggle_type = "checkmark", toggle_state = 1 }),
        entry(7, "Disconnect", { enabled = false }),
    } },
    { id = "1.3/SNI", name = "bt", icon_name = "bluetooth-active-symbolic", item_is_menu = false, status = "Active" },
} }
state("tray_menu_item", ""):set("1.2/SNI")
state("tray_menu_expanded", {}):set({ ["3"] = true })
-- The network button's rect, as its click would pass it.
state("tray_menu_anchor", {}):set({ x = 646, y = 4, width = 24, height = 24 })
state("tray_menu_open", false):set(true)
