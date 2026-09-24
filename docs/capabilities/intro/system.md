```lua
text {
    content = mantle.system:map(function(system)
        return system and os.date("%a %H:%M", system.time) or ""
    end),
}
```

<!-- reference -->

## Backend

Ticks once a second, aligned to the wall-clock second at start only; an NTP step or resume is not
re-aligned. `time` is epoch seconds; `monotonic` counts seconds from the capability's start and
excludes suspend.

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a clock | `os.date` over `mantle.system.time`, as in the example above |

See also: [Clock bar](../cookbook/clock-bar.md) recipe.
