-- Stage geometry for the director and the demo shell it drives. Both read the same
-- output, so a popup and the pointer aimed at it share one origin.
--
-- Drawn for a 3440-wide frame: the code pane is 44% at 22px, and the shell's popups
-- sit in the other 56%. A narrower frame grows the pane until those same columns fit
-- at 16px, and every popup shrinks into what remains.

local DESIGN_PANE = math.floor(3440 * 0.44)
local CODE_FONT = 22
local MIN_FONT = 16
-- ponytail: budget 114 columns including the gutter at about 0.6em per glyph.
-- Recalibrate EM against the director's char_box measurement when changing the font.
local COLUMNS = 114
local EM = 0.6

local function metrics(screen)
    if not screen then
        -- `mantle check` hands no screens; the validation recipes size it here.
        local w, h = (os.getenv("MANTLE_DEMO_SCREEN") or ""):match("^(%d+)x(%d+)$")
        screen = { width = tonumber(w) or 1920, height = tonumber(h) or 1080, name = "" }
    end
    local w, h = screen.width, screen.height
    local natural = math.floor(w * 0.44)
    local min_pane = math.ceil(COLUMNS * MIN_FONT * EM)
    local pane = natural < min_pane and math.min(min_pane, math.floor(w * 0.62)) or natural
    local font = math.floor(CODE_FONT * pane / DESIGN_PANE / 2) * 2
    font = math.max(MIN_FONT, math.min(CODE_FONT, font))
    return {
        width = w,
        height = h,
        name = screen.name or "",
        pane = pane,
        open = w - pane - 16,
        font = font,
        line = math.floor(font * 1.5),
    }
end

local function box(screen, design, docked)
    local m = metrics(screen)
    local gutter = docked and 64 or 48
    local width = math.min(design, math.max(240, m.open - gutter))
    local left = docked and (m.open - width - 40) or math.floor((m.open - width) / 2)
    return { top = 24, left = math.max(12, left), width = width }
end

local M = { metrics = metrics }

function M.center(screen, design)
    return box(screen, design)
end

function M.dock(screen, design)
    return box(screen, design, true)
end

function M.fit(screen, design)
    local m = metrics(screen)
    return math.min(design, math.max(240, m.open - 96))
end

-- Four tiles: row padding 16, gap 16, thumbnail padding 4. Borders take no layout space.
local PICKER_CHROME = 16 * 2 + 16 * 3 + 4 * 2 * 4

function M.picker(screen)
    local m = metrics(screen)
    local tile = math.max(1, math.min(300, math.floor((m.open - 48 - PICKER_CHROME) / 4)))
    local width = 4 * tile + PICKER_CHROME
    return {
        top = 24,
        left = math.max(12, math.floor((m.open - width) / 2)),
        width = width,
        tile = tile,
        tile_h = math.max(40, math.floor(tile * 126 / 300)),
    }
end

function M.overview(screen)
    local m = metrics(screen)
    local sheet = math.floor(m.open * 0.86)
    local shot = math.max(120, sheet - 64)
    local card = math.min(200, math.floor((shot - 18 * 3) / 4))
    return {
        output = m.name,
        left = math.max(12, math.floor((m.open - sheet) / 2)),
        top = 24,
        sheet = sheet,
        shot = shot,
        shot_height = math.max(80, math.floor(shot * m.height / math.max(1, m.width))),
        card = math.max(96, card),
    }
end

function M.mock(screen)
    local m = metrics(screen)
    local reserve = math.min(460, math.floor(m.height * 460 / 1440))
    local width = math.min(1400, math.max(320, m.open - 80))
    return {
        width = width,
        height = math.min(840, math.max(360, m.height - 56 - reserve)),
        left = math.max(12, math.floor((m.open - width) / 2)),
        top = 70,
    }
end

-- Hugs its text on a stage wide enough for the longest line. Narrower, it takes the
-- stage and wraps, so a caption does not run under the code pane.
function M.caption(m)
    if m.open >= 1400 then return { title = 52, detail = 26 } end
    -- 48px panel margin, 60px of card padding, 16px before the code pane.
    return { width = math.max(320, m.open - 124), title = 36, detail = 22 }
end

return M
