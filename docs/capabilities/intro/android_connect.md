```lua
list {
    items = mantle.android_connect:map(function(android)
        return android.devices
    end),
    key = function(device) return device.id end,
    each = function(device)
        local battery = device.battery and string.format(" %d%%", device.battery) or ""
        return rect {
            on_click = function() mantle.android_connect:ring(device.id) end,
            children = { text { content = device.name .. battery } },
        }
    end,
}
```

<!-- reference -->

## Backend

`kdeconnect-service` (`io.github.hepp3n.kdeconnect`) on the session bus.

| Contract | Behavior |
| :--- | :--- |
| State | Tracked via `io.github.hepp3n.kdeconnect.Daemon`. Without a running daemon, `available` is `false`, `devices` is empty, and every action is a no-op. When the daemon appears, the capability asks it to broadcast its identity so nearby devices answer |
| Security | `send_files` takes absolute paths of existing regular files; one relative path, missing file or directory rejects the whole batch |
| Daemon files | `clipboard_auto_share` is read from, and `set_clipboard_auto_share` rewrites only the `autoShare` line of, `~/.config/kdeconnect/<id>/kdeconnect_clipboard/config`. An id holding anything but letters, digits, `_` and `-` is refused. `commands` is read from, and `add_command` and `remove_command` rewrite, `~/.config/kdeconnect/runcommand.json`; a file that does not parse is left untouched |

### Installing the daemon

Only `kdeconnect-service` is needed, not the COSMIC applet. Install it with Rust and the `libpulse`
and `dbus` development headers:

```sh
cargo install --git https://github.com/cosmic-utils/kdeconnect kdeconnect-service
```

Run the same command to update: it rebuilds only when upstream has new commits. Leave out
`--locked`: upstream's `Cargo.lock` lags its own crate versions, so cargo refuses it.

Start it from the compositor, so it has the Wayland socket that clipboard sync needs. In niri's
`config.kdl`:

```kdl
spawn-sh-at-startup "~/.cargo/bin/kdeconnect-service"
```

In Hyprland's `hyprland.conf`:

```ini
exec-once = ~/.cargo/bin/kdeconnect-service
```

Open TCP and UDP ports 1714–1764 in the firewall, and stop KDE's `kdeconnectd` if it runs: both
bind port 1716.

## How do I…

### Find my phone

Call `ring` with the target device's `id`:

```lua,fragment
mantle.android_connect:ring(device.id)
```

### Send clipboard text

```lua,fragment
mantle.android_connect:send_clipboard(device.id, "Hello from desktop")
```

### Show why browsing failed

`browse` reports no error itself. When the mount fails, `browse_error` names the cause until the
next `browse` or a successful mount:

```lua,fragment
text { content = device.browse_error or "" }
```

### Stop syncing the clipboard to a device

```lua,fragment
mantle.android_connect:set_clipboard_auto_share(device.id, not device.clipboard_auto_share)
```

### Offer a command to my phone

Any paired device can then run it on this PC, as `sh -c`, from its Run command screen:

```lua,fragment
mantle.android_connect:add_command("Lock screen", "loginctl lock-session")
```
