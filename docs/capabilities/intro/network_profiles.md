```lua
local public_dns = {
    ipv4 = { dns = { "1.1.1.1", "8.8.8.8" }, ignore_auto_dns = true },
    ipv6 = { dns = { "2606:4700:4700::1111", "2001:4860:4860::8888" }, ignore_auto_dns = true },
}

return column {
    spacing = 4,
    children = mantle.network_profiles:map(function(state)
        local rows = {}
        for _, profile in ipairs(state.profiles) do
            if profile.active and (profile.kind == "wifi" or profile.kind == "ethernet") then
                rows[#rows + 1] = row {
                    on_click = function()
                        mantle.network:update_profile(profile.uuid, public_dns)
                        mantle.network:reapply_profile(profile.uuid)
                    end,
                    children = {
                        text { content = profile.id .. ": " .. table.concat(profile.ipv4 and profile.ipv4.dns or {}, " ") },
                    },
                }
            end
        end
        return rows
    end, {}),
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Source | The same NetworkManager connection as [`network`](network.md); reading either starts both |
| Updates | A profile added or removed re-reads every profile; NetworkManager's `Updated` re-reads the one it names, so an edit from `nmcli` shows too. `active` and `activating` follow NetworkManager's active connections, re-read only when one changes |
| Order | `_profile` actions run in call order: an `update_profile` lands before the `reapply_profile` or `activate_profile` called after it. An activation lets the next action start once NetworkManager takes the request, not at its verdict |
| Update | Reads the profile, changes only the keys the edit names and writes the whole profile back with `Update2`, which keeps its stored secrets. `NO_REAPPLY` keeps even `metered` off an active connection until `reapply_profile` or `activate_profile`. A profile edited elsewhere in between is read again once. DNS servers are written as `dns-data`, addresses as `address-data` |
| Reapply | On the interface named, else on every device the profile's activations name: `GetAppliedConnection` checks the profile is applied there, then `Reapply` applies the saved settings without reconnecting. NetworkManager refuses some changes, such as the IP method; `error` then carries its message and `activate_profile` reconnects with them |
| Activate | `ActivateConnection` on the interface named, else where NetworkManager picks: it follows the profile's `interface-name` and creates a missing bridge or VLAN. Watched for 90 s; past that the attempt is stopped. A `deactivate_profile` meanwhile is no error. A VPN or WireGuard profile calls `connect_vpn` instead, so its secret prompt and failure land on `network` |
| Errors | `error` holds the last failed action with NetworkManager's message; `"not authorized"` for a polkit refusal, `"no such profile"` and `"no interface <name>"` for a UUID or interface NetworkManager does not know |
| Missing | Stays `nil`, as `network` does |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `update_profile` sets `error` to "needs NetworkManager 1.44+" | Older NetworkManager has no `version-id` check. Edit with `nmcli` there |
| An edit to an active profile does not change the connection | Call `reapply_profile(uuid)`, or `activate_profile(uuid)`, which reconnects |
| `update_profile` on `Wired connection 1` writes a file | NetworkManager generates that profile in memory; saving it writes it to disk, as `nmcli con mod` does |
| `update_profile` fails with "a key typed in connect is not saved yet" | A key typed in `network:connect` stays staged until a join with the right key succeeds or NetworkManager restarts. Join with the right key, then retry |
| `update_profile` fails with "the profile has unsaved changes" | Another tool changed the profile in memory only, as `nmcli connection modify --temporary` does. Save it there, or restart NetworkManager |
| A DNS-over-TLS server read from `dns` raises in `update_profile` | Edits take plain addresses. A `dns` edit replaces the whole list, DNS-over-TLS servers included; an edit without `dns` keeps them |
| `activate_profile` on Wi-Fi fails with "password is missing or wrong" | It never prompts. Join with `network:connect(ssid, false)`, which asks for the key |
