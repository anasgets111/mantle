-- Colour roles derived from a wallpaper's `palette.quantize` swatches: neutrals tinted with the hue
-- that covers most of the picture, accents from its distinct vivid hues. Until the first pick the
-- roles hold Catppuccin Mocha. The demo shell and the director each require it and quantize the same
-- file, so both land on the same theme without talking to each other.

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

local function hsv(color)
    local r = tonumber(color:sub(2, 3), 16) / 255
    local g = tonumber(color:sub(4, 5), 16) / 255
    local b = tonumber(color:sub(6, 7), 16) / 255
    local max, min = math.max(r, g, b), math.min(r, g, b)
    local d, h = max - min, 0
    if d > 0 then
        if max == r then
            h = ((g - b) / d) % 6
        elseif max == g then
            h = (b - r) / d + 2
        else
            h = (r - g) / d + 4
        end
    end
    return h * 60, max == 0 and 0 or d / max, max
end

local function rgb(h, s, v)
    local c = v * s
    local x = c * (1 - math.abs((h / 60) % 2 - 1))
    local sectors = { { c, x, 0 }, { x, c, 0 }, { 0, c, x }, { 0, x, c }, { x, 0, c }, { c, 0, x } }
    local sector, m = sectors[math.floor(h / 60) % 6 + 1], v - c
    return { sector[1] + m, sector[2] + m, sector[3] + m }
end

local function hex(h, s, v)
    local channels = rgb(h, s, v)
    local function byte(k) return math.floor(channels[k] * 255 + 0.5) end
    return string.format("#%02x%02x%02x", byte(1), byte(2), byte(3))
end

local function gap(a, b)
    local d = math.abs(a - b) % 360
    return math.min(d, 360 - d)
end

-- A wallpaper is mostly dark ground, so its swatches are bucket means dimmer than its glow: hues
-- survive quantizing, brightness does not. Every role keeps a swatch's hue at its own lightness.
local function derive(swatches)
    local ground = { h = 0, s = 0, share = -1 }
    local vivid = {}
    for _, swatch in ipairs(swatches) do
        local h, s, v = hsv(swatch.color)
        if swatch.share > ground.share then ground = { h = h, s = s, share = swatch.share } end
        if v > 0.1 and s > 0.15 then vivid[#vivid + 1] = { h = h, score = s * v } end
    end
    table.sort(vivid, function(a, b) return a.score > b.score end)
    local hues = {}
    local function add(h)
        for _, other in ipairs(hues) do
            if gap(h, other) < 30 then return end
        end
        hues[#hues + 1] = h
    end
    for _, candidate in ipairs(vivid) do
        add(candidate.h)
    end
    if #hues == 0 then hues[1] = ground.h end
    -- A palette with fewer than five distinct hues borrows neighbours of its main one.
    for _, offset in ipairs({ 50, -50, 110, -110, 170, 140, -140 }) do
        if #hues >= 5 then break end
        add((hues[1] + offset) % 360)
    end

    -- A grey wallpaper has hue 0; scaling saturation by the ground's keeps its neutrals grey, not red.
    local tint = math.min(1, ground.s * 2)
    local function neutral(v, s) return hex(ground.h, s * tint, v) end
    local function pastel(h) return hex(h, 0.45, 0.95) end
    -- A semantic role takes a palette hue within 25 degrees of its meaning's, one no other semantic
    -- role took, so a privacy pill never matches the idle pill; else it keeps its meaning's hue.
    local taken = {}
    local function near(target)
        for _, h in ipairs(hues) do
            if not taken[h] and gap(h, target) <= 25 then
                taken[h] = true
                return pastel(h)
            end
        end
        return pastel(target)
    end
    local danger, caution, success = near(345), near(45), near(115)

    local avatars, avatars_dim = {}, {}
    for k = 1, 5 do
        avatars[k] = pastel(hues[k])
        avatars_dim[k] = hex(hues[k], 0.4, 0.22)
    end
    return {
        crust = neutral(0.07, 0.45),
        mantle = neutral(0.095, 0.42),
        base = neutral(0.12, 0.38),
        surface = neutral(0.2, 0.3),
        overlay = neutral(0.28, 0.24),
        overlay2 = neutral(0.36, 0.2),
        muted = neutral(0.46, 0.16),
        subtle = neutral(0.62, 0.12),
        subtext = neutral(0.72, 0.1),
        subtext1 = neutral(0.8, 0.09),
        text = neutral(0.93, 0.07),
        cursor = hex(hues[1], 0.12, 0.97),
        accent = pastel(hues[1]),
        accent2 = pastel(hues[2]),
        accent3 = pastel(hues[3]),
        success = success,
        danger = danger,
        caution = caution,
        warm = near(25),
        avatars = avatars,
        avatars_dim = avatars_dim,
        tint_a = rgb(hues[1], 0.45, 0.98),
        tint_b = rgb(hues[2], 0.35, 0.97),
    }
end

local M = { state = theme, derive = derive }

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
-- the stock roles back rather than an approximation derived from them.
function M.choose(path)
    if path:match("/mantle%.png$") then return theme:set(DEFAULT) end
    palette.quantize(path, { depth = 5 }, function(swatches)
        if swatches and #swatches > 0 then theme:set(derive(swatches)) end
    end)
end

return M
