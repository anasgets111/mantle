```lua
list {
    direction = "Horizontal",
    source = mantle.power:map(function(power)
        return power and power.profiles or {}
    end),
    itemfn = function(name)
        return button {
            padding = 6,
            background = mantle.power:map(function(power)
                return (power and power.active_profile == name) and "#89B4FA" or "#313244"
            end),
            on_click = function() mantle.power:set_profile(name) end,
            children = { text { content = name } },
        }
    end,
}
```

<!-- reference -->

## Backend

| Half | Source | Missing |
| :--- | :--- | :--- |
| `active_profile`, `profiles` | power-profiles-daemon: `org.freedesktop.UPower.PowerProfiles`, else `net.hadess.PowerProfiles` | Both fields absent; `set_profile` is logged and ignored |
| `on_battery`, `energy_rate` | UPower's `OnBattery` and its `DisplayDevice`'s `EnergyRate` | Both fields absent |

Every `OnBattery`, `EnergyRate` or `ActiveProfile` change re-reads all four fields. With neither
service the push is an empty table.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| power-profiles-daemon started after the capability never shows up | The daemon is looked up once, on the first read. Restart `mantle` after installing it |
| `energy_rate` has no sign | It is a magnitude in watts. Read `mantle.battery`'s `state` for the direction |

See also: [battery](battery.md) for charge and time estimates.
