```lua
list {
    source = mantle.bluetooth:map(function(bluetooth)
        return bluetooth and bluetooth.connected_devices or {}
    end),
    key = function(device) return device.mac end,
    itemfn = function(device)
        local battery = device.battery >= 0 and string.format(" %d%%", device.battery) or ""
        return rect {
            on_click = function() mantle.bluetooth:disconnect(device.mac) end,
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
| State | `org.bluez`'s `ObjectManager` plus property changes. An adapter added later is picked up. A `bluetoothd` that starts late or restarts is read afresh and gets the pairing agent again; what the old one reported, and its open `pairing_request`, are dropped. Without BlueZ, `available` is `false`, the lists stay empty and every action does nothing |
| Agent | Mantle registers the default `DisplayYesNo` agent at `/org/mantle/Bluez/Agent1`. A confirmation, authorization or displayed code becomes `pairing_request` only while the adapter is discoverable or Mantle is pairing that device. A `"service"` request asks only for a paired device. One request shows at a time: a second is rejected, unless the first only displays a code and the second needs an answer. PIN and passkey entry are rejected |
| Discovery | `start_discovery` clears `discovered_devices`, but the next device change refills it with every unpaired device BlueZ still holds, earlier scans' included; `stop_discovery` keeps it |
| Battery, category | `Battery1` gives `battery`; the `Class` major and minor bits give `category` |
| Codecs | PipeWire owns them: [`audio`](audio.md)`.bluetooth` lists each device's profiles and `set_bluetooth_profile` switches one |

## How do I…

### Pair a new device

Scan, `pair` a discovered device, then answer the `pairing_request` it raises. A `"display"`
request only shows a code to type on the device; the others take a yes or no, and a yes within
750 ms of the prompt appearing is ignored so a stray click cannot accept it:

```lua
local function answer(accept)
    return function()
        local bluetooth = mantle.bluetooth:get()
        local request = bluetooth and bluetooth.pairing_request
        if request then
            mantle.bluetooth:answer_pairing(request.mac, accept)
        end
    end
end

local function button(label, on_click)
    return rect { padding = 4, on_click = on_click, children = { text { content = label } } }
end

return column {
    spacing = 6,
    children = {
        button("Scan", function() mantle.bluetooth:start_discovery() end),
        list {
            source = mantle.bluetooth:map(function(bluetooth)
                return bluetooth and bluetooth.discovered_devices or {}
            end),
            key = function(device) return device.mac end,
            itemfn = function(device)
                return button(device.name ~= "" and device.name or device.mac, function()
                    mantle.bluetooth:pair(device.mac)
                end)
            end,
        },
        text {
            content = mantle.bluetooth:map(function(bluetooth)
                local request = bluetooth and bluetooth.pairing_request
                if request == nil then
                    return ""
                end
                return string.format("%s (%s) %s", request.name, request.kind, request.code or "")
            end),
        },
        row {
            visible = mantle.bluetooth:map(function(bluetooth)
                local request = bluetooth and bluetooth.pairing_request
                return request ~= nil and request.kind ~= "display"
            end),
            spacing = 6,
            children = { button("Yes", answer(true)), button("No", answer(false)) },
        },
    },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A device pairing from its own side gets rejected | Mantle only prompts for invited devices. Set `set_discoverable` to `true` while pairing |
| A device that needs a PIN typed on the computer fails to pair | The agent rejects PIN and passkey entry. Pair it with `bluetoothctl`, which brings its own agent |
