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

`.desktop` files under `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` `applications/`; the first entry for an
ID wins. Watched with inotify, subdirectories and later-created directories included: a change rescans
250 ms after the last event. `launch` spawns detached (`Terminal=true` needs
`$TERMINAL`); `open_url` hands `http`, `https` and `mailto` URLs (≤ 2048 bytes) to `xdg-open`.

## How do I…

| Task | Answer |
| :--- | :--- |
| Name or iconify the focused app | `workspaces.active_client.class` through `applications.by_app_id`, as in the example above |

See also: [App launcher](../cookbook/launcher.md) recipe.
