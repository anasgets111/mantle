# icon

A square icon from the desktop icon theme, or from a file path. Reach for it for app icons, status
glyphs and tray items. Symbolic (SVG) icons take a tint. For photos and artwork at their own aspect
ratio, use an [`image`](image.md).

The focused window's icon and title. [`mantle.applications`](../capabilities/applications.md) maps a
window's `app_id` to its desktop entry, whose `icon` is a theme name:

```lua,shot
local focused_icon = computed({ mantle.applications, mantle.workspaces }, function(apps, workspaces)
    local client = workspaces and workspaces.active_client
    if apps == nil or client == nil then return "" end
    local index = apps.by_app_id[client.class] or apps.by_app_id[string.lower(client.class)]
    return index and apps.entries[index].icon or ""
end)

local app_badge = row {
    spacing = 8, padding = { left = 8, right = 12, top = 6, bottom = 6 }, radius = 8, background = "#313244",
    children = {
        icon { name = focused_icon, size = 20, align_v = "Center" },
        text { content = mantle.workspaces:map(function(w)
            return w and w.active_client and w.active_client.title or ""
        end), max_width = 200, elide = "End", align_v = "Center", foreground = "#CDD6F4" },
    },
}

return app_badge
```

## Properties

`icon` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `name` | `string\|Bound` | `""` | An icon theme name (`"firefox"`, `"audio-volume-high-symbolic"`), looked up at the drawn size, or an absolute image path, used as is. `""` or a name the theme lacks draws nothing |
| `size` | `number\|Bound` | `12` | The box is `size` × `size` px; not range-checked |
| `foreground` | `Color\|Bound` | The file's own colours | Colour for the SVG's `currentColor` (CSS `color`), which tints symbolic icons. Full-colour icons ignore it |
<!-- End of the generated table. -->

An explicit `width` or `height` overrides that axis of the square; the icon draws at the shorter
side, centred.

The theme is `gtk-icon-theme-name` from `$XDG_CONFIG_HOME/gtk-4.0/settings.ini`, else
`gtk-3.0/settings.ini`, else `hicolor`. It is read once per Renderer process, so a theme
change shows after a shell restart, not a reload. Files load as PNG, JPEG, WebP, GIF, SVG or SVGZ.

## How do I…

| Task | Answer |
| :--- | :--- |
| Show an app's icon | The example above |
| Tint a symbolic icon | `foreground = "#CDD6F4"` on a `-symbolic` name |
| Show a tray item's icon | `name = item.icon_name or item.icon_path`: both spellings work ([tray](../capabilities/tray.md)) |
| Show a notification's app icon | `name = notification.app_icon` ([notifications](../capabilities/notifications.md)) |
| Make an icon button | Put the `icon` in a [`button`](button.md) |
| Put a badge on an icon | Layer them in a [`rect`](rect.md) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| An icon draws nothing | The theme has no such name. Check the name under `/usr/share/icons/<theme>`, or pass an absolute path |
| `foreground` does not change a colour icon | Only SVGs that use `currentColor` (symbolic icons) take it |
| The wrong theme's icons appear | The theme comes from GTK settings, read at Renderer start. Set `gtk-icon-theme-name` and restart the shell |
| An icon is smaller than its box | It draws at the shorter side of `width`/`height`. Keep them equal, or use `size` alone |
| An icon given as a relative path draws nothing | A relative `name` is a theme name. Use `mantle.config_dir .. "/icons/x.svg"` |

See also: [image](image.md), [capabilities](../capabilities/index.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[theme lookup](../../renderer/src/image/icons.rs), [decode](../../renderer/src/image/decode.rs),
[icon draw](../../renderer/src/layout/paint/build.rs).
