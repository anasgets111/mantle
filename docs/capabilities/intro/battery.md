```lua
text {
    content = mantle.battery:map(function(battery)
        local seconds = battery.present and battery.time_to_empty
        if not seconds then
            return ""
        end
        return string.format("%d:%02d left", seconds // 3600, seconds % 3600 // 60)
    end, ""),
}
```

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Source | UPower's `DisplayDevice` for the system battery; `EnumerateDevices` for `peripherals` and `health`, which sums each system battery's `EnergyFull` over its `EnergyFullDesign` because `DisplayDevice` reports no design capacity |
| Updates | Re-reads the display device on `PropertiesChanged`; follows peripheral add, remove and property changes; retries failed enumeration after one second |
| No system battery | `present = false`, `percent = 0`, `state = "unknown"`, no time estimates; peripherals may remain |
| No UPower | The same display defaults, with `peripherals = {}` |

## How do I…

### Show a battery label

`before` stands in for the first push, so `fn` needs no `nil` guard.

```lua
text {
    foreground = mantle.battery:map(function(battery)
        return (battery.present and battery.percent <= 15) and "#F38BA8" or "#CDD6F4"
    end, "#CDD6F4"),
    content = mantle.battery:map(function(battery)
        if not battery.present then
            return ""
        end
        return string.format("%d%%%s", battery.percent, battery.state == "charging" and " +" or "")
    end, "…"),
}
```

See also: [Battery indicator](../cookbook/battery.md) recipe; [power](power.md) for on-battery and power draw.
