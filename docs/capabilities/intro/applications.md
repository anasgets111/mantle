```lua
text {
    content = computed({ mantle.workspaces, mantle.applications }, function(workspaces, applications)
        local client = workspaces.active_client
        if client == nil then
            return ""
        end
        local index = applications.by_app_id[client.app_id]
        return index and applications.entries[index].name or client.app_id
    end, ""),
}
```

<!-- reference -->

## Backend

Reads `applications/` under `$XDG_DATA_HOME` (default `~/.local/share`), then each absolute `$XDG_DATA_DIRS`
entry (default `/usr/local/share:/usr/share`), subdirectories included, four levels deep. The first file for a desktop id wins, so a
copy under `~/.local/share/applications` overrides the system one. `Type=Application` entries with
`Name` and a non-empty `Exec` are listed, including `NoDisplay=true` entries so windows can resolve
their names and icons. Launchers filter out entries with `no_display = true`; `Hidden=true` entries
are excluded entirely. `Name`, `GenericName`, `Comment`, `Keywords` and `Icon` resolve through the active locale (`$LC_ALL`,
else `$LC_MESSAGES`, else `$LANG`), trying `ll_CC@mod`, `ll_CC`, `ll@mod`, `ll` and then the bare key.
Each entry's `Actions=` list is published as `entry.actions` (each with `key`, localized `name` and
optional `icon`) for `[Desktop Action <key>]` groups that declare a `Name` and valid `Exec`;
`launch_action(id, action, opts?)` runs one detached, inheriting the entry's `Terminal` and `Path`.
inotify watches every
directory, including ones created later. `launch` spawns the `Exec` command detached, dropping field
codes such as `%u`; a `Terminal=true` entry runs as `$TERMINAL -e command args`. The program starts in the entry's `Path`
directory when that exists. `launch(id, { env = { NAME = "value", OTHER = false } })` (and `launch_action(id, action, { env = ... })`)
sets or unsets variables for that launch (a string sets, `false` unsets; for a `Terminal=true` entry they apply to the `$TERMINAL` process); a single-instance app that is already running passes the launch to
its existing process, which keeps its old environment. `open_url` takes no options: use
`process.detach("xdg-open", { url }, { env = ... })`.

On the session bus, the capability also claims `com.canonical.Unity` (without replacing an existing
owner), subscribes to `com.canonical.Unity.LauncherEntry.Update` signals, and exposes live unread
counts, progress bars and urgency under `badges`, keyed by desktop id
(`"application://org.telegram.desktop.desktop"` becomes `"org.telegram.desktop"`, matching
`entries[].id`). Partial updates merge into the existing entry until updated again or replaced by
another sender; entries are removed when their D-Bus sender disconnects. At most 64 concurrent
desktop ids are tracked.

## How do I…

| Task | Answer |
| :--- | :--- |
| Hide an app from a launcher | Copy its `.desktop` file to `~/.local/share/applications` and add `NoDisplay=true`; filter `entry.no_display` in the launcher |
| Show a badge on a dock icon | Read `local badge = applications.badges[entry.id]`; when `badge ~= nil`, check `badge.count_visible`, `badge.progress_visible` and `badge.urgent` |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `launch` of a terminal app does nothing | `Terminal=true` needs `$TERMINAL` in the Supervisor's environment, not an interactive shell's. `mantle log` names the refusal |

See also: [App launcher](../cookbook/launcher.md) recipe.
