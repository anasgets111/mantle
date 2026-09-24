```lua
text {
    content = mantle.battery:map(function(battery)
        local seconds = battery and battery.present and battery.time_to_empty
        if not seconds then
            return ""
        end
        return string.format("%d:%02d left", seconds // 3600, seconds % 3600 // 60)
    end),
}
```

<!-- reference -->

## Backend

UPower's `DisplayDevice`, read-only. Without UPower it reads `present = false` and `state`
`"Unknown"`.

## How do I…

### Show a battery label with every `nil` state handled

```lua
text {
    foreground = mantle.battery:map(function(battery)
        return (battery and battery.present and battery.percent <= 15) and "#F38BA8" or "#CDD6F4"
    end),
    content = mantle.battery:map(function(battery)
        if battery == nil then
            return "…"
        elseif not battery.present then
            return ""
        end
        return string.format("%d%%%s", battery.percent, battery.state == "Charging" and " +" or "")
    end),
}
```

See also: [Battery indicator](../cookbook/battery.md) recipe; [power](power.md) for on-battery and power draw.
