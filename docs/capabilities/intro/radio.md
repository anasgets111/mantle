Toggle airplane mode with one call, and show which radios are blocked:

```lua
rect {
    on_click = function()
        local radio = mantle.radio:get()
        if radio then
            local blocked = false
            for _, r in ipairs(radio.radios) do
                blocked = blocked or r.soft_blocked
            end
            mantle.radio:set_all_blocked(not blocked)
        end
    end,
    children = {
        text {
            content = mantle.radio:map(function(radio)
                if not radio then return "" end
                for _, r in ipairs(radio.radios) do
                    if r.hard_blocked then return "airplane (switch)" end
                end
                return "radios"
            end),
        },
    },
}
```

rfkill is the airplane layer: `set_blocked` and `set_all_blocked` flip the kernel's soft block, which switches the radios themselves off. [`network.set_wifi_enabled`](network.md) and [`bluetooth.set_enabled`](bluetooth.md) are separate and unchanged. There is no airplane field: compute it from `radios`.

<!-- reference -->

## Backend

| Contract | Behavior |
| :--- | :--- |
| Source | `/dev/rfkill` only: one event per existing device at start, then live changes. No polling |
| Kinds | One entry per kind with a device, ordered by kernel type id. A kind counts as blocked when any of its devices is. Unknown kernel types are skipped |
| No device node | Stays `nil` for good; actions are logged and ignored |
| No radios | Stays `nil` until one appears: the kernel sends nothing to read |
| Writes | One `CHANGE_ALL` request to the same fd; the kernel also applies it to radios plugged in later. State updates when the kernel reports the change |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `set_blocked(kind, false)` leaves `hard_blocked = true` | A hardware switch overrides software. Only the switch clears it; show it instead |
| A session without a seat cannot write `/dev/rfkill` | State is still reported; actions are logged and ignored |
