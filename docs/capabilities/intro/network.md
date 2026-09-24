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
| Events | Manager, device list, device state, wireless APs, active AP and saved-profile changes rebuild state. A hotplugged device rescans the set |
| Devices | Only the first Wi-Fi device is tracked |
| Toggles | Networking via `Enable`; Wi-Fi via `WirelessEnabled`. Ethernet off disconnects wired devices; on activates each one's first autoconnect profile |
| Scan | `RequestScan`; duplicate SSIDs merge. Keeps the connected AP, then saved networks, then the strongest, capped at 20. Band from frequency |
| Connect | Saved profiles reactivate. A secured join takes its key through `secure_submit`, never Lua. An aborted join deletes the profile it created. 45 s backstop on activation |
| Disconnect | `Device.Disconnect`; NetworkManager then skips autoconnect until the user joins again |
| Missing | Stays `nil`; the next generation's start retries |

## How do I…

### Know whether a join worked

`invoke` returns nothing, so a failed `connect` shows up in state instead:

```lua
text {
    foreground = "#F38BA8",
    content = mantle.network:map(function(network)
        local failure = network and network.connect_error
        return failure and (failure.ssid .. ": " .. failure.message) or ""
    end),
}
```
