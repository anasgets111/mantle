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
| Devices | Only the first Wi-Fi device is tracked. Wired fields describe the first activated wired device |
| Toggles | Networking through `Enable`, Wi-Fi through `WirelessEnabled` |
| Scan | `RequestScan`. `scanning` turns `true` on the call and `false` when `LastScan` moves or NetworkManager refuses |
| Access points | The associated one's strength is live. The others' are read when they appear and after each scan, when NetworkManager updates them |
| Connect | A saved profile or an open network in range joins at once. Anything else sets `password_ssid` and waits for the key from a `secure_submit = { capability = "network", action = "connect" }` field ([secure fields](../guide/input.md#secure-fields)); the key never reaches Lua |
| Join verdict | Watched for up to 45 s. A rejected key sets `password_ssid` again. A new network's profile, key included, is saved when the join starts and stays after a rejection; a key retyped for a saved profile reaches disk only once NetworkManager accepts it |
| Abort | `abort_connect` deletes a profile the join created, else deactivates the join |
| Missing | Stays `nil`. The next generation's first read retries |

## How do I…

### Ask for a Wi-Fi password

Show the field while `password_ssid` is set. Escape clears a secure field and keeps it armed, then
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
