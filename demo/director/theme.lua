-- Colour roles from `palette.score` and `palette.scheme` on a wallpaper. Until the first pick
-- they hold Catppuccin Mocha. The demo shell and the director each require it and quantize the
-- same file, so both land on the same theme without talking to each other.

local DEFAULT = {
    crust = "#11111b",
    mantle = "#181825",
    base = "#1e1e2e",
    surface = "#313244",
    overlay = "#45475a",
    overlay2 = "#585b70",
    muted = "#6c7086",
    subtle = "#9399b2",
    subtext = "#a6adc8",
    subtext1 = "#bac2de",
    text = "#cdd6f4",
    cursor = "#f5e0dc",
    accent = "#89b4fa",
    accent2 = "#cba6f7",
    accent3 = "#b4befe",
    success = "#a6e3a1",
    danger = "#f38ba8",
    caution = "#f9e2af",
    warm = "#fab387",
    avatars = { "#f5c2e7", "#94e2d5", "#fab387", "#89dceb", "#cba6f7" },
    avatars_dim = { "#3a2433", "#1f3533", "#3a2c22", "#1f3038", "#2a2340" },
    tint_a = { 0.54, 0.71, 0.98 },
    tint_b = { 0.80, 0.65, 0.97 },
}

local theme = state("theme", DEFAULT)

-- Shader uniforms want 0..1 channels. Scheme roles are `#RRGGBB`.
local function unit(color)
    local function channel(at) return tonumber(color:sub(at, at + 1), 16) / 255 end
    return { channel(2), channel(4), channel(6) }
end

-- The shell's role names, filled from a dark Material scheme. `error`, `tertiary` and
-- `secondary` stand in for danger, success and caution: a scheme has no separate semantic hues.
local function from_scheme(s)
    return {
        crust = s.surface_container_lowest,
        mantle = s.surface_container_low,
        base = s.surface,
        surface = s.surface_container_high,
        overlay = s.surface_container_highest,
        overlay2 = s.outline_variant,
        muted = s.outline,
        subtle = s.on_surface_variant,
        subtext = s.on_surface,
        subtext1 = s.on_surface,
        text = s.on_surface,
        cursor = s.primary,
        accent = s.primary,
        accent2 = s.secondary,
        accent3 = s.on_tertiary_container,
        success = s.tertiary,
        danger = s.error,
        caution = s.secondary,
        warm = s.on_error_container,
        avatars = { s.primary, s.secondary, s.tertiary, s.error, s.primary_fixed },
        avatars_dim = {
            s.primary_container,
            s.secondary_container,
            s.tertiary_container,
            s.error_container,
            s.surface_container_high,
        },
        tint_a = unit(s.primary),
        tint_b = unit(s.secondary),
    }
end

local M = { state = theme }

for role, value in pairs(DEFAULT) do
    if type(value) == "string" then
        M[role] = theme:map(function(t) return t[role] end)
    end
end

-- `role` with a two-digit hex alpha, as a signal.
function M.fade(role, alpha)
    return theme:map(function(t) return t[role] .. alpha end)
end

function M.avatar(k)
    return theme:map(function(t) return t.avatars[k] end)
end

function M.avatar_dim(k)
    return theme:map(function(t) return t.avatars_dim[k] end)
end

M.tints = theme:map(function(t) return { tint_a = t.tint_a, tint_b = t.tint_b } end)

-- Switches every role to `path`, a small raster. The logo art is drawn in Mocha itself, so it gets
-- the stock roles back. Score wants a deep quantize; `content` keeps the seed's chroma.
function M.choose(path)
    if path:match("/mantle%.png$") then return theme:set(DEFAULT) end
    palette.quantize(path, { depth = 7 }, function(swatches)
        if not swatches or #swatches == 0 then return end
        theme:set(from_scheme(palette.scheme(palette.score(swatches)[1], { dark = true, variant = "content" })))
    end)
end

return M
