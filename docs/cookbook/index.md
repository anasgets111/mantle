# Cookbook

Complete widgets to copy. Each recipe is a whole `shell.lua`: save it in a config directory, run
`mantle check -c <dir>`, then start the shell. The colours and sizes are literals, so restyle them
in place. Every page ends with how it works, linking the reference pages, and a few one-line
variations.

To combine recipes, keep one bar `panel` and put each recipe's bar widgets in its row. Every other
surface can be copied across as it is.

| Recipe | Builds |
| :--- | :--- |
| [Clock bar](clock-bar.md) | A top bar with a centred clock that toggles to the date on click and shows a tooltip |
| [Workspaces](workspaces.md) | Per-monitor workspace pills for Hyprland and niri, with wheel switching and Hyprland special workspaces |
| [Volume OSD](volume-osd.md) | A card that slides in with an icon and level bar whenever the volume changes |
| [Notification popups](notifications.md) | A corner stack of notification cards with formatted bodies, action buttons and dismiss |
| [App launcher](launcher.md) | A search overlay that fuzzy-ranks installed apps, with keyboard selection |
| [Battery indicator](battery.md) | A bar pill with charge icon, percentage, time-left tooltip and a low-battery warning |
| [System tray with menu](tray.md) | Tray icons with click, wheel and a right-click dropdown menu with submenus |
| [Lock screen](lock-screen.md) | A per-monitor lock screen with clock, password field, error hint, fade and idle lock |
| [Media player](media-player.md) | A now-playing pill and a card with cover art, a seekable progress bar and controls |
| [Power menu](power-menu.md) | A full-screen menu to lock, suspend, log out, restart or power off, with confirmation |

New to Mantle? Start with the [introduction](../introduction.md), which builds a first bar step by
step, then come back here.
