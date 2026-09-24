```lua
button {
    on_wheel = function(_, steps)
        local brightness = mantle.brightness:get()
        if brightness then
            local percent = brightness.percent + math.floor(steps * 5) -- math.floor returns an integer
            mantle.brightness:invoke("set", math.max(1, math.min(100, percent)))
        end
    end,
    children = {
        text {
            content = mantle.brightness:map(function(brightness)
                return brightness and string.format("☀ %d%%", brightness.percent) or ""
            end),
        },
    },
}
```

<!-- reference -->

## Backend

The sysfs backlight is chosen once (firmware, then platform, then raw) and watched through udev,
with a 30 s poll as fallback. Writes go through logind's `Session.SetBrightness`, so they need an
active session. It reads the requested level, not `actual_brightness`, and stays `nil` without a
backlight; external monitors are not covered.

## How do I…

| Task | Answer |
| :--- | :--- |
| Change brightness with the scroll wheel | `on_wheel` plus `:get()` and `:invoke`, as in the example above |
