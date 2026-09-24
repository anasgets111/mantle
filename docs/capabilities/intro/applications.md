```lua
text {
    content = computed({ mantle.workspaces, mantle.applications }, function(workspaces, applications)
        local client = workspaces and workspaces.active_client
        if client == nil or applications == nil then
            return ""
        end
        local index = applications.by_app_id[client.class]
        return index and applications.entries[index].name or client.class
    end),
}
```

<!-- reference -->

## Backend

Reads `applications/` under `$XDG_DATA_HOME`, then each `$XDG_DATA_DIRS` entry (default
`/usr/local/share:/usr/share`), subdirectories included. The first file for a desktop id wins, so a
copy under `~/.local/share/applications` overrides the system one. `Type=Application` entries with
`Name` and `Exec` are listed; `NoDisplay=true` and `Hidden=true` ones are not. inotify watches every
directory, including ones created later.

## How do I…

| Task | Answer |
| :--- | :--- |
| Hide an app from a launcher | Copy its `.desktop` file to `~/.local/share/applications` and add `NoDisplay=true`; the rescan drops it |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `launch` of a terminal app does nothing | `Terminal=true` needs `$TERMINAL` in the Supervisor's environment, not an interactive shell's. `mantle log` names the refusal |

See also: [App launcher](../cookbook/launcher.md) recipe.
