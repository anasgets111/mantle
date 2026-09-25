```lua
mantle.system:configure({ interval = 60 }) -- the text below shows no seconds

text {
    content = mantle.system:map(function(system)
        return system and os.date("%a %H:%M", system.time) or ""
    end),
}
```

<!-- reference -->

## Backend

The Supervisor's own clocks; nothing external. Pushes land on wall-clock multiples of `interval`
since the epoch (default `1`): `60` lands on each minute's `:00`; `3600` on UTC hours, not local
ones in a half-hour timezone. A push due during suspend lands on resume, and a clock step (NTP,
`settimeofday`) pushes at once and re-aligns. `0` stops pushes; `time` and `monotonic` keep their
last values. The interval lives in the Supervisor, so it outlasts reloads until the next
`configure`, and every reader shares it.

See also: [Clock bar](../cookbook/clock-bar.md) recipe.
