fakes = {}
fakes.workspaces = {
    compositor = "hyprland",
    outputs = { { name = "DP-1", active_workspace = "2", workspaces = {
        { id = "1", number = 1, populated = true },
        { id = "2", number = 2, populated = false },
        { id = "3", number = 3, name = "web", populated = true },
    } } },
    special = { { name = "special:scratch", populated = true, shown_on = "DP-1" } },
}
