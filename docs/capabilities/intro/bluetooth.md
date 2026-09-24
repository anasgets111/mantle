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

| Contract | Behavior |
| :--- | :--- |
| State | `ObjectManager` plus property changes. An adapter added later is picked up; a `bluetoothd` started after the Supervisor is not. Without BlueZ the capability is inert |
| Agent | The default `DisplayYesNo` agent at `/org/mantle/Bluez/Agent1`. A confirmation, authorization or displayed code becomes `pairing_request`, only while the adapter is visible or this shell is pairing that device. PIN and passkey entry are rejected |
| Discovery | Starting clears the discovered list; stopping keeps it. BlueZ expires only devices still marked temporary, after `TemporaryTimeout` (30 s default) |
| Battery, category | `Battery1` gives accessory charge; the `Class` major/minor bits give the category |
| Codecs | From PipeWire: [`audio`](audio.md)`.bluetooth` lists each device's profiles and `set_bluetooth_profile` switches one |
