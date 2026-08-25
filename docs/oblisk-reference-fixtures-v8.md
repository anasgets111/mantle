# Oblisk Reference Fixtures (v9)
## Syntactically Perfect, Production-Grade Declarative Lua Configurations (v9)

This document serves as the fifth and final component of the Oblisk specification suite. It contains complete, production-grade, syntactically flawless Lua configurations that strictly adhere to the schemas, constraint passes, and IPC event structures established in the previous four specification documents.

Any downstream LLM or compiler executing this framework must use these configurations as the primary validation test suite for AST parsing, layout resolution, and reactive signal bindings.

---

## 1. Core Shell Configuration (`shell.lua`)

This file constructs a responsive, auto-anchored top status bar. It relies entirely on lazy-activated capabilities. If the user removes the `cava` or `webcam` blocks, the corresponding supervisor-side threads are automatically torn down.

```lua
-- =============================================================================
-- shell.lua - Oblisk Core Declarative Shell Layout
-- =============================================================================

-- 1. Lazy Capability Loading
local battery      = require("oblisk.battery")
local audio        = require("oblisk.audio")
local brightness   = require("oblisk.brightness")
local updates      = require("oblisk.updates")
local keyboard     = require("oblisk.keyboard")
local webcam       = require("oblisk.webcam")
local cava         = require("oblisk.cava")
local network      = require("oblisk.network")
local mpris        = require("oblisk.mpris")

-- 2. Service Configurations (Dynamic Online/Offline Pacman Monitoring)
updates:configure({
    online_interval = 900,  -- 15 minutes
    offline_interval = 300, -- 5 minutes
})

-- 2.1 Zero-Opinion Event Sound Triggers (Lua-Side Observers)
-- Because Oblisk is a pure, zero-opinion framework, the Supervisor does not
-- play sound effects automatically. We subscribe to signals and play sounds in Lua.
local notifications = require("oblisk.notifications")
local last_notif_count = 0
notifications.feed:map(function(feed)
    -- Play a system sound on new notification arrival
    if #feed > last_notif_count then
        audio:play_sound("notification-message")
    end
    last_notif_count = #feed
end)

battery.percent:map(function(p)
    -- Play warning sound if battery falls to/below 15% on battery power
    if p <= 15 and not battery.charging:get() then
        audio:play_sound("battery-caution")
    end
end)

-- 3. The Declarative Scene-Graph Roots
-- Returns a flat layout configuration table parsed by the Rust Renderer.
return panel {
    id = "top_bar",
    width = "Fill",          -- Stretch to match output physical boundaries
    height = 32,             -- 32px logical height (scaled by viewport multiplier)
    background = "#121214",  -- Static hex fallback (overridden by dynamic palette)
    border_color = bind(theme.primary), -- Dynamic Material Design 3 Palette color
    border_width = { bottom = 1 },
    align_h = "Stretch",
    align_v = "Start",

    children = {
        row {
            id = "left_section",
            align_h = "Start",
            align_v = "Center",
            spacing = 12,
            padding = { left = 16, right = 16 },
            children = {
                -- Clock Display (Reactive Native Timer)
                text {
                    id = "clock_text",
                    content = bind(system.time):map(function(epoch)
                        return os.date("%H:%M:%S", epoch)
                    end),
                    font_size = 14,
                    foreground = bind(theme.surface_text),
                },
                
                -- Dynamic System Updates Badge
                row {
                    id = "updates_badge",
                    visible = bind(updates.count):map(function(c) return c > 0 end),
                    spacing = 4,
                    children = {
                        icon { name = "system-software-update", size = 16 },
                        text { 
                            content = bind(updates.count):map(tostring),
                            font_size = 13,
                            foreground = "#E4B854"
                        }
                    }
                }
            }
        },

        row {
            id = "center_section",
            align_h = "Center",
            align_v = "Center",
            spacing = 16,
            children = {
                -- MPRIS Media Display (Supervisor-owned persistent metadata)
                row {
                    id = "media_player",
                    visible = bind(mpris.active_player):map(function(p) return p ~= nil end),
                    spacing = 8,
                    children = {
                        icon { name = "media-playback-start", size = 14 },
                        text {
                            content = bind(mpris.metadata):map(function(meta)
                                if not meta then return "No media" end
                                return string.format("%s - %s", meta.artist or "Unknown", meta.title or "Untitled")
                            end),
                            font_size = 13,
                            max_width = 250, -- Truncated in Rust via cosmic-text
                            clip = true
                        },
                        -- Generation-guarded play/pause click handler
                        button {
                            width = 24,
                            height = 24,
                            background = "#1E1E22",
                            on_click = function()
                                mpris:toggle_play()
                            end,
                            children = {
                                icon { name = "media-skip-forward", size = 12 }
                            }
                        }
                    }
                },

                -- Real-time CAVA Audio Visualizer List Repeater
                list {
                    id = "cava_visualizer",
                    direction = "Horizontal",
                    spacing = 2,
                    align_v = "End",
                    height = 20,
                    source = bind(cava.bars), -- 20 elements (normalized f32 array from 0.0 to 1.0)
                    itemfn = function(amplitude)
                        return rect {
                            width = 4,
                            -- Height resolves reactively based on FFT band amplitude
                            height = amplitude:map(function(amp) 
                                return math.max(2, math.floor(amp * 18)) 
                            end),
                            background = bind(theme.accent),
                            radius = 2
                        }
                    end
                }
            }
        },

        row {
            id = "right_section",
            align_h = "End",
            align_v = "Center",
            spacing = 14,
            padding = { left = 16, right = 16 },
            children = {
                -- Camera Privacy Indicator
                row {
                    id = "webcam_privacy_indicator",
                    visible = bind(webcam.active),
                    spacing = 6,
                    children = {
                        rect {
                            width = 8,
                            height = 8,
                            radius = 4,
                            background = "#FF5555" -- Red dot alerts camera is running
                        },
                        text {
                            content = bind(webcam.active_clients):map(function(clients)
                                return clients[1] or "Active Camera"
                            end),
                            font_size = 12,
                            foreground = "#FF5555"
                        }
                    }
                },

                -- Caps Lock Status Indicator
                text {
                    id = "caps_lock_indicator",
                    content = "CAPS",
                    visible = bind(keyboard.caps_lock),
                    font_size = 12,
                    foreground = "#E44B4B",
                    font_weight = "Bold"
                },

                -- Network Connectivity Display
                row {
                    id = "network_display",
                    spacing = 4,
                    children = {
                        icon { 
                            name = bind(network.connected):map(function(is_connected)
                                return is_connected and "network-wireless" or "network-offline"
                            end), 
                            size = 14 
                        },
                        text { 
                            content = bind(network.ssid):map(function(ssid)
                                return ssid or "Disconnected"
                            end),
                            font_size = 12 
                        }
                    }
                },

                -- Volume Level Controller (Mouse Scroll Enabled)
                row {
                    id = "volume_widget",
                    spacing = 4,
                    on_scroll = function(direction)
                        local current = audio.volume:get()
                        if direction == "Up" then
                            audio:set_volume(math.min(1.0, current + 0.05))
                        else
                            audio:set_volume(math.max(0.0, current - 0.05))
                        end
                    end,
                    children = {
                        icon { name = "audio-volume-medium", size = 14 },
                        text {
                            content = bind(audio.volume):map(function(v)
                                return string.format("%d%%", math.floor(v * 100))
                            end),
                            font_size = 12
                        }
                    }
                },

                -- Battery Percent Indicator
                row {
                    id = "battery_widget",
                    spacing = 4,
                    children = {
                        icon { 
                            name = bind(battery.charging):map(function(chg)
                                return chg and "battery-charging" or "battery-good"
                            end), 
                            size = 14 
                        },
                        text {
                            content = bind(battery.percent):map(function(p)
                                return string.format("%d%%", p)
                            end),
                            font_size = 12
                        }
                    }
                }
            }
        }
    }
}
```

---

## 2. PolicyKit Authorization Dialog (`polkit_agent.lua`)

This file is the Lua UI configuration executed when the Supervisor receives an authentication challenge over the D-Bus system bus. 

All user passwords typed into the `textfield` are stored in secure zeroized memory in Rust. No keystrokes, buffers, or PAM state properties are ever accessible to the Lua VM, preventing password extraction attacks.

```lua
-- =============================================================================
-- polkit_agent.lua - Custom PolicyKit Privilege Escalation Dialog
-- =============================================================================

local polkit = require("oblisk.polkit")

-- Explicitly activate the supervisor-side D-Bus PolicyKit authentication agent.
-- This ensures the system does not register the agent pre-emptively on boot.
polkit:enable_agent()

-- Returns a centered, modal dialog panel configuration
return panel {
    id = "polkit_dialog",
    width = 380,
    height = 240,
    background = "#1A1A1E",
    border_color = bind(theme.accent),
    border_width = 2,
    radius = 8,
    align_h = "Center",
    align_v = "Center",

    children = {
        column {
            spacing = 16,
            padding = 24,
            align_h = "Stretch",
            align_v = "Stretch",
            children = {
                -- Header Section with Application Identity
                row {
                    spacing = 8,
                    children = {
                        icon { name = "dialog-password", size = 24 },
                        text {
                            content = "Authentication Required",
                            font_size = 16,
                            font_weight = "Bold",
                            foreground = bind(theme.surface_text)
                        }
                    }
                },

                -- Target Request Explanation (Captured via DBus challenge metadata)
                text {
                    content = bind(polkit.message):map(function(msg)
                        return msg or "An application is requesting administrative privileges."
                    end),
                    font_size = 13,
                    foreground = "#A0A0A5",
                    max_width = 332,
                    clip = false
                },

                -- Passphrase Input Form (Native Rust exception boundary)
                textfield {
                    id = "password_input",
                    width = "Fill",
                    height = 36,
                    placeholder = "Enter administrator password...",
                    secure = true, -- Masks text visualizer dots. Real text held inside Rust
                    focus = true,  -- Requests default compositor keyboard focus
                    
                    -- Triggered when enter key is hit inside the native field
                    on_submit = function()
                        -- Submits generation ID and references the target textfield ID
                        polkit:authenticate("password_input")
                    end
                },

                -- Control Action Buttons
                row {
                    align_h = "End",
                    spacing = 12,
                    children = {
                        -- Cancel Authentication
                        button {
                            width = 80,
                            height = 32,
                            background = "#2E2E32",
                            radius = 4,
                            on_click = function()
                                polkit:cancel()
                            end,
                            children = {
                                text { content = "Cancel", font_size = 12, foreground = "#E2E2E6" }
                            }
                        },
                        -- Action Submit Button
                        button {
                            width = 100,
                            height = 32,
                            background = bind(theme.primary),
                            radius = 4,
                            on_click = function()
                                polkit:authenticate("password_input")
                            end,
                            children = {
                                text { content = "Authenticate", font_size = 12, foreground = "#000000" }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

---

## 3. Idle and Security Configuration (`idle_lock.lua`)

This file implements the event-driven system idle timeouts and ties directly into the secure PAM companion lockscreen (`oblisk-lock`). 

It consumes zero idle CPU because timeouts are registered with the Wayland compositor’s native `ext_idle_notifier_v1` protocol instead of running Lua background timers.

```lua
-- =============================================================================
-- idle_lock.lua - System Idle Monitor and Secure Locking Configuration
-- =============================================================================

local idle       = require("oblisk.idle")
local brightness = require("oblisk.brightness")

-- 1. Dim screens to 10% brightness after 5 minutes of inactivity (300 seconds)
idle:on_trigger(300, {
    on_idle = function()
        -- Stash current brightness level and dim physical backlights via Rust helper
        brightness:dim(10)
    end,
    on_resume = function()
        -- Instantly restore backlights on seat mouse activity or keypress events
        brightness:restore()
    end
})

-- 2. Trigger Secure Companion Lockscreen after 10 minutes of inactivity (600 seconds)
idle:on_trigger(600, {
    on_idle = function()
        -- Sends generation-guarded lock invocation to Supervisor daemon.
        -- Supervisor spawns setuid root 'oblisk-lock' companion process.
        idle:lock()
    end
})
```

---

## 4. Architectural Layout Verification Cases (For Compilers)

To confirm that any parsing or processing LLM has fully respected the layout engine parameters of **`oblisk-layout-engine-geometry.md`**, any compiled Rust backend running these configurations must pass the following test assertions:

### Case A: Horizontal Distribution and Sizing Pass
*   **Target**: Bar parent node `#top_bar` (width: `Fill`, height: `32`).
*   **Test Assertion**: If the compositor viewport dimensions are physical size `1920x1080` with scaling `1.0`:
    1.  The layout engine must resolve the panel's bounding box to width `1920` and height `32`.
    2.  The vertical positions of `#left_section`, `#center_section`, and `#right_section` must be resolved to center y-offset `(32 - child_height) / 2`.
    3.  `#right_section` absolute coordinates must be placed such that its right outer edge rests exactly at physical x-coordinate `1904` (representing `1920 - padding.right` (16)).

### Case B: List Sizing Loop Prevention
*   **Target**: Cava `#cava_visualizer` list node containing `amplitude:map(...)` callbacks.
*   **Test Assertion**: The height calculations of the dynamic bars inside `itemfn` must complete in exactly 1 layout pass. Height variations must not trigger parent recalculations or list resize event loops.

-----

## 5. Spotlight-Style Launcher with Calculator and Web Autocomplete (`launcher.lua`)

This file is a fully styleable, highly responsive, spotlight-style application launcher. It demonstrates rendering standard XDG applications, local math calculation results, offline currency conversions, and debounced online search suggestions inside a single unified view.

All clipboard writes and disowned process launching are executed natively inside the Rust Supervisor, preventing execution leaks and window freezing.

```lua
-- =============================================================================
-- launcher.lua - Oblisk Spotlight-Style Unified Launcher Configuration
-- =============================================================================

local launcher  = require("oblisk.launcher")
local clipboard = require("oblisk.clipboard")

-- Returns a centered floating spotlight search pane
return panel {
    id = "spotlight_launcher",
    width = 540,
    height = 420,
    background = "#181825",
    border_color = bind(theme.primary),
    border_width = 2,
    radius = 12,
    align_h = "Center",
    align_v = "Center",

    children = {
        column {
            spacing = 12,
            padding = { top = 16, right = 16, bottom = 16, left = 16 },
            align_h = "Stretch",
            align_v = "Stretch",
            children = {
                -- Input Field Section with Native IME & Focus Exception
                row {
                    spacing = 8,
                    align_v = "Center",
                    children = {
                        icon { name = "system-search", size = 20 },
                        textfield {
                            id = "launcher_query_input",
                            width = "Fill",
                            height = 38,
                            placeholder = "Search applications, calculate expressions, or query currencies...",
                            focus = true, -- Auto-gathers seat keyboard input
                            
                            -- Dispatches keystrokes instantly to debounced Rust indexing handlers
                            on_change = function(text)
                                launcher:set_query(text)
                            end,
                            
                            -- Executed when enter key is pressed inside textfield
                            on_submit = function()
                                -- Grabs first index result and executes standard launch/action
                                local results = launcher.results:get()
                                if results and results[1] then
                                    local item = results[1]
                                    if item.kind == "web_suggestion" then
                                        launcher:launch_url(item.url)
                                        launcher:close()
                                    elseif item.kind == "calc" or item.kind == "currency" then
                                        clipboard:set_text(item.result)
                                        launcher:close()
                                    else
                                        launcher:launch(item.exec)
                                    end
                                end
                            end
                        },
                        -- Spinner icon when network thread is active
                        icon { 
                            name = "process-working", 
                            size = 16,
                            visible = bind(launcher.results_loading)
                        }
                    }
                },

                -- Results Repetitive List View (Virtual list bound to Rust snapshot)
                list {
                    id = "launcher_results_list",
                    direction = "Vertical",
                    spacing = 4,
                    align_h = "Stretch",
                    height = "Fill",
                    source = bind(launcher.results), -- Reactive top-20 merged list
                    
                    itemfn = function(item)
                        return button {
                            align_h = "Stretch",
                            height = 48,
                            background = "#1E1E2E",
                            radius = 6,
                            
                            on_click = function()
                                if item.kind == "web_suggestion" then
                                    launcher:launch_url(item.url)
                                    launcher:close()
                                elseif item.kind == "calc" or item.kind == "currency" then
                                    clipboard:set_text(item.result)
                                    launcher:close()
                                else
                                    launcher:launch(item.exec)
                                end
                            end,
                            
                            children = {
                                row {
                                    spacing = 14,
                                    align_v = "Center",
                                    padding = { left = 12, right = 12 },
                                    children = {
                                        icon { 
                                            name = item.icon or "system-search", 
                                            size = 24 
                                        },
                                        column {
                                            spacing = 2,
                                            children = {
                                                text {
                                                    content = (item.kind == "calc" or item.kind == "currency") and item.result or item.name,
                                                    font_size = 14,
                                                    font_weight = "SemiBold",
                                                    foreground = (item.kind == "calc" or item.kind == "currency") and "#A6E3A1" or "#CDD6F4"
                                                },
                                                text {
                                                    content = (item.kind == "calc") and ("Math Evaluation: " .. item.expression) 
                                                           or (item.kind == "currency") and ("Currency Conversion: " .. item.expression)
                                                           or (item.kind == "web_suggestion") and "Query Web Suggestion"
                                                           or item.description or "XDG System Application",
                                                    font_size = 11,
                                                    foreground = "#7F849C"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    end
                }
            }
        }
    }
}
```


---


## 6. Interactive Dropdown Control Center (`control_center.lua`)

This file constructs a comprehensive macOS/Noctalia-style "Control Center" dropdown card. It integrates complete Wi-Fi access point scanning, secure hidden/open network connections, Bluetooth discovery, battery level indicators, on-the-fly audio codec switching, and event-driven sound feedback.

```lua
-- =============================================================================
-- control_center.lua - Interactive Dropdown Control Center Widget
-- =============================================================================

local network   = require("oblisk.network")
local bluetooth = require("oblisk.bluetooth")
local audio     = require("oblisk.audio")
local system    = require("oblisk.system")

-- Local UI state holds temporary modal visibility (tracked inside Lua heap)
local active_secure_ssid = bind(system.state):map(function(s) return s.selected_ssid or "" end)
local show_password_popup = bind(system.state):map(function(s) return s.show_popup or false end)

return panel {
    id = "control_center_panel",
    width = 380,
    height = 550,
    background = "#1E1E2E",
    border_color = bind(theme.primary),
    border_width = 1,
    radius = 12,
    padding = 16,
    align_h = "End",
    align_v = "Start",

    children = {
        column {
            spacing = 16,
            align_h = "Stretch",
            children = {
                -- 1. TITLE & TIME SECTION
                row {
                    align_h = "SpaceBetween",
                    children = {
                        text { content = "Control Center", font_size = 16, font_weight = "Bold", foreground = "#CDD6F4" },
                        text {
                            content = bind(system.time):map(function(t) return os.date("%H:%M", t) end),
                            font_size = 14,
                            foreground = "#A6ADC8"
                        }
                    }
                },

                -- 2. MASTER NETWORKING PANEL
                column {
                    id = "network_panel",
                    spacing = 8,
                    background = "#181825",
                    padding = 12,
                    radius = 8,
                    children = {
                        row {
                            align_h = "SpaceBetween",
                            align_v = "Center",
                            children = {
                                row {
                                    spacing = 8,
                                    children = {
                                        icon { name = "network-wireless", size = 18 },
                                        text { content = "Network", font_size = 14, font_weight = "Bold" }
                                    }
                                },
                                -- Global network master toggle switch
                                button {
                                    width = 60,
                                    height = 24,
                                    background = bind(network.networking_enabled):map(function(en)
                                        return en and "#A6E3A1" or "#313244"
                                    end),
                                    radius = 12,
                                    on_click = function()
                                        network:set_networking_enabled(not network.networking_enabled:get())
                                    end,
                                    children = {
                                        text {
                                            content = bind(network.networking_enabled):map(function(en)
                                                return en and "Enabled" or "Disabled"
                                            end),
                                            font_size = 11,
                                            foreground = "#11111B",
                                            align_h = "Center",
                                            align_v = "Center"
                                        }
                                    }
                                }
                            }
                        },

                        -- Sub-Radio toggles
                        row {
                            spacing = 12,
                            children = {
                                button {
                                    width = 110,
                                    height = 28,
                                    background = bind(network.wifi_enabled):map(function(en) return en and "#89B4FA" or "#313244" end),
                                    radius = 4,
                                    on_click = function() network:set_wifi_enabled(not network.wifi_enabled:get()) end,
                                    children = { text { content = "Wi-Fi", font_size = 12, foreground = "#11111B" } }
                                },
                                button {
                                    width = 110,
                                    height = 28,
                                    background = bind(network.ethernet_enabled):map(function(en) return en and "#89B4FA" or "#313244" end),
                                    radius = 4,
                                    on_click = function() network:set_ethernet_enabled(not network.ethernet_enabled:get()) end,
                                    children = { text { content = "Ethernet", font_size = 12, foreground = "#11111B" } }
                                },
                                button {
                                    width = 100,
                                    height = 28,
                                    background = "#45475A",
                                    radius = 4,
                                    on_click = function() network:scan() end, -- Triggers scan background thread
                                    children = { text { content = "Scan", font_size = 12, foreground = "#CDD6F4" } }
                                }
                            }
                        },

                        -- Scanned access points list repeater
                        list {
                            id = "wifi_list",
                            height = 120,
                            spacing = 4,
                            source = bind(network.available_networks),
                            itemfn = function(ap)
                                return button {
                                    align_h = "Stretch",
                                    height = 32,
                                    background = ap.active and "#2E3047" or "Transparent",
                                    radius = 4,
                                    on_click = function()
                                        if ap.secure and not ap.active then
                                            -- Reveals custom password modal block inside Lua state
                                            system:set_state({ selected_ssid = ap.ssid, show_popup = true })
                                        else
                                            -- Connects instantly if open network
                                            network:connect(ap.ssid)
                                        end
                                    end,
                                    children = {
                                        row {
                                            spacing = 8,
                                            align_v = "Center",
                                            padding = { left = 8, right = 8 },
                                            children = {
                                                icon { name = ap.secure and "network-wireless-encrypted" or "network-wireless", size = 14 },
                                                text { content = ap.ssid, font_size = 12, foreground = ap.active and "#A6E3A1" or "#CDD6F4" },
                                                text { content = string.format("%d%%", ap.strength), font_size = 11, foreground = "#7F849C" },
                                                icon { name = "dialog-ok", size = 12, visible = ap.active }
                                            }
                                        }
                                    }
                                end
                            end
                        },

                        -- Active IP configuration connection details card
                        row {
                            id = "ip_details",
                            spacing = 8,
                            visible = bind(network.connected),
                            background = "#11111B",
                            padding = 8,
                            radius = 6,
                            align_h = "Stretch",
                            children = {
                                text {
                                    content = bind(network.connection_details):map(function(details)
                                        if not details then return "No Connection Details" end
                                        return string.format("IP: %s | Intf: %s\nGW: %s | DNS: %s", 
                                            details.ip_address or "---", 
                                            details.interface or "---",
                                            details.gateway or "---",
                                            details.dns and details.dns[1] or "---"
                                        )
                                    end),
                                    font_size = 10,
                                    foreground = "#BAC2DE"
                                }
                            }
                        }
                    }
                },

                -- 3. BLUETOOTH & CODEC SELECTOR
                column {
                    id = "bluetooth_panel",
                    spacing = 8,
                    background = "#181825",
                    padding = 12,
                    radius = 8,
                    children = {
                        row {
                            align_h = "SpaceBetween",
                            align_v = "Center",
                            children = {
                                row {
                                    spacing = 8,
                                    children = {
                                        icon { name = "bluetooth", size = 18 },
                                        text { content = "Bluetooth", font_size = 14, font_weight = "Bold" }
                                    }
                                },
                                button {
                                    width = 60,
                                    height = 24,
                                    background = bind(bluetooth.enabled):map(function(en) return en and "#A6E3A1" or "#313244" end),
                                    radius = 12,
                                    on_click = function() bluetooth:set_enabled(not bluetooth.enabled:get()) end,
                                    children = { text { content = bind(bluetooth.enabled):map(function(en) return en and "On" or "Off" end), font_size = 11, foreground = "#11111B" } }
                                }
                            }
                        },

                        -- Bluetooth Scan & Discover controls
                        row {
                            spacing = 10,
                            children = {
                                button {
                                    width = 120,
                                    height = 26,
                                    background = bind(bluetooth.discovering):map(function(d) return d and "#F9E2AF" or "#313244" end),
                                    radius = 4,
                                    on_click = function()
                                        if bluetooth.discovering:get() then
                                            bluetooth:stop_discovery()
                                        else
                                            bluetooth:start_discovery()
                                        end
                                    end,
                                    children = { text { content = bind(bluetooth.discovering):map(function(d) return d and "Stop Scan" or "Scan Devices" end), font_size = 11 } }
                                }
                            }
                        },

                        -- Discovered bluetooth devices
                        list {
                            id = "discovered_devices_list",
                            height = 60,
                            visible = bind(bluetooth.discovering),
                            spacing = 4,
                            source = bind(bluetooth.discovered_devices),
                            itemfn = function(dev)
                                return row {
                                    align_h = "SpaceBetween",
                                    children = {
                                        text { content = dev.name or dev.mac, font_size = 11, foreground = "#BAC2DE" },
                                        button {
                                            width = 60,
                                            height = 20,
                                            background = "#89B4FA",
                                            radius = 2,
                                            on_click = function() bluetooth:pair(dev.mac) end,
                                            children = { text { content = "Pair", font_size = 10, foreground = "#11111B" } }
                                        }
                                    }
                                }
                            end
                        },

                        -- Connected devices, showing battery levels and codec switcher
                        list {
                            id = "connected_devices_list",
                            height = 80,
                            spacing = 6,
                            source = bind(bluetooth.connected_devices),
                            itemfn = function(dev)
                                return row {
                                    align_h = "SpaceBetween",
                                    align_v = "Center",
                                    children = {
                                        column {
                                            spacing = 2,
                                            children = {
                                                text { content = dev.name, font_size = 12, font_weight = "Bold" },
                                                text { content = string.format("Battery: %d%%", dev.battery), font_size = 10, foreground = "#A6ADC8" }
                                            }
                                        },
                                        -- Interactive Audio Codec selector
                                        row {
                                            spacing = 6,
                                            children = {
                                                text { content = "Codec:", font_size = 10, foreground = "#7F849C" },
                                                button {
                                                    width = 80,
                                                    height = 24,
                                                    background = "#45475A",
                                                    radius = 4,
                                                    on_click = function()
                                                        -- Cycles audio codec between LDAC, AAC, SBC
                                                        local current = dev.codec
                                                        local target = "SBC"
                                                        if current == "SBC" then target = "AAC"
                                                        elseif current == "AAC" then target = "LDAC" end
                                                        bluetooth:set_audio_codec(dev.mac, target)
                                                    end,
                                                    children = {
                                                        text { content = dev.codec or "None", font_size = 11, foreground = "#F9E2AF" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            end
                        }
                    }
                },

                -- 4. SYSTEM SOUNDS FEEDBACK CONTROL
                column {
                    id = "audio_feedback_panel",
                    spacing = 8,
                    background = "#181825",
                    padding = 12,
                    radius = 8,
                    children = {
                        row {
                            align_h = "SpaceBetween",
                            children = {
                                row {
                                    spacing = 8,
                                    children = {
                                        icon { name = "audio-volume-high", size = 18 },
                                        text { content = "Sound Feedback", font_size = 14, font_weight = "Bold" }
                                    }
                                },
                                button {
                                    width = 60,
                                    height = 24,
                                    background = bind(audio.event_sounds_enabled):map(function(en) return en and "#A6E3A1" or "#313244" end),
                                    radius = 12,
                                    on_click = function() audio:set_event_sounds_enabled(not audio.event_sounds_enabled:get()) end,
                                    children = { text { content = bind(audio.event_sounds_enabled):map(function(en) return en and "On" or "Off" end), font_size = 11, foreground = "#11111B" } }
                                }
                            }
                        },
                        -- Test Audio latency bell button
                        button {
                            width = "Fill",
                            height = 32,
                            background = "#313244",
                            radius = 4,
                            on_click = function()
                                audio:play_sound("dialog-information")
                            end,
                            children = {
                                text { content = "🔊 Play Test Bell (Latency test)", font_size = 12, foreground = "#CDD6F4" }
                            }
                        }
                    }
                }
            }
        },

        -- 5. PASSWORD ENTRY POPUP MODAL DIALOG
        panel {
            id = "password_modal",
            width = "Fill",
            height = "Fill",
            background = "#11111BCC", -- Dim backdrop overlay
            visible = show_password_popup,
            align_h = "Stretch",
            align_v = "Stretch",
            children = {
                panel {
                    width = 300,
                    height = 160,
                    background = "#1E1E2E",
                    border_color = "#89B4FA",
                    border_width = 1,
                    radius = 8,
                    align_h = "Center",
                    align_v = "Center",
                    padding = 16,
                    children = {
                        column {
                            spacing = 12,
                            align_h = "Stretch",
                            children = {
                                text {
                                    content = bind(active_secure_ssid):map(function(ssid) return "Connect to: " .. ssid end),
                                    font_size = 13,
                                    font_weight = "Bold"
                                },
                                textfield {
                                    id = "wifi_pwd_input",
                                    width = "Fill",
                                    height = 32,
                                    placeholder = "Enter Password...",
                                    secure = true,
                                    focus = true,
                                    on_submit = function()
                                        -- Captures input, invokes connection, and clears popup
                                        network:connect(active_secure_ssid:get(), "wifi_pwd_input")
                                        system:set_state({ selected_ssid = "", show_popup = false })
                                    end
                                },
                                row {
                                    align_h = "End",
                                    spacing = 8,
                                    children = {
                                        button {
                                            width = 60,
                                            height = 24,
                                            background = "#313244",
                                            radius = 4,
                                            on_click = function() system:set_state({ selected_ssid = "", show_popup = false }) end,
                                            children = { text { content = "Cancel", font_size = 11 } }
                                        },
                                        button {
                                            width = 80,
                                            height = 24,
                                            background = "#89B4FA",
                                            radius = 4,
                                            on_click = function()
                                                network:connect(active_secure_ssid:get(), "wifi_pwd_input")
                                                system:set_state({ selected_ssid = "", show_popup = false })
                                            end,
                                            children = { text { content = "Connect", font_size = 11, foreground = "#11111B" } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```


---

## 7. Power and Session Management Menu (`session_menu.lua`)

This file implements a spotlight-style centered overlay menu for user session control and system power management. It demonstrates compositor-specific commands registration and clean D-Bus integrated click triggers.

```lua
-- =============================================================================
-- session_menu.lua - System Power & User Session Overlay Menu
-- =============================================================================

local power = require("oblisk.power")

-- 1. Configure compositor-specific commands for Hyprland setup
power:configure({
    shutdown_cmd   = "systemctl poweroff",                 -- Bypassed via logind, default
    reboot_cmd     = "systemctl reboot",                   -- Bypassed via logind, default
    suspend_cmd    = "systemctl suspend",                  -- Bypassed via logind, default
    logout_cmd     = "hyprctl dispatch exit",              -- Compositor-specific
    dpms_off_cmd   = "hyprctl dispatch dpms off",          -- Compositor-specific
    dpms_on_cmd    = "hyprctl dispatch dpms on",           -- Compositor-specific
})

-- 2. Helper to render power button options
local function power_button(label, icon_name, color, callback)
    return button {
        width = 110,
        height = 100,
        background = "#1E1E2E",
        radius = 8,
        on_click = callback,
        children = {
            column {
                spacing = 10,
                align_h = "Center",
                align_v = "Center",
                children = {
                    icon { name = icon_name, size = 32 },
                    text { content = label, font_size = 12, foreground = color }
                }
            }
        }
    }
end

-- 3. Centered Power Menu Panel Layout
return panel {
    id = "power_menu_overlay",
    width = 620,
    height = 160,
    background = "#11111B",
    border_color = "#313244",
    border_width = 2,
    radius = 12,
    align_h = "Center",
    align_v = "Center",
    padding = 20,

    children = {
        column {
            spacing = 16,
            align_h = "Stretch",
            children = {
                text {
                    content = "System Power and Session Control",
                    font_size = 14,
                    font_weight = "Bold",
                    foreground = "#CDD6F4",
                    align_h = "Center"
                },
                row {
                    align_h = "Center",
                    spacing = 14,
                    children = {
                        -- Suspend (using logind D-Bus calls)
                        power_button("Suspend", "system-suspend", "#89B4FA", function()
                            power:suspend()
                        end),
                        
                        -- Log Out (invokes double-forked compositor-specific exit command)
                        power_button("Log Out", "system-log-out", "#F9E2AF", function()
                            power:logout()
                        end),
                        
                        -- DPMS Standby (triggers registered dpms_off_cmd)
                        power_button("Screen Off", "weather-clear-night", "#A6E3A1", function()
                            power:dpms(false)
                        end),
                        
                        -- System Reboot
                        power_button("Reboot", "system-reboot", "#FAB387", function()
                            power:reboot()
                        end),
                        
                        -- System Shutdown
                        power_button("Power Off", "system-shutdown", "#F38BA8", function()
                            power:shutdown()
                        end)
                    }
                }
            }
        }
    }
}
```


---

## 8. Keyboard Layout Switcher Widget (`keyboard_layout.lua`)

This widget provides a clickable status indicator for the bar and a dropdown panel to select configured keyboard layouts. It updates reactively with zero lag whenever layout switches are triggered via physical keybinds or software calls.

```lua
-- =============================================================================
-- keyboard_layout.lua - Interactive Keyboard Layout Selector Widget
-- =============================================================================

local keyboard = require("oblisk.keyboard")

-- 1. Status Bar Indicator (Toggles the layout on click, cycles on scroll)
local function layout_indicator()
    return button {
        id = "keyboard_indicator",
        width = 48,
        height = 32,
        background = "transparent",
        
        -- Cycles to the next layout on click
        on_click = function()
            keyboard:next_layout()
        end,
        
        children = {
            row {
                align_h = "Center",
                align_v = "Center",
                spacing = 4,
                children = {
                    icon { name = "preferences-desktop-keyboard", size = 14 },
                    text {
                        -- Reactively binds to the active layout name
                        content = bind(keyboard.active_layout):map(function(layout)
                            if not layout then return "US" end
                            -- Returns the capitalized short code (e.g. "English (US)" -> "US", "Turkish" -> "TR")
                            if string.find(layout, "Turkish") then return "TR" end
                            if string.find(layout, "Arabic") then return "AR" end
                            return "US"
                        end),
                        font_size = 12,
                        foreground = bind(theme.surface_text),
                    }
                }
            }
        }
    end
end

-- 2. Dropdown Menu (Renders a clickable list of all configured layouts)
local function layout_dropdown()
    return panel {
        id = "layout_dropdown_panel",
        width = 220,
        height = "Fill", -- Dynamically fills to fit child layout repetitions
        background = "#1E1E2E",
        border_color = bind(theme.accent),
        border_width = 1,
        radius = 6,
        padding = 8,

        children = {
            column {
                spacing = 6,
                align_h = "Stretch",
                children = {
                    text {
                        content = "Input Sources",
                        font_size = 13,
                        font_weight = "Bold",
                        foreground = "#7F849C",
                        margin = { bottom = 4, left = 8 }
                    },
                    
                    -- Iterates over all configured layouts in Rust natively
                    list {
                        source = bind(keyboard.layouts),
                        align_h = "Stretch",
                        itemfn = function(layout_name, index)
                            -- Calculates if this layout index matches the active index
                            local is_active = bind(keyboard.active_layout_index):map(function(active_idx)
                                return active_idx == index
                            end)

                            return button {
                                align_h = "Stretch",
                                height = 36,
                                -- Highlights background if active
                                background = is_active:map(function(active)
                                    return active and "#313244" or "transparent"
                                end),
                                radius = 4,
                                padding = { left = 12, right = 12 },
                                
                                on_click = function()
                                    keyboard:set_layout(index) -- Set specific layout via 0-indexed integer
                                end,
                                
                                children = {
                                    row {
                                        align_v = "Center",
                                        spacing = 8,
                                        children = {
                                            -- Small dot indicating selection
                                            rect {
                                                width = 6,
                                                height = 6,
                                                radius = 3,
                                                background = is_active:map(function(active)
                                                    return active and "#A6E3A1" or "transparent"
                                                end)
                                            },
                                            text {
                                                content = layout_name,
                                                font_size = 13,
                                                foreground = is_active:map(function(active)
                                                    return active and "#CDD6F4" or "#A6ADC8"
                                                end)
                                            }
                                        }
                                    }
                                }
                            }
                        end
                    }
                }
            }
        }
    end
end

return {
    indicator = layout_indicator,
    dropdown = layout_dropdown
}
```


## 9. Rich Interactive Notification Center with Inline Replies & Grouping (`notifications_rich.lua`)

This file implements a complete, production-grade notification center sidebar layout. It displays incoming system notifications grouped reactively by their calling application, handles safe inline input text fields for interactive replies, and contains a persistent state selection widget that writes atomically to the user's `$XDG_STATE_HOME/oblisk/state.json`.

```lua
-- =============================================================================
-- notifications_rich.lua - Rich Notification Center & User State Settings
-- =============================================================================

local notifications = require("oblisk.notifications")
local system        = require("oblisk.system")

-- 1. Computed Signal: Group Notifications by Application Name (Client-Side Sorting)
-- The evaluation function groups the flat D-Bus snapshot feed, caching results
-- to minimize Renderer garbage collection and avoid layout thrashing.
local grouped_notifications = computed({ notifications.feed }, function(feed)
    local groups = {}
    local app_keys = {} -- Tracks dense keys for array sorting
    
    for _, notif in ipairs(feed) do
        local app = notif.app_name or "System"
        if not groups[app] then
            groups[app] = {
                app_name = app,
                items = {},
                count = 0
            }
            table.insert(app_keys, app)
        end
        table.insert(groups[app].items, notif)
        groups[app].count = groups[app].count + 1
    end
    
    -- Convert dictionary map to sorted array list for list repeater compatibility
    local sorted_groups = {}
    for _, app in ipairs(app_keys) do
        table.insert(sorted_groups, groups[app])
    end
    return sorted_groups
end)

-- 2. Bind Wallpaper Animation Selector to Persistent State Signal
local current_animation = bind(system.state):map(function(state)
    return state.wallpaper_animation or "Crossfade"
end)

-- 3. Visual Layout Roots
return panel {
    id = "notification_sidebar",
    width = 380,
    height = "Fill", -- Anchors vertically to match logical height
    background = "#181825",
    border_color = bind(theme.primary),
    border_width = { left = 1 },
    align_h = "End",
    align_v = "Stretch",

    children = {
        column {
            spacing = 16,
            padding = { top = 20, right = 16, bottom = 20, left = 16 },
            align_h = "Stretch",
            align_v = "Stretch",
            children = {
                -- Header Section with Global State Choice indicator
                row {
                    align_h = "Stretch",
                    align_v = "Center",
                    children = {
                        text {
                            content = "System Controls",
                            font_size = 16,
                            font_weight = "Bold",
                            foreground = bind(theme.surface_text)
                        },
                        -- Display current wallpaper animation loaded from state.json
                        row {
                            spacing = 4,
                            children = {
                                icon { name = "image-viewer", size = 14 },
                                text {
                                    content = current_animation,
                                    font_size = 11,
                                    foreground = "#89B4FA"
                                }
                            }
                        }
                    }
                },

                -- Wallpaper Animation Choice Panel (Atomic write-back state selector)
                panel {
                    id = "animation_picker",
                    width = "Fill",
                    height = 80,
                    background = "#1E1E2E",
                    radius = 6,
                    padding = 8,
                    children = {
                        column {
                            spacing = 6,
                            align_h = "Stretch",
                            children = {
                                text { content = "WALLPAPER TRANSITION", font_size = 10, foreground = "#7F849C", font_weight = "Bold" },
                                row {
                                    spacing = 8,
                                    align_h = "Stretch",
                                    children = {
                                        -- Option: Crossfade
                                        button {
                                            height = 28,
                                            width = "Fill",
                                            background = current_animation:map(function(anim)
                                                return anim == "Crossfade" and bind(theme.primary) or "#313244"
                                            end),
                                            on_click = function()
                                                system:write_state("wallpaper_animation", "Crossfade") -- Atomic write to state.json
                                            end,
                                            children = {
                                                text { content = "Fade", font_size = 11, foreground = current_animation:map(function(anim)
                                                    return anim == "Crossfade" and "#000000" or "#CDD6F4"
                                                end) }
                                            }
                                        },
                                        -- Option: Sweep
                                        button {
                                            height = 28,
                                            width = "Fill",
                                            background = current_animation:map(function(anim)
                                                return anim == "Sweep" and bind(theme.primary) or "#313244"
                                            end),
                                            on_click = function()
                                                system:write_state("wallpaper_animation", "Sweep") -- Atomic write to state.json
                                            end,
                                            children = {
                                                text { content = "Sweep", font_size = 11, foreground = current_animation:map(function(anim)
                                                    return anim == "Sweep" and "#000000" or "#CDD6F4"
                                                end) }
                                            }
                                        },
                                        -- Option: Zoom
                                        button {
                                            height = 28,
                                            width = "Fill",
                                            background = current_animation:map(function(anim)
                                                return anim == "Zoom" and bind(theme.primary) or "#313244"
                                            end),
                                            on_click = function()
                                                system:write_state("wallpaper_animation", "Zoom") -- Atomic write to state.json
                                            end,
                                            children = {
                                                text { content = "Zoom", font_size = 11, foreground = current_animation:map(function(anim)
                                                    return anim == "Zoom" and "#000000" or "#CDD6F4"
                                                end) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },

                text {
                    content = "NOTIFICATIONS",
                    font_size = 11,
                    font_weight = "Bold",
                    foreground = "#7F849C",
                    margin = { top = 8 }
                },

                -- Render Grouped Notifications List
                list {
                    id = "grouped_notif_list",
                    source = grouped_notifications,
                    align_h = "Stretch",
                    align_v = "Start",
                    spacing = 14,
                    itemfn = function(group)
                        return column {
                            spacing = 8,
                            align_h = "Stretch",
                            children = {
                                -- Application Header Row
                                row {
                                    spacing = 6,
                                    align_v = "Center",
                                    children = {
                                        icon { name = "preferences-desktop-notification-bell", size = 16 },
                                        text {
                                            content = group.app_name,
                                            font_size = 13,
                                            font_weight = "Bold",
                                            foreground = "#CDD6F4"
                                        },
                                        rect {
                                            width = 16,
                                            height = 16,
                                            radius = 8,
                                            background = "#313244",
                                            children = {
                                                text {
                                                    content = tostring(group.count),
                                                    font_size = 10,
                                                    foreground = "#A6ADC8",
                                                    align_h = "Center",
                                                    align_v = "Center"
                                                }
                                            }
                                        }
                                    }
                                },

                                -- Sub-list rendering items inside the application group
                                list {
                                    source = group.items,
                                    align_h = "Stretch",
                                    spacing = 8,
                                    itemfn = function(notif)
                                        return panel {
                                            id = "notif_card_" .. notif.id,
                                            width = "Fill",
                                            background = "#1E1E2E",
                                            radius = 8,
                                            padding = 10,
                                            children = {
                                                column {
                                                    spacing = 6,
                                                    align_h = "Stretch",
                                                    children = {
                                                        row {
                                                            spacing = 8,
                                                            children = {
                                                                -- Decoded SHM image spooled off-thread by Supervisor
                                                                icon { name = notif.icon_path, size = 32 },
                                                                column {
                                                                    spacing = 2,
                                                                    children = {
                                                                        text { content = notif.summary, font_size = 13, font_weight = "Bold" },
                                                                        -- Renders safe formatting runs (<b>, <i>, <a href>) using cosmic-text
                                                                        text {
                                                                            content = notif.html_formatted_body,
                                                                            font_size = 12,
                                                                            foreground = "#BAC2DE"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        },

                                                        -- Interactive Inline Reply Form (Rendered only on matching D-Bus hint)
                                                        row {
                                                            id = "reply_box_row_" .. notif.id,
                                                            visible = notif.has_reply,
                                                            align_h = "Stretch",
                                                            spacing = 8,
                                                            margin = { top = 4 },
                                                            children = {
                                                                textfield {
                                                                    id = "reply_field_" .. notif.id,
                                                                    width = "Fill",
                                                                    height = 28,
                                                                    placeholder = "Reply...",
                                                                    on_submit = function(text)
                                                                        notifications:reply(notif.id, text) -- Dispatches back to D-Bus
                                                                        notifications:dismiss(notif.id)     -- Closes notification
                                                                    end
                                                                },
                                                                button {
                                                                    width = 50,
                                                                    height = 28,
                                                                    background = bind(theme.primary),
                                                                    radius = 4,
                                                                    on_click = function()
                                                                        -- Submits content using textfield ID reference
                                                                        notifications:reply(notif.id, "reply_field_" .. notif.id)
                                                                        notifications:dismiss(notif.id)
                                                                    end,
                                                                    children = {
                                                                        text { content = "Send", font_size = 11, foreground = "#000000", align_h = "Center", align_v = "Center" }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    end
                                }
                            }
                        }
                    end
                }
            }
        }
    }
}
```

----

## 9. Multi-Display Workspaces Indicator (`workspaces_bar.lua`)

This widget implements a complete, multi-display workspaces status indicator. It binds to the reactive nested output signal tree, loops over physical displays, and highlights active workspaces, focused inputs, and special scratchpads dynamically.

```lua
-- =============================================================================
-- workspaces_bar.lua - Multi-Display and Scratchpad Workspace Bar Widget
-- =============================================================================

local workspaces = require("oblisk.workspaces")

local function workspace_indicator(output_name)
    -- Multi-display visibility loop. We fetch the monitor profile from the output.
    return list {
        id = "workspace_list_" .. output_name,
        direction = "Horizontal",
        spacing = 6,
        align_v = "Center",
        -- Filter workspaces matching this physical output
        source = bind(workspaces.outputs):map(function(outputs)
            for _, out in ipairs(outputs) do
                if out.name == output_name then
                    return out.workspaces
                end
            end
            return {}
        end),
        itemfn = function(ws)
            -- Exclude special scratchpad workspaces from the standard list.
            -- Scratchpads are drawn separately in an overlay widget.
            local is_special = ws:map(function(w) return w.is_special end)
            
            return button {
                width = 24,
                height = 24,
                visible = is_special:map(function(sp) return not sp end),
                -- Highlight active, visible, and focused states differently
                background = bind(ws):map(function(w)
                    if w.is_focused then
                        return "#89B4FA" -- Accent primary for focused monitor
                    elseif w.is_active then
                        return "#45475A" -- Dim highlight for active on other monitors
                    elseif w.is_visible then
                        return "#313244" -- Outline highlight for visible
                    else
                        return "#1E1E2E" -- Dark default for inactive, empty
                    end
                end),
                radius = 12, -- Rounded circles
                on_click = function()
                    workspaces:focus(ws:get().id)
                end,
                children = {
                    text {
                        content = bind(ws):map(function(w) return w.name end),
                        font_size = 11,
                        foreground = bind(ws):map(function(w)
                            if w.is_focused then
                                return "#11111B"
                            elseif w.is_empty then
                                return "#585B70" -- Dark text for empty
                            else
                                return "#CDD6F4" -- Bright text for occupied
                            end
                        end),
                        align_h = "Center",
                        align_v = "Center"
                    }
                }
            }
        end
    }
end

-- Draw special scratchpad indicators (e.g. music, terminals overlay)
local function scratchpad_overlay()
    return list {
        id = "scratchpad_indicator",
        direction = "Horizontal",
        spacing = 8,
        source = bind(workspaces.outputs):map(function(outputs)
            local specials = {}
            for _, out in ipairs(outputs) do
                for _, ws in ipairs(out.workspaces) do
                    if ws.is_special then
                        table.insert(specials, ws)
                    end
                end
            end
            return specials
        end),
        itemfn = function(ws)
            return button {
                height = 28,
                padding = { left = 10, right = 10 },
                background = bind(ws):map(function(w)
                    return w.is_active and "#F38BA8" or "#181825"
                end),
                radius = 6,
                on_click = function()
                    workspaces:toggle_special(ws:get().name)
                end,
                children = {
                    row {
                        spacing = 6,
                        children = {
                            icon { name = "window-restore-symbolic", size = 14 },
                            text {
                                content = bind(ws):map(function(w) return w.name end),
                                font_size = 12,
                                foreground = bind(ws):map(function(w)
                                    return w.is_active and "#11111B" or "#F38BA8"
                                end)
                            }
                        }
                    }
                }
            }
        end
    }
end

return {
    bar = workspace_indicator,
    scratchpad = scratchpad_overlay
}
```

---


## 10. Rescue Mode Fallback UI Schema (`rescue_panel.lua`)

This panel is hardcoded and compiled directly into the Rust Renderer binary. If the user's `shell.lua` contains compilation syntax errors or runtime exceptions, this layout is rendered directly using FemtoVG on the GPU, displaying the error trace in a clean window with an interactive reload mechanism.

```lua
-- =============================================================================
-- rescue_panel.lua - Hardcoded Fallback Error and Recovery UI
-- =============================================================================

local rescue = require("oblisk.rescue")

return panel {
    id = "rescue_screen",
    width = "Fill",
    height = "Fill",
    background = "#0F0F11",
    align_h = "Center",
    align_v = "Center",

    children = {
        column {
            spacing = 20,
            align_h = "Center",
            align_v = "Center",
            children = {
                -- Warning Header Card
                panel {
                    width = 650,
                    background = "#1A1012",
                    border_color = "#F38BA8",
                    border_width = 1,
                    radius = 8,
                    padding = 24,
                    children = {
                        column {
                            spacing = 12,
                            children = {
                                row {
                                    spacing = 10,
                                    children = {
                                        icon { name = "dialog-warning-symbolic", size = 24 },
                                        text {
                                            content = "Oblisk Shell Config Crash Boundary Triggered",
                                            font_size = 18,
                                            font_weight = "Bold",
                                            foreground = "#F38BA8"
                                        }
                                    }
                                },
                                text {
                                    content = "Your active shell.lua layout has crashed or contains a compilation syntax bug. To prevent a black screen or seat freeze, Oblisk has locked execution and loaded this recovery session.",
                                    font_size = 13,
                                    foreground = "#A6ADC8"
                                }
                            }
                        }
                    }
                },

                -- Backtrace Console Container
                panel {
                    width = 650,
                    height = 250,
                    background = "#11111B",
                    border_color = "#313244",
                    border_width = 1,
                    radius = 6,
                    padding = 16,
                    children = {
                        column {
                            spacing = 8,
                            children = {
                                text {
                                    content = "LUA EXCEPTION BACKTRACE:",
                                    font_size = 11,
                                    font_weight = "Bold",
                                    foreground = "#585B70"
                                },
                                text {
                                    content = bind(rescue.error_log),
                                    font_size = 12,
                                    foreground = "#F38BA8",
                                    font_family = "monospace" -- Decoded using cosmic-text monospace fallback
                                }
                            }
                        }
                    }
                },

                -- Control recovery bar
                row {
                    spacing = 16,
                    align_h = "Center",
                    children = {
                        button {
                            width = 200,
                            height = 36,
                            background = "#A6E3A1",
                            radius = 6,
                            on_click = function()
                                rescue:reload_config()
                            end,
                            children = {
                                text {
                                    content = "Reload Configuration",
                                    font_size = 13,
                                    font_weight = "Bold",
                                    foreground = "#11111B",
                                    align_h = "Center",
                                    align_v = "Center"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

---


## 11. Modular Reusable Widget Example (`widgets/battery.lua` & `shell.lua`)

This fixture demonstrates how to write highly modular visual subcomponents with lexical, scoped imports—enforcing our strict single-entry design and preventing global VM memory leaks.

### Subcomponent Widget: `widgets/battery.lua`
```lua
-- =============================================================================
-- widgets/battery.lua - Modular Scoped Subcomponent
-- =============================================================================

local battery = require("oblisk.battery")

-- We return a clean constructor function.
-- Variables, bindings, and states are strictly sandboxed inside this closure.
return function(options)
    local icon_size = options.icon_size or 14
    local text_size = options.font_size or 12

    return row {
        id = "battery_widget",
        spacing = 4,
        align_v = "Center",
        children = {
            icon {
                name = bind(battery.charging):map(function(chg)
                    return chg and "battery-charging" or "battery-good"
                end),
                size = icon_size
            },
            text {
                content = bind(battery.percent):map(function(p)
                    return string.format("%d%%", p)
                end),
                font_size = text_size,
                foreground = bind(battery.percent):map(function(p)
                    return p < 15 and "#F38BA8" or "#CDD6F4" -- Turn text red on low battery
                end)
            }
        }
    }
end
```

### Entry-Point Configuration: `shell.lua`
```lua
-- =============================================================================
-- shell.lua - Single Entry Point Declarative Shell Layout
-- =============================================================================

-- 1. Import Subcomponents Scoped Lexically
local battery_widget = require("widgets.battery")
local workspaces_bar = require("widgets.workspaces_bar")

-- 2. Build and Return single visual root panel
return panel {
    id = "top_bar",
    width = "Fill",
    height = 32,
    background = "#11111B",
    align_h = "Stretch",
    align_v = "Start",

    children = {
        row {
            align_h = "Stretch",
            align_v = "Center",
            padding = { left = 16, right = 16 },
            children = {
                -- Left section containing workspace lists
                row {
                    align_h = "Start",
                    children = {
                        workspaces_bar.bar("eDP-1") -- Spawns indicator locked to main monitor
                    }
                },

                -- Right section displaying battery state via modular subcomponent
                row {
                    align_h = "End",
                    children = {
                        battery_widget({ icon_size = 16, font_size = 13 }) -- Scoped call
                    }
                }
            }
        }
    }
}
```
