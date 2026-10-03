<p class="wordmark"><img src="theme/m.png" alt="M">antle</p>

# Introduction

Mantle runs a desktop shell written in Lua on Wayland. Your `shell.lua` returns
[surfaces](surfaces/index.md) such as bars, windows and popups. Each surface holds
[nodes](nodes/index.md) for layout and drawing. [Signals](guide/signals.md) keep their properties
up to date as [capabilities](capabilities/index.md) report changes.

<video src="https://github.com/user-attachments/assets/6eea81f4-9755-468e-aa2d-f8151c25e2f5" controls muted playsinline preload="metadata"></video>

The video is a Mantle shell: [`demo/director`](../demo/director/shell.lua) types each save and records
the result. `just demo` records it again.

## Your first shell

[Install Mantle](guide/installation.md#install), then create a config directory:

```sh
mantle init
```

Open `~/.config/mantle/shell.lua` and replace its contents with this bar:

<!-- shot-alt: A top bar spanning the screen, with Mantle in bold blue at left and a bold 12:45 clock at right. -->
```lua,shot
return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    width = "Fill",
    height = 32,
    exclusive_zone = true,
    background = "#1e1e2e",
    child = row {
        width = "Fill",
        height = "Fill",
        padding = { left = 12, right = 12 },
        children = {
            text { content = "Mantle", font_weight = 700, foreground = "#89b4fa", align_v = "Center" },
            rect { width = "Fill" },
            text {
                content = mantle.system:map(function(system)
                    return system and os.date("%H:%M", system.time) or "--:--"
                end),
                font_weight = 700,
                foreground = "#cdd6f4",
                align_v = "Center",
            },
        },
    },
}
```

| Part | What it does |
| :--- | :--- |
| `panel` | Places a 32 px bar at the top of each output and reserves that space from windows |
| `row` and the `"Fill"` `rect` | Put the label on the left and push the clock to the right |
| `mantle.system:map(...)` | Updates the clock when the system capability pushes time. The callback handles `nil` before its first push |

Check the config before starting a shell, then run it:

```sh
mantle check
mantle -d
mantle log -f
```

`check` evaluates and lays out the config without Wayland. It catches Lua and layout errors, but
cannot test every value, callback or compositor response ([what check covers](guide/cli.md#what-check-covers)).
`-d` starts the shell detached; `log -f` follows its output. Change `"Mantle"` in `shell.lua` and
save it. The bar reloads in place. If a save fails, the previous bar stays visible and the error
goes to the log ([reload behavior](guide/runtime.md#evaluation-reload-and-generations)).

`mantle init` also writes `.luarc.json` so lua-language-server can complete the API and flag type
mistakes ([editor setup](guide/installation.md#set-up-a-config)).

## Where to go next

| To build | Start with |
| :--- | :--- |
| A clock that changes on click | [Clock bar](cookbook/clock-bar.md), then [signals](guide/signals.md) |
| An app launcher opened by a key | [App launcher](cookbook/launcher.md), then [CLI keybinds](guide/cli.md#cli) |
| A bar with workspace buttons | [Workspaces](cookbook/workspaces.md) |
| A popup, window or lock screen | [Surfaces](surfaces/index.md) |
| Your own widget layout | [Nodes](nodes/index.md) and [paint](guide/paint.md) |
| Help with a shell that shows nothing | [FAQ](guide/faq.md) |

Every [cookbook recipe](cookbook/index.md) is a complete `shell.lua` you can run on its own. The
[glossary](glossary.md) defines terms used throughout the book.

Source: [init](../supervisor/src/setup.rs), [starter](../share/starter/shell.lua),
[check](../renderer/src/check.rs), [watcher](../supervisor/src/watcher.rs),
[reload](../renderer/src/socket/client/mod.rs).
