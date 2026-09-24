```lua
mantle.updates:invoke("configure", { interval = 3600 })

button {
    on_click = function() mantle.updates:invoke("check") end,
    children = {
        text {
            content = mantle.updates:map(function(updates)
                if updates == nil or updates.checking then
                    return "…"
                end
                return updates.count > 0 and (updates.count .. " updates") or "up to date"
            end),
        },
    },
}
```

<!-- reference -->

## Backend

libalpm syncs into a user-owned database; the AUR RPC covers foreign packages when `aur = true`.
Installs run `pkexec pacman -Syu --noconfirm`, or paru/yay with `--sudo pkexec`, with progress
parsed from their output. `/run/mantle-reboot-required` drives `reboot_required`. The
[polkit rule](../guide/installation.md#install) keeps one approval per run for `wheel` users.
