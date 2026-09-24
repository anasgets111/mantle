```lua
text {
    content = mantle.system:map(function(system)
        return system and os.date("%a %H:%M", system.time) or ""
    end),
}
```

<!-- reference -->

## Backend

The Supervisor's own clocks; nothing external. The first push lands on the next wall-clock second,
up to 1 s after the first read, then one push a second. That alignment happens once: after an NTP
step or a resume, pushes land mid-second until the Supervisor restarts.

See also: [Clock bar](../cookbook/clock-bar.md) recipe.
