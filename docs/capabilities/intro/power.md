```lua
list {
    direction = "Horizontal",
    source = mantle.power:map(function(power)
        return power and power.profiles or {}
    end),
    itemfn = function(name)
        local power = mantle.power:get()
        return button {
            padding = 6,
            background = (power and power.active_profile == name) and "#89B4FA" or "#313244",
            on_click = function() mantle.power:invoke("set_profile", name) end,
            children = { text { content = name } },
        }
    end,
}
```

<!-- reference -->

## Backend

power-profiles-daemon (`org.freedesktop.UPower.PowerProfiles`, else `net.hadess.PowerProfiles`)
for profiles; UPower's `OnBattery` and `EnergyRate` for the rest. Either half may be missing; its
fields stay absent, and `set_profile` without the daemon is ignored.
