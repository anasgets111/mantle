```lua
mantle.sysinfo:configure({ cpu_interval = 2, ram_interval = 5 })

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
| `temp_gpu` | The first sensor of hwmon `amdgpu`, `nouveau`, `nvidia`, `i915` or `xe` | `temp_interval` |
| `disks` | `lsblk --json --bytes` block devices and mounted partitions (`FSUSED` and `FSSIZE`) | `disk_interval` |
| `gpu` | `nvtop -s`, `nvidia-smi` or DRM sysfs | `gpu_interval` |
| `net_rx_bytes_sec`, `net_tx_bytes_sec` | `/proc/net/dev` non-loopback interface counter deltas | `net_interval` |

The chips are picked once, when `sysinfo` starts; a driver loaded later needs a Supervisor
restart. A reading pushes only when it changed a field. Intervals live in the Supervisor, so they
outlast reloads until the next `configure`. Each read runs on the wall-clock second, first at the
next one after `configure`, so it lands with `mantle.system.time`'s tick and the two can share one
layout pass.

## How do I…

### Show GPU, disk, and network transfer rates

```lua
mantle.sysinfo:configure({ disk_interval = 30, gpu_interval = 2, net_interval = 1 })

text {
    content = mantle.sysinfo:map(function(sysinfo)
        if sysinfo == nil then
            return ""
        end
        local gpu_str = sysinfo.gpu and sysinfo.gpu.util_percent and
            string.format("GPU %d%%", sysinfo.gpu.util_percent) or "GPU --"
        local disk = sysinfo.disks[1]
        local disk_str = disk and string.format("Disk %d%%", disk.percent) or "Disk --"
        local down_kib = sysinfo.net_rx_bytes_sec // 1024
        local up_kib = sysinfo.net_tx_bytes_sec // 1024
        return string.format("%s  %s  ↓%d KiB/s ↑%d KiB/s", gpu_str, disk_str, down_kib, up_kib)
    end),
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `sysinfo` stays `nil` | Nothing is read until `configure`. With only `temp_interval` set on a machine without sensors, every reading equals the defaults and nothing pushes |
| `ram_percent` stays `0` while `cpu_percent` moves | Each field updates on its own interval, and an unset one is `0` (off). Set `ram_interval` too |
| `gpu` stays `nil` | No supported GPU tool (`nvtop`, `nvidia-smi`) or DRM sysfs was found, or `gpu_interval` is unset |
| `disks` stays empty | `lsblk` is missing or returned no mounted partitions, or `disk_interval` is unset |
| `net_rx_bytes_sec` stays `0` | Rate calculation needs two consecutive samples. Set `net_interval` to `1` or higher |
| Network rates look high with bridges or tunnels | The counters include every non-loopback interface, including virtual ones; the same traffic may cross more than one |

See also: [system](system.md) for clock time; [battery](battery.md) for charge and power; [network](network.md) for Wi-Fi and connectivity.
