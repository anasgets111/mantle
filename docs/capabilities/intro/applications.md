```lua
text {
    content = computed({ mantle.workspaces, mantle.applications }, function(workspaces, applications)
        local client = workspaces and workspaces.active_client
        if client == nil or applications == nil then
            return ""
        end
        local index = applications.by_app_id[client.app_id]
        return index and applications.entries[index].name or client.app_id
    end),
}
```

<!-- reference -->

## Backend

Reads `applications/` under `$XDG_DATA_HOME` (default `~/.local/share`), then each absolute `$XDG_DATA_DIRS`
entry (default `/usr/local/share:/usr/share`), subdirectories included, four levels deep. The first file for a desktop id wins, so a
copy under `~/.local/share/applications` overrides the system one. `Type=Application` entries with
`Name` and a non-empty `Exec` are listed, including `NoDisplay=true` entries so windows can resolve
their names and icons. Launchers filter out entries with `no_display = true`; `Hidden=true` entries
are excluded entirely. inotify watches every
directory, including ones created later. `launch` spawns the `Exec` command detached, dropping field
codes such as `%u`; a `Terminal=true` entry runs as `$TERMINAL -e command args`.

## How do I…

| Task | Answer |
| :--- | :--- |
| Hide an app from a launcher | Copy its `.desktop` file to `~/.local/share/applications` and add `NoDisplay=true`; filter `entry.no_display` in the launcher |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `launch` of a terminal app does nothing | `Terminal=true` needs `$TERMINAL` in the Supervisor's environment, not an interactive shell's. `mantle log` names the refusal |

See also: [App launcher](../cookbook/launcher.md) recipe.
