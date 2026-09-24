```lua
mantle.sysinfo:invoke("configure", { cpu_interval = 2, ram_interval = 5 })

text {
    content = mantle.sysinfo:map(function(sysinfo)
        if sysinfo == nil then
            return ""
        end
        return string.format("CPU %d%%  RAM %d%%", sysinfo.cpu_percent, sysinfo.ram_percent)
    end),
}
```

<!-- reference -->

## Backend

CPU from `/proc/stat`, RAM and swap from `/proc/meminfo`, temperatures from `/sys/class/hwmon/`
chips chosen by name preference: `k10temp`, then `coretemp`, else `acpitz` for the CPU; `amdgpu`,
`nouveau` or `nvidia` for the GPU. Each sample has its own interval; all are `0` (off) until `configure`. The
first reading lands one interval later (CPU: two).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show CPU and memory use | `configure` once at top level, then map, as in the example above |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `sysinfo` stays `nil` | It reads nothing until `mantle.sysinfo:invoke("configure", { cpu_interval = 2 })` |
