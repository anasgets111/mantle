-- What the director draws: the code pane it types into, captions, title and end cards, wallpaper.

local syntax, edits, theme, layout, run = require("lua_syntax"), require("edits"), require("theme"),
    require("layout"), require("run")

local M = {}

local MONO = "CaskaydiaCove Nerd Font Mono"
local GUTTER = 6
-- The demo bar's final height, cleared through its exclusive zone.
local BAR = 56
local HEADER = 64

local frame = mantle.screens:map(function(screens)
    return layout.metrics(screens and screens[1])
end)
local code_size = frame:map(function(m) return m.font end)
local line_px = frame:map(function(m) return m.line end)
local pane_width = frame:map(function(m) return m.pane end)
local row_count = frame:map(function(m)
    return math.max(1, math.floor((m.height - BAR - 32 - HEADER) / m.line))
end)

local WORDMARK = mantle.config_dir .. "/../../docs/theme/m.png"
local WALLPAPER = mantle.config_dir .. "/wallpaper.svg"

-- The buffer lives in these locals; `version` is the signal that says it changed.
local lines = {}
local caret = { line = 1, col = 1 }
-- Lines outside the playing hunk dim while an edit types; the save restores them.
local focus

local version = state("demo_version", 0)
local code_scroll = scroll("demo_code")
local status = state("demo_status", "saved")
local caption = state("demo_caption", "")
local detail = state("demo_detail", "")
local keys = state("demo_keys", "")
local key_command = state("demo_key_command", "")
local card = state("demo_card", "title")
local card_shown = state("demo_card_shown", true)
local char_box = geometry("demo_char_box")
local meter = state("demo_meter", "")
local file_shown = state("demo_file", "shell.lua")
-- Set, the code pane is off and `layout.placed` boxes span the whole screen, here and in the
-- demo shell (see `set_stage`).
local stage_full = state("stage_full", false)
-- The last paste, for the flash over its lines.
local flash = state("demo_flash", false)
-- The picked wallpaper, for the cards and the browser mockup; the logo art until the first pick.
local backdrop = state("demo_backdrop", WALLPAPER)

local function bump() version:set(version:get() + 1) end

-- The first visible line, where the pane is headed; the scroll eases there.
local top = 0

local function scroll_top(first)
    top = first
    code_scroll:scroll_to(first * line_px:get())
end

-- Scrolls so `line` sits a third of the way down whenever it strays near an edge.
local function reveal(line)
    local count = row_count:get()
    if line > top + 3 and line <= top + count - 4 then return end
    scroll_top(math.max(0, math.min(line - math.floor(count / 3), #lines - count + 1)))
end

mantle.screens:on_change(function() reveal(caret.line) end)

-- `syntax.highlight` caches per line text and is warmed in chunks behind the title card: tokenizing a
-- whole file in one recompute overruns the 2.5 ms budget. Coloured and dimmed runs per line, dropped
-- with the theme.
local highlighted, dimmed, highlighted_for = {}, {}, nil

local code = computed({ version, theme.state }, function(_, t)
    if t ~= highlighted_for then highlighted, dimmed, highlighted_for = {}, {}, t end
    local runs = {}
    for n, line in ipairs(lines) do
        runs[#runs + 1] = { text = string.format("%4d  ", n), color = n == caret.line and t.subtext or t.overlay }
        -- Outside the focus, the same colours at 70% alpha.
        local dim = focus and (n < focus.first or n > focus.last)
        local cache = dim and dimmed or highlighted
        local colored = cache[line]
        if not colored then
            colored = {}
            for _, run in ipairs(syntax.highlight(line)) do
                local color = t[syntax.roles[run.kind]]
                colored[#colored + 1] = { text = run.text, color = dim and color .. "b3" or color }
            end
            cache[line] = colored
        end
        table.move(colored, 1, #colored, #runs + 1, runs)
        runs[#runs + 1] = { text = "\n" }
    end
    return runs
end)

local caret_row = computed({ version, line_px }, function(_, line) return (caret.line - 1) * line end)

-- `version` as well: typing along one line moves `caret.col` but leaves `caret_row` unchanged.
local caret_at = computed({ version, caret_row, char_box }, function(_, y, box)
    local width = (box and box.width or 0) / 100
    return { x = (GUTTER + caret.col - 1) * width, y = y + 5 }
end)

M.code_pane = panel {
    id = "code",
    layer = "top",
    anchor = { top = true, right = true, bottom = true },
    margin = { top = 16, right = 16, bottom = 16 },
    width = pane_width,
    height = "fill",
    -- Hidden, the pane slides off the right edge.
    child = column {
        width = "fill",
        height = "fill",
        background = theme.fade("crust", "f2"),
        radius = 18,
        translate = computed({ stage_full, frame }, function(full, m)
            return { x = full and m.pane + 32 or 0, y = 0 }
        end),
        animate = { translate = { duration = 450, easing = "in_out_cubic" } },
        children = {
            row {
                width = "fill",
                height = HEADER,
                padding = { left = 24, right = 24 },
                spacing = 12,
                children = {
                    text { content = file_shown, align_v = "center", font = MONO, font_size = 20, foreground = theme.text },
                    rect { width = "fill" },
                    rect {
                        visible = meter:map(function(m) return m ~= "" end),
                        height = 40,
                        align_v = "center",
                        padding = { left = 16, right = 16 },
                        radius = 12,
                        clip = "box",
                        background = theme.base,
                        animate = { width = { duration = 200, easing = "out_cubic" } },
                        children = {
                            text {
                                content = meter,
                                align_v = "center",
                                font = MONO,
                                font_size = 18,
                                foreground = theme.subtext,
                            },
                        },
                    },
                    rect {
                        align_v = "center",
                        clip = "box",
                        animate = { width = { duration = 200, easing = "out_cubic" } },
                        children = {
                            text {
                                content = status:map(function(s) return s == "unsaved" and "●  unsaved" or "✓  saved" end),
                                foreground = computed({ status, theme.warm, theme.success }, function(s, dirty, clean)
                                    return s == "unsaved" and dirty or clean
                                end),
                                animate = { foreground = 200 },
                                font = MONO,
                                font_size = 18,
                            },
                        },
                    },
                },
            },
            rect { width = "fill", height = 1, background = theme.surface },
            rect {
                width = "fill",
                height = "fill",
                clip = "box",
                padding = { top = 12 },
                children = {
                    text {
                        content = string.rep("0", 100),
                        geometry = char_box,
                        opacity = 0,
                        font = MONO,
                        font_size = code_size,
                    },
                    column {
                        width = "fill",
                        height = "fill",
                        scroll = code_scroll,
                        animate = { scroll = { duration = 320, easing = "out_cubic" } },
                        children = {
                            rect {
                                width = "fill",
                                children = {
                                    rect {
                                        width = "fill",
                                        height = line_px,
                                        background = theme.fade("text", "0a"),
                                        translate = caret_row:map(function(y) return { x = 0, y = y } end),
                                        animate = { translate = 80 },
                                    },
                                    rect {
                                        width = "fill",
                                        children = computed({ flash, line_px }, function(f, line)
                                            if not f then return {} end
                                            return { rect {
                                                id = "flash:" .. f.serial,
                                                width = "fill",
                                                height = f.count * line,
                                                background = theme.fade("accent", "40"),
                                                translate = { x = 0, y = (f.line - 1) * line },
                                                opacity = 0,
                                                animate = { opacity = { duration = 600, from = 1 } },
                                            } }
                                        end),
                                    },
                                    text {
                                        content = code,
                                        font = MONO,
                                        font_size = code_size,
                                        line_height = 1.5,
                                        foreground = theme.text,
                                    },
                                    rect {
                                        width = 3,
                                        height = line_px:map(function(line) return line - 10 end),
                                        background = theme.cursor,
                                        translate = caret_at,
                                        animate = { translate = 60 },
                                    },
                                },
                            },
                        },
                    },
                },
            },
        },
    },
}

-- Captions ----------------------------------------------------------------------------------

local function chip(label)
    return rect {
        padding = { left = 16, right = 16, top = 8, bottom = 8 },
        radius = 10,
        background = theme.surface,
        border_width = 1,
        border_color = theme.overlay2,
        scale = 1,
        animate = { scale = { duration = 260, easing = "out_back", from = 0.6 } },
        children = { text { content = label, font = MONO, font_size = 24, foreground = theme.text } },
    }
end

local key_row = computed({ keys, key_command }, function(combo, command)
    local out = {}
    for key in combo:gmatch("[^+]+") do
        if #out > 0 then
            out[#out + 1] = text { content = "+", align_v = "center", font_size = 24, foreground = theme.muted }
        end
        out[#out + 1] = chip(key)
    end
    out[#out + 1] = text {
        content = (#out > 0 and "→  " or "$ ") .. command,
        align_v = "center",
        font = MONO,
        font_size = 22,
        foreground = theme.accent,
    }
    return out
end)

M.caption_pane = panel {
    id = "caption",
    layer = "overlay",
    anchor = { bottom = true, left = true },
    -- The card sits in a 64 px frame so its shadow is not cut at the surface edge.
    margin = { bottom = 48 - 64, left = 48 - 64 },
    visible = caption:map(function(title) return title ~= "" end),
    child = computed({ caption, frame }, function(title, m)
        local cap = layout.caption(m)
        return column {
            width = cap.width and cap.width + 128,
            padding = 64,
            children = { column {
                id = "caption:" .. title,
                width = cap.width,
                padding = { left = 30, right = 30, top = 24, bottom = 24 },
                spacing = 12,
                radius = 18,
                background = theme.fade("crust", "e6"),
                shadows = layout.SHADOWS,
                opacity = 1,
                translate = { x = 0, y = 0 },
                animate = {
                    opacity = { duration = 350, from = 0 },
                    translate = { duration = 500, easing = "out_cubic", from = { x = 0, y = 30 } },
                },
                children = {
                    text {
                        content = title,
                        width = cap.width and "fill" or nil,
                        wrap = cap.width and "word" or nil,
                        font_size = cap.title,
                        font_weight = 800,
                        foreground = theme.text,
                    },
                    text {
                        content = detail,
                        visible = detail:map(function(d) return d ~= "" end),
                        width = cap.width and "fill" or nil,
                        wrap = cap.width and "word" or nil,
                        font_size = cap.detail,
                        foreground = theme.subtext,
                    },
                    row {
                        visible = computed({ keys, key_command }, function(k, c) return k ~= "" or c ~= "" end),
                        margin = { top = 6 },
                        spacing = 10,
                        children = key_row,
                    },
                },
            } },
        }
    end),
}

-- Title and end cards -----------------------------------------------------------------------

local function line_of(content, size, color, font)
    return text { content = content, align_h = "center", font = font, font_size = size, foreground = color }
end

local card_lines = {
    title = {
        image { source = WORDMARK, width = 191, height = 160, fit = "contain", align_h = "center" },
        line_of("Mantle", 132, theme.text),
        line_of("Desktop shells in Lua, on Wayland.", 44, theme.subtext),
    },
    ["end"] = {
        image { source = WORDMARK, width = 143, height = 120, fit = "contain", align_h = "center" },
        line_of("Write your shell in Lua.", 64, theme.text),
        line_of("anasgets111.github.io/mantle", 34, theme.accent, MONO),
        line_of("github.com/anasgets111/mantle", 30, theme.subtext, MONO),
        line_of("This video is a Mantle shell too.", 24, theme.muted),
    },
}

M.wallpaper = panel {
    id = "wallpaper",
    layer = "background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    child = image { source = backdrop, width = "fill", height = "fill", fit = "cover" },
}

M.card_pane = panel {
    id = "card",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    visible = card:map(function(kind) return kind ~= "" end),
    child = card:map(function(kind)
        return rect {
            id = "card:" .. kind,
            width = "fill",
            height = "fill",
            opacity = card_shown:map(function(on) return on and 1 or 0 end),
            animate = { opacity = { duration = 700, easing = "out_cubic", from = 0 } },
            children = {
                image { source = backdrop, width = "fill", height = "fill", fit = "cover" },
                rect { width = "fill", height = "fill", background = theme.fade("crust", "b8") },
                column {
                    align_h = "center",
                    align_v = "center",
                    spacing = 24,
                    children = card_lines[kind] or {},
                },
            },
        }
    end),
}

function M.unfocus()
    focus = nil
    bump()
end

function M.set_text(text)
    lines = edits.split(text)
    caret.line, caret.col = 1, 1
    focus = nil
    scroll_top(0)
    bump()
end

function M.play(ops, done)
    local k = 0
    local step
    local function apply(op)
        run.progress = run.progress + 1
        focus = op.focus
        caret.line, caret.col = edits.apply(lines, op)
        if op.kind == "paste_block" then flash:set({ line = op.line, count = #op.lines, serial = run.progress }) end
        reveal(caret.line)
        bump()
        timer(edits.pause(op, math.random(0, 26)), step)
    end
    step = function()
        k = k + 1
        local op = ops[k]
        if not op then return done() end
        if math.abs(op.line - caret.line) > 1 then
            caret.line, caret.col = math.min(op.line, #lines), op.col or 1
            reveal(caret.line)
            bump()
            timer(380, function() apply(op) end)
        else
            apply(op)
        end
    end
    step()
end

M.MONO, M.BAR, M.WALLPAPER, M.code_size, M.pane_width = MONO, BAR, WALLPAPER, code_size, pane_width
M.status, M.caption, M.detail, M.keys, M.key_command = status, caption, detail, keys, key_command
M.card, M.card_shown, M.meter, M.file_shown = card, card_shown, meter, file_shown
M.stage_full, M.flash, M.backdrop = stage_full, flash, backdrop
return M
