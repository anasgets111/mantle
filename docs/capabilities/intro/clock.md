```lua
mantle.clock:configure({ interval = 60 }) -- the text below shows no seconds

text {
    content = mantle.clock:map(function(clock)
        return os.date("%a %H:%M", clock.time)
    end, ""),
}
```

<!-- reference -->

## Reading monotonic time now

`monotonic` is `CLOCK_MONOTONIC`: seconds since boot as a number with a fraction, the same clock
in every process, and it excludes suspend. The field holds the reading from the last push, up to
`interval` stale. `mantle.clock:monotonic()` reads the clock at that call, works before the first
push and sends nothing:

```lua,fragment
local armed_at = mantle.clock:monotonic()
local function elapsed() return mantle.clock:monotonic() - armed_at end
```

## Backend

The Supervisor's own clocks; nothing external. Pushes land on wall-clock multiples of `interval`
since the epoch (default `1`): `60` lands on each minute's `:00`; `3600` on UTC hours, not local
ones in a half-hour timezone. A push due during suspend lands on resume, and a clock step (NTP,
`settimeofday`) pushes at once and re-aligns. `0` stops pushes; the `time` and `monotonic` fields keep
their last values, but `monotonic()` stays live. The interval lives in the Supervisor, so it outlasts reloads until the next
`configure`, and every reader shares it.

See also: [Clock bar](../cookbook/clock-bar.md) recipe.
