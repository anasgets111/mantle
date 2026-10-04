```lua
text {
    content = mantle.network:map(function(network)
        if network == nil then
            return ""
        elseif not network.connected then
            return "offline"
        elseif network.ssid == "Ethernet" then
            return "wired"
        end
        return string.format("%s %d%%", network.ssid or "", network.strength)
    end),
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Updates | Every manager, device-list, device-state, access-point, association and saved-profile change re-reads the whole state from NetworkManager. A hotplugged adapter rescans the device set |
| Devices | `wifi_devices` lists each Wi-Fi interface by name, primary first. Flat association, scan, address and access-point fields use the primary; flat join fields describe the one join on any device. Old actions use the primary. Wired fields describe the first activated wired device |
| Toggles | Networking through `Enable`, Wi-Fi through `WirelessEnabled` |
| Scan | `RequestScan`. `scanning` turns `true` on the call and `false` when `LastScan` moves or NetworkManager refuses |
| Access points | The associated one's strength is live. The others' are read when they appear and after each scan, when NetworkManager updates them |
| Connect | A saved profile or an open network in range joins at once. Anything else sets `password_ssid` and waits for the key from a `secure_submit = { capability = "network", action = "connect" }` field ([secure fields](../guide/input.md#secure-fields)); the key never reaches Lua |
| Join verdict | Watched for up to 45 s. A rejected key sets `password_ssid` again. A new network's profile, key included, is saved when the join starts and stays after a rejection; a key retyped for a saved profile reaches disk only once NetworkManager accepts it |
| Abort | `abort_connect` deletes a profile the join created, else deactivates the join |
| VPN profiles | `vpns` lists saved profiles of type `vpn` (plugin VPNs) and `wireguard`, by name. `active` and `activating` follow NetworkManager's active connections, so a VPN started elsewhere shows too. The list needs a profile change or restart to notice an edited profile |
| VPN activation | `connect_vpn(uuid)` calls `ActivateConnection` and watches the verdict for 45 s, not counting time on that profile's secret prompt; at 45 s it deactivates the attempt. A failure lands in `vpn_error`; `disconnect_vpn` or `cancel_vpn_secret` during the attempt sets none. A second `connect_vpn` for a profile already waiting does nothing. Only profiles NetworkManager manages: no VPN client the shell runs itself |
| VPN secrets | The engine registers a NetworkManager secret agent, answering only NetworkManager itself. When an activation needs secrets, `vpn_secret` lists the `fields` to enter and waits for `secure_submit` fields named `request.id .. "/" .. field`; the last one answers NetworkManager. One request at a time: a second is refused. `cancel_vpn_secret` or a 120 s wait fails the activation. Nothing is stored; NetworkManager asks again when a secret is agent-owned |
| VPN secret scope | Only `vpn` and `wireguard` requests are answered; other connection types get "no secrets", as with no agent. Fields come from NetworkManager's hints, else from the profile's `<key>-flags` entries marked agent-owned or not saved. WireGuard asks for `private-key` only; a peer's preshared key and a plugin's `x-vpn-message` text are not handled. Registering needs polkit's `org.freedesktop.NetworkManager.network-control`; without it a warning is logged and VPN activation still works for secrets NetworkManager already stores. NetworkManager takes one agent per identifier per user, so only the first running shell gets prompts; a second logs the same warning |
| Missing | Stays `nil`. The next generation's first read retries |

Use `scan_device(id)`, `connect_device(ssid, hidden, id)` and `disconnect_wifi_device(id)` to target an entry in `wifi_devices`. An unknown or removed ID is never switched to another device: all three log a warning and return. Without Wi-Fi hardware, `scan` and `disconnect_wifi` do nothing; `connect` reports an error in the flat `connect_error` field. A password prompt keeps its selected device through submission and activation. Only one password prompt or join attempt is tracked across all devices. Starting another settles the previous join first.

## How do I…

### Ask for a Wi-Fi password

A click on an `available_networks` entry calls `mantle.network:connect(entry.ssid, false)`; a secured one without a
saved profile then sets `password_ssid`. Show the field while it is set. Escape clears a secure field and keeps it armed, then
calls its `on_cancel`, the place to call `cancel_connect`:

```lua
local asking = mantle.network:map(function(network)
    return network ~= nil and network.password_ssid ~= nil
end)

return column {
    visible = asking,
    spacing = 6,
    children = {
        text {
            content = mantle.network:map(function(network)
                return network and network.password_ssid and ("Password for " .. network.password_ssid) or ""
            end),
        },
        textfield {
            width = 240,
            height = 24,
            placeholder = "Password",
            secure_submit = { capability = "network", action = "connect" },
            on_cancel = function() mantle.network:cancel_connect() end,
        },
    },
}
```

### List and toggle a VPN

Which profile counts as the preferred one is the shell's call; `vpns` carries what it needs:

```lua
local vpns = mantle.network:map(function(network) return network and network.vpns or {} end)

return column {
    spacing = 4,
    children = vpns:map(function(list)
        local rows = {}
        for _, vpn in ipairs(list) do
            rows[#rows + 1] = row {
                on_click = function()
                    if vpn.active or vpn.activating then
                        mantle.network:disconnect_vpn(vpn.uuid)
                    else
                        mantle.network:connect_vpn(vpn.uuid)
                    end
                end,
                children = { text { content = vpn.id .. (vpn.active and " (on)" or "") } },
            }
        end
        return rows
    end),
}
```

### Ask for a VPN secret

NetworkManager's request sets `vpn_secret`; show one secure field per entry of `fields`. `cancel_vpn_secret`
drops the whole request, not one field, and fails the activation. Here a first Escape clears the field and
a second cancels. `retry` is `true` after rejected secrets:

```lua
local request = mantle.network:map(function(network) return network and network.vpn_secret end)

return column {
    visible = request:map(function(secret) return secret ~= nil end),
    spacing = 6,
    children = request:map(function(secret)
        local rows = {}
        for _, field in ipairs(secret and secret.fields or {}) do
            rows[#rows + 1] = textfield {
                width = 240,
                height = 24,
                placeholder = field,
                secure_submit = { capability = "network", action = "vpn_secret", name = secret.id .. "/" .. field },
                on_cancel = function(cleared)
                    if not cleared then mantle.network:cancel_vpn_secret() end
                end,
            }
        end
        return rows
    end),
}
```

### Know whether a join worked

A failed `connect` lands in `connect_error`:

```lua
text {
    foreground = "#F38BA8",
    content = mantle.network:map(function(network)
        local failure = network and network.connect_error
        return failure and (failure.ssid .. ": " .. failure.message) or ""
    end),
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `available_networks` has a row with `ssid == ""` | Hidden networks broadcast no name; they merge into one nameless row. Skip it and join hidden networks with `connect(ssid, true)` |
| A hidden network asks for a password even when open | Its security is unknown until it answers. Enter on the empty field joins it as open |
