# Cookbook

Complete widgets to copy, simplest first. Each recipe is a whole `shell.lua`: save it in a config
directory, run `mantle check -c <dir>`, then start the shell. New to Mantle? The
[introduction](../introduction.md) builds a first bar step by step.

| Recipe | Builds | New since the one above |
| :--- | :--- | :--- |
| [Clock bar](clock-bar.md) | A top bar with a centred clock that toggles to the date on click, with a tooltip | `computed`, named state, a hover tooltip |
| [Battery indicator](battery.md) | A bar pill with charge icon, percentage, time-left tooltip and a low-battery warning | `on_change`, `process.detach` |
| [Volume OSD](volume-osd.md) | A card that slides in with an icon and level bar whenever the volume changes | `pulse`, `animate` with `from`, `monitor = "Active"` |
| [Workspaces](workspaces.md) | Per-monitor workspace pills for Hyprland and niri, with wheel switching and Hyprland special workspaces | Per-output `child`, keyed `list`, `invoke` |
| [Lock screen](lock-screen.md) | A per-monitor lock screen with clock, password field, error hint, fade and idle lock | `lock`, a secure field, an idle threshold |
| [Power menu](power-menu.md) | A full-screen menu to lock, suspend, log out, restart or power off, with confirmation | A full-screen overlay closed by an outside click |
| [Notification popups](notifications.md) | A corner stack of notification cards with formatted bodies, action buttons and dismiss | Text runs, `animate.exit`, nested buttons |
| [App launcher](launcher.md) | A search overlay that fuzzy-ranks installed apps, with keyboard selection | `textfield`, `fuzzy`, `scroll:reveal` |
| [System tray with menu](tray.md) | Tray icons with click, wheel and a right-click dropdown menu with submenus | A grabbing popup, a flattened menu tree |
| [Media player](media-player.md) | A now-playing pill and a card with cover art, a seekable progress bar and controls | Extrapolated position, `on_drag` |

To combine recipes, keep one bar `panel` and put each recipe's bar widgets in its row. Copy every
other surface across as it is.
