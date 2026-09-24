```lua
mantle.updates:configure({ interval = 3600 })

button {
    on_click = function() mantle.updates:check() end,
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

| Part | Behaviour |
| :--- | :--- |
| Detection | `pacman` on `PATH` at start, and `paru`, else `yay`, as `aur_helper`. Without `pacman`, `package_manager` is `nil` and every action is ignored |
| Check | A child process syncs the repo databases into `$XDG_RUNTIME_DIR/mantle/pacman`, against `/var/lib/pacman/local`; the system's own databases stay untouched. With `aur = true`, one `curl` POST to the AUR RPC (30 s timeout) covers the foreign packages |
| Install | `pkexec pacman -Syu --noconfirm`, or `<aur_helper> -Syu --noconfirm --sudo pkexec` with `aur` on. `pkexec` asks the session's polkit agent. The [polkit rule](../guide/installation.md#install) makes that one approval per run for `wheel` users |
| Progress | Parsed from pacman's `(2/5) upgrading name` lines. The download phase prints nothing, since pacman draws no progress without a tty |
| Reboot | An inotify watch on `/run` mirrors `/run/mantle-reboot-required` into `reboot_required` |

## How do I…

### Remember the last check across restarts

Save each successful check in a [`persistent_table`](../guide/scripting.md#persistent_table), and
send `configure` only once the file has loaded, so its seed lands before the first scheduled check.

```lua
local dir = (os.getenv("HOME") or "") .. "/.local/state/myshell"
local cache = persistent_table { path = dir, name = "updates.json" }
local file = dir .. "/updates.json"

mantle.storage:on_change(function(storage, previous)
    local saved = storage.files[file]
    if saved and not (previous and previous.files[file]) then
        mantle.updates:configure({
            interval = 3600,
            checked_at = saved.checked_at,
            packages = saved.packages,
        })
    end
end)

mantle.updates:on_change(function(updates, previous)
    local at = updates.last_successful_check
    if at and at ~= (previous and previous.last_successful_check) then
        cache:set("checked_at", at)
        cache:set("packages", updates.packages)
    end
end)

return text {
    content = mantle.updates:map(function(updates)
        return updates and tostring(updates.count) or ""
    end),
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `packages` still lists everything after a successful install | `install` does not recheck. Invoke `check` from `on_change` when `installing` falls with `install_exit_code == 0` |
| `install` ends at once with `install_exit_code` `127` | No polkit agent answered `pkexec`. Read `mantle.polkit` and draw its prompt ([polkit](polkit.md)), or run another agent. `126` means the prompt was dismissed |
