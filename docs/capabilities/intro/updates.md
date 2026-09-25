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

The first manager that owns `/` wins, each on `PATH` at start: `pacman` with packages in
`/var/lib/pacman/local`, else `apt-get` with packages in `/var/lib/dpkg/status`, else `dnf`. With
none, `package_manager` is `nil` and every action is ignored. The dnf and apt backends are
verified in containers only, untested on a Fedora or Ubuntu install for now.

| Part | pacman (Arch) | dnf (Fedora) | apt (Debian, Ubuntu) |
| :--- | :--- | :--- | :--- |
| Check | `curl` fetches each repo database `pacman-conf` lists into `$XDG_RUNTIME_DIR/mantle/pacman`, skipping one the mirror reports unchanged. `pacman -Qu`, `-Sp` and `-Si` read it against `/var/lib/pacman/local` | `dnf repoquery --refresh --upgrades`, then `--installed` for `old_version`; metadata goes to your cache (`~/.cache/libdnf5`) | `apt-get update` into `$XDG_RUNTIME_DIR/mantle/apt` with its hooks cleared, then `apt-get -s upgrade --with-new-pkgs` and `apt-cache show` for sizes |
| AUR | With `aur = true`, `pacman -Qm` names the foreign packages, one `curl` POST to the AUR RPC (30 s timeout) covers them, and `vercmp` orders the versions. `aur_helper` is `paru`, else `yay` | None: `aur_helper` is `nil`, `aur` only sets `aur_error` | Same as dnf |
| Install | `env LC_ALL=C pkexec pacman -Syu --noconfirm`, or `env LC_ALL=C <aur_helper> -Syu --noconfirm --sudo pkexec` with `aur` on | `pkexec env LC_ALL=C dnf upgrade -y --refresh` | `pkexec env LC_ALL=C DEBIAN_FRONTEND=noninteractive sh -c "apt-get update && apt-get -y ... upgrade --with-new-pkgs"`; a changed config file keeps your copy |
| Progress | pacman's `(2/5) upgrading name` lines; nothing while downloading, since pacman draws no progress without a tty | `[ 4/12] Upgrading name-...` lines. The total also counts removals of the old versions and, on dnf5, two setup steps; dnf5 cuts a long name to fit its column | Each `Setting up name` line against the `4 upgraded, 1 newly installed` summary; `0` while downloading |

Every check runs as you and leaves the system's package databases untouched. `pkexec` asks the
session's polkit agent; the [polkit rule](../guide/installation.md#install) makes that one approval
per run for `wheel` users of pacman, and dnf and apt install in one `pkexec` call, so one prompt.
`reboot_required` mirrors `/run/mantle-reboot-required` through an inotify watch on `/run`, whatever
the manager.

What `packages` holds differs by manager:

| Field | pacman | dnf | apt |
| :--- | :--- | :--- | :--- |
| `repository` | The repo, or `"aur"` | The repo id | The suites carrying the new version, comma-joined: `"noble-updates,noble-security"` |
| `download_size` | `0` once cached | The package size | The `.deb`'s size even when cached |
| `installed_size` | From `-Si`, rounded to ~5 KiB | Exact | Exact to the KiB |
| Left out | `IgnorePkg` entries | `excludepkgs` entries | New dependencies the upgrade pulls in; a package whose upgrade would remove another, and Ubuntu's phased updates not yet offered, are held back, listed and installed by neither. `sudo apt full-upgrade` takes those |
| Duplicates | None | One entry per architecture when two of one package upgrade | None |

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
