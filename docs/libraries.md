# Component libraries

Mantle ships nodes, not widgets. These libraries build full widget sets from them in plain Lua on
the public API, each with a demo app that shows every component. Both are MIT-licensed and typed
for LuaLS, so the editor completes and checks every `opts` table.

| Library | Look | Module | Needs |
| :--- | :--- | :--- | :--- |
| [mantle-glass](https://github.com/anasgets111/mantle-glass) | Liquid Glass | `glass` | Mantle built from `main` after 0.4.0 |
| [mantle-material](https://github.com/anasgets111/mantle-material) | Material 3 Expressive | `m3` | Mantle 0.4.0 |

To use one, copy or symlink its module folder (`glass/` or `m3/`) into your config directory and
`require` it. To run its demo, clone the repository and run `mantle -c .` inside it.

## mantle-glass

![mantle-glass demo: a glass menu bar with an open menu and the Dock](https://raw.githubusercontent.com/anasgets111/mantle-glass/main/docs/screenshot.png)

A Liquid Glass component library: window chrome (traffic lights, toolbars, sidebar, inspector,
tabs), controls, text fields, pickers, tables and outlines, menus, popovers,
alerts, sheets, a menu bar and a Dock. Its glass materials refract and follow the desktop's dark
mode, accent, contrast and reduced-motion settings.

```lua,fragment
fonts { "Inter", "lucide", "Noto Sans" }
local glass = require("glass")
glass.core.window = "app" -- the app window's id, before building controls

return {
    glass.app_window {
        id = "app",
        title = "My app",
        child = glass.button("save", { label = "Save", kind = "default" }),
    },
}
```

## mantle-material

![mantle-material demo: badges, progress indicators, loading indicator and the Expressive shapes](https://raw.githubusercontent.com/anasgets111/mantle-material/main/docs/screenshot.png)

A Material 3 Expressive component library: the full M3 catalogue (actions, communication,
containment, navigation, selection and text inputs), dynamic colour from one seed, the Expressive
motion springs, and adaptive layouts: window size classes, navigation that becomes a bar, rail or
drawer by width, and list-detail panes.

```lua,fragment
fonts { "Roboto", "Noto Sans" }
local m3 = require("m3")
m3.theme.seed:set("#6750A4") -- optional

return {
    m3.app_window {
        id = "app",
        title = "My app",
        child = m3.button("save", { label = "Save", on_click = function() m3.overlay.notify("Saved") end }),
    },
}
```
