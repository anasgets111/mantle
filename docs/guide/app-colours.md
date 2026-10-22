# Colour your apps

Turn the wallpaper's colours into config files other programs read: a kitty theme, GTK and Qt
colour definitions. The pipeline is [`palette.quantize`](scripting.md#palettequantize), then
[`palette.score`](scripting.md#palettescore), then [`palette.scheme`](scripting.md#palettescheme)
once for dark and once for light, then a template renderer, then [`file.write`](scripting.md#file). A reload hook
runs only when a file's bytes changed.

## Render matugen templates

This renderer fills the placeholder subset `{{colors.<role>.<default|light|dark>.<format>}}`.
Roles are `palette.scheme`'s snake_case names, the ones matugen uses. Formats are `hex`
(`#RRGGBB`, uppercase like `palette.scheme`), `hex_stripped`, `rgb`, `rgba` (alpha 1.0) and `hsl`. `default` is the mode you pass.
A placeholder it cannot resolve stays as written. Not supported: filters
(`{{ ... | lighten: 10 }}`) and `<* ... *>` blocks, so templates using them need those lines edited.

```lua
local function hsl(r, g, b)
    r, g, b = r / 255, g / 255, b / 255
    local hi, lo = math.max(r, g, b), math.min(r, g, b)
    local l, h, s = (hi + lo) / 2, 0, 0
    if hi ~= lo then
        local d = hi - lo
        s = l > 0.5 and d / (2 - hi - lo) or d / (hi + lo)
        if hi == r then h = (g - b) / d + (g < b and 6 or 0)
        elseif hi == g then h = (b - r) / d + 2
        else h = (r - g) / d + 4 end
    end
    local function round(x) return math.floor(x + 0.5) end -- `%d` raises on a float
    return string.format("hsl(%d, %d%%, %d%%)", round(h * 60), round(s * 100), round(l * 100))
end

local function format(hex, how)
    local r, g, b = tonumber(hex:sub(2, 3), 16), tonumber(hex:sub(4, 5), 16), tonumber(hex:sub(6, 7), 16)
    if how == "hex" then return hex end
    if how == "hex_stripped" then return hex:sub(2) end
    if how == "rgb" then return string.format("rgb(%d, %d, %d)", r, g, b) end
    if how == "rgba" then return string.format("rgba(%d, %d, %d, 1.0)", r, g, b) end
    if how == "hsl" then return hsl(r, g, b) end
end

--- `schemes` is { dark = palette.scheme(...), light = palette.scheme(...) }; `mode` is "dark" or "light".
local function render(template, schemes, mode)
    return (template:gsub("{{%s*colors%.([%w_]+)%.(%w+)%.([%w_]+)%s*}}", function(role, which, how)
        local scheme = schemes[which == "default" and mode or which]
        local hex = scheme and scheme[role]
        return hex and format(hex, how) or nil
    end))
end

local seed = "#6750A4"
local schemes = {
    dark = palette.scheme(seed, { dark = true }),
    light = palette.scheme(seed, { dark = false }),
}
return text {
    content = render("background {{colors.surface.default.hex}} / {{colors.primary.light.rgb}} / {{colors.primary.dark.hsl}}", schemes, "dark"),
}
```

Save the `format` and `render` functions (with `hsl`) as `app_colours.lua` in your config directory,
ending in `return render`.

## Reference templates

kitty, `kitty.conf.tpl`, written to `~/.config/kitty/mantle.conf`. Add `include mantle.conf` to `kitty.conf`:

```text
foreground {{colors.on_surface.default.hex}}
background {{colors.surface.default.hex}}
selection_foreground {{colors.on_primary.default.hex}}
selection_background {{colors.primary.default.hex}}
cursor {{colors.primary.default.hex}}
url_color {{colors.tertiary.default.hex}}
active_border_color {{colors.primary.default.hex}}
inactive_border_color {{colors.outline_variant.default.hex}}
```

GTK, `gtk.css.tpl`, for `~/.config/gtk-3.0/colors.css` and `gtk-4.0/colors.css`. Add `@import 'colors.css';` at the top of the `gtk.css` beside it:

```text
@define-color accent_color {{colors.primary.default.hex}};
@define-color accent_bg_color {{colors.primary_container.default.hex}};
@define-color accent_fg_color {{colors.on_primary_container.default.hex}};
@define-color window_bg_color {{colors.surface.default.hex}};
@define-color window_fg_color {{colors.on_surface.default.hex}};
@define-color headerbar_bg_color {{colors.surface_container.default.hex}};
@define-color view_bg_color {{colors.surface_container_low.default.hex}};
```

qt6ct, `mantle.conf.tpl`, for `~/.config/qt6ct/colors/mantle.conf`. The 22 entries per palette
are Qt's `QPalette::ColorRole` indices in order, which qt6ct reads by position: `WindowText, Button, Light, Midlight, Dark, Mid, Text, BrightText, ButtonText, Base, Window, Shadow,
Highlight, HighlightedText, Link, LinkVisited, AlternateBase, NoRole, ToolTipBase, ToolTipText, PlaceholderText, Accent`:

```text
[ColorScheme]
active_colors={{colors.on_surface.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface_container_high.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface_dim.default.hex}}, {{colors.outline_variant.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface.default.hex}}, {{colors.shadow.default.hex}}, {{colors.primary.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.tertiary.default.hex}}, {{colors.secondary.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface.default.hex}}, {{colors.surface_container_highest.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.primary.default.hex}}
disabled_colors={{colors.on_surface_variant.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface_container_high.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface_dim.default.hex}}, {{colors.outline_variant.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface.default.hex}}, {{colors.shadow.default.hex}}, {{colors.outline.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.tertiary.default.hex}}, {{colors.secondary.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface.default.hex}}, {{colors.surface_container_highest.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.outline.default.hex}}
inactive_colors={{colors.on_surface.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface_container_high.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface_dim.default.hex}}, {{colors.outline_variant.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.surface_container_low.default.hex}}, {{colors.surface.default.hex}}, {{colors.shadow.default.hex}}, {{colors.primary.default.hex}}, {{colors.on_primary.default.hex}}, {{colors.tertiary.default.hex}}, {{colors.secondary.default.hex}}, {{colors.surface_container.default.hex}}, {{colors.surface.default.hex}}, {{colors.surface_container_highest.default.hex}}, {{colors.on_surface.default.hex}}, {{colors.on_surface_variant.default.hex}}, {{colors.primary.default.hex}}
```

Qt reads `#RRGGBB` or `#AARRGGBB`; select the file in qt6ct's Appearance tab once.

## The pipeline

The wallpaper path is yours. `file.read` fetches each template, `file.write` writes the output, and
`process.run` signals the app only when `changed` is `true`, so an unchanged wallpaper restarts
nothing.

```lua,fragment
local render = require("app_colours")

local targets = {
    { template = "kitty.conf.tpl", out = "~/.config/kitty/mantle.conf", reload = { "pkill", { "-USR1", "kitty" } } },
    { template = "gtk.css.tpl", out = "~/.config/gtk-4.0/colors.css" },
    { template = "mantle.conf.tpl", out = "~/.config/qt6ct/colors/mantle.conf" },
}

local function apply(seed, mode)
    local schemes = {
        dark = palette.scheme(seed, { dark = true }),
        light = palette.scheme(seed, { dark = false }),
    }
    for _, target in ipairs(targets) do
        file.read(mantle.config_dir .. "/templates/" .. target.template, function(template, err)
            if not template then return log.warn("app colours:", err) end
            file.write(target.out, render(template, schemes, mode), nil, function(changed, werr)
                if werr then return log.warn("app colours:", werr) end
                if changed and target.reload then
                    process.run(target.reload[1], target.reload[2], function() end, function() end)
                end
            end)
        end)
    end
end

palette.quantize(mantle.config_dir .. "/wallpaper.png", { depth = 7 }, function(found)
    if found then apply(palette.score(found)[1], "dark") end
end)
```

A reload drops callbacks still pending. If one lands first, the file is written but its hook skipped,
and the next run reports `changed = false`. Touch the output by hand to force the hook.

`file.write` replaces a file by renaming a temporary over it, which ends an inotify watch an app holds
on that one path. For an app that live-reloads a single file, pass `{ in_place = true }` in the
`opts` slot: it rewrites the same inode and keeps the watch. A crash mid-write leaves a truncated
file, so use it only where the app needs it.

```lua,fragment
file.write(target.out, text, { in_place = true }, on_done)
```

## Recolour the shell itself

Keep the theme in one `state`, or a [`persistent_table`](scripting.md#persistent_table) key to survive
restarts, and derive every colour token as a `:map` over it. Each node reading a token repaints when
the theme changes; nothing else needs rebuilding.

```lua
local seed = "#6750A4"
local schemes = {
    dark = palette.scheme(seed, { dark = true }),
    light = palette.scheme(seed, { dark = false }),
}
local theme = state("theme", "dark")

local function token(role)
    return theme:map(function(name) return schemes[name][role] end)
end
local surface, primary = token("surface"), token("primary")

return rect {
    padding = 12, radius = 8,
    background = surface,
    -- an alpha suffix goes inside the map: a signal does not concatenate
    border_color = primary:map(function(hex) return hex .. "80" end),
    border_width = 2,
    on_click = function() theme:set(theme:get() == "dark" and "light" or "dark") end,
    children = {
        rect { width = 80, height = 6, background = {
            gradient = "linear", angle = 90, stops = { { 0, primary }, { 1, surface } },
        } },
    },
}
```

What takes a signal:

| Where the colour goes | Pass |
| :--- | :--- |
| A property, such as `background` | The token |
| A gradient stop's colour, a shader `params` entry | The token, or a `:map` of it, in the table. The walk reaches 8 tables deep and skips a table with a metatable |
| A string built from a token, such as `token .. "80"` | The whole string as one `:map`. `..` on a signal raises |
| A whole gradient or `params` table | One `:map` that returns the full table |

A shader's `params` hold numbers, never hex: map the token to `{ r, g, b }` in `0..1`.

## React to a theme change

`on_change` runs `fn(current, previous)` after any write: `:set` or `mantle set` on a state; `store:set` or another process editing the file for a key.
Rewrite the templates and relaunch from there instead of watching `mantle.storage` and diffing keys by hand.

```lua,fragment
local settings = persistent_table { path = "~/.local/state/myshell/theme.json", defaults = { mode = "dark" } }

settings.mode:on_change(function(mode)
    apply(seed, mode) -- the pipeline above
    mantle.applications:launch("org.example.Editor", { env = { GTK_THEME = false } })
end)
```

The `env` table sets variables with a string and unsets them with `false`; here the relaunched app
reads the files just written, not a `GTK_THEME` inherited from the shell. A running single-instance
app keeps its old environment: see [`process.detach`](processes.md#processdetach) and the
[applications](../capabilities/applications.md) page.
