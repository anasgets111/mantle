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

| Field | Read from | Interval |
| :--- | :--- | :--- |
| `cpu_percent` | `/proc/stat`'s `cpu` line, the delta between two reads | `cpu_interval` |
| `ram_percent`, `swap_percent` | `/proc/meminfo` | `ram_interval` |
| `temp_cores` | hwmon `k10temp` `Tccd*` or `coretemp` `Core *` sensors, else that chip's first sensor, else `acpitz`'s | `temp_interval` |
| `temp_gpu` | The first sensor of hwmon `amdgpu`, `nouveau` or `nvidia` | `temp_interval` |

The chips are picked once, when `sysinfo` starts; a driver loaded later needs a Supervisor
restart. A reading pushes only when it changed a field. Intervals live in the Supervisor, so they
outlast reloads until the next `configure`.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `sysinfo` stays `nil` | Nothing is read until `configure`. With only `temp_interval` set on a machine without sensors, every reading equals the defaults and nothing pushes |
| `ram_percent` stays `0` while `cpu_percent` moves | Each field updates on its own interval, and an unset one is `0` (off). Set `ram_interval` too |
