```lua
list {
    source = mantle.bluetooth:map(function(bluetooth)
        return bluetooth and bluetooth.connected_devices or {}
    end),
    key = function(device) return device.mac end,
    itemfn = function(device)
        local battery = device.battery >= 0 and string.format(" %d%%", device.battery) or ""
        return button {
            on_click = function() mantle.bluetooth:invoke("disconnect", device.mac) end,
            children = { text { content = device.name .. battery } },
        }
    end,
}
```

<!-- reference -->

## Backend

BlueZ on the system bus.

| Contract | Behavior |
| :--- | :--- |
| State | `org.bluez`'s `ObjectManager` plus property changes. An adapter added later is picked up; a `bluetoothd` started after the Supervisor is not. Without BlueZ, `available` is `false`, the lists stay empty and every action does nothing |
| Agent | Mantle registers the default `DisplayYesNo` agent at `/org/mantle/Bluez/Agent1`. A confirmation, authorization or displayed code becomes `pairing_request` only while the adapter is discoverable or Mantle is pairing that device. A `"service"` request asks only for a paired device. One request shows at a time: a second is rejected, unless the first only displays a code and the second needs an answer. PIN and passkey entry are rejected |
| Discovery | `start_discovery` clears `discovered_devices`; `stop_discovery` keeps it |
| Battery, category | `Battery1` gives `battery`; the `Class` major and minor bits give `category` |
| Codecs | PipeWire owns them: [`audio`](audio.md)`.bluetooth` lists each device's profiles and `set_bluetooth_profile` switches one |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A device pairing from its own side gets rejected | Mantle only prompts for invited devices. Set `set_discoverable` to `true` while pairing |
| A device that needs a PIN typed on the computer fails to pair | The agent rejects PIN and passkey entry. Pair it with `bluetoothctl`, which brings its own agent |
