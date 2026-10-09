-- Records the demo video: types each file in `stages/` into a code pane, saves it into a scratch
-- shell that reloads on camera, and captions each beat. `just demo` runs it. It stops every other
-- running shell for the take and starts them again after, from `session.lua`'s restore list.

local syntax = require("lua_syntax")
local edits = require("edits")
local session = require("session")
local mockups = require("mockups")
local theme = require("theme")
local layout = require("layout")
local takes = require("takes")

fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local MONO = "CaskaydiaCove Nerd Font Mono"
local GUTTER = 6
-- The demo bar's final height, cleared through its exclusive zone.
local BAR = 56
local HEADER = 64
-- A tight key shadow under a wide ambient one; the surface needs a 64 px frame to hold them.
local SHADOWS = {
    { color = "#00000055", blur = 48, offset = { y = 18 } },
    { color = "#00000040", blur = 6,  offset = { y = 2 } },
}

local frame = mantle.screens:map(function(screens)
    return layout.metrics(screens and screens[1])
end)
local code_size = frame:map(function(m) return m.font end)
local line_px = frame:map(function(m) return m.line end)
local pane_width = frame:map(function(m) return m.pane end)
local row_count = frame:map(function(m)
    return math.max(1, math.floor((m.height - BAR - 32 - HEADER) / m.line))
end)

local function env(name, fallback)
    local value = os.getenv(name)
    return (value and value ~= "") and value or fallback
end

local DEMO_DIR = env("MANTLE_DEMO_DIR", env("XDG_RUNTIME_DIR", "/tmp") .. "/mantle-demo")
local OUT = env("MANTLE_DEMO_OUT", env("HOME", "") .. "/Videos/mantle-demo.mp4")
-- A preview records nothing: it plays every step before edit FROM at once behind the title card,
-- stops at the edit after TO, and with SHOTS set saves a screenshot after each save and each beat.
local FROM = env("MANTLE_DEMO_FROM", nil)
local TO = env("MANTLE_DEMO_TO", nil)
local SHOTS = env("MANTLE_DEMO_SHOTS", nil)
local STAGES = mantle.config_dir .. "/stages/"
local WORDMARK = mantle.config_dir .. "/../../docs/theme/m.png"
local WALLPAPER = mantle.config_dir .. "/wallpaper.svg"
local POINTER = mantle.config_dir .. "/pointer.svg"
local COVERS = mantle.config_dir .. "/covers"
-- Rendered to PNG in the demo's `wallpapers/` at setup: `palette.quantize` reads no SVG. `mantle`
-- is the logo art the take opens on, so the last pick comes back to it.
local WALLPAPERS = {}
for name, svg in pairs(takes.wallpapers) do
    WALLPAPERS[name] = mantle.config_dir .. "/" .. svg
end
local WALLPAPER_ASPECT = 3440 / 1440

-- The buffer lives in these locals; `version` is the signal that says it changed.
local lines = {}
local caret = { line = 1, col = 1 }
-- Lines outside the playing hunk dim while an edit types; the save restores them.
local focus
local texts = {}
-- Bumped by every script step and keystroke; the watchdog ends a take that stops moving.
local progress, step_at, finished = 0, 0, false

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

-- Code pane ---------------------------------------------------------------------------------

-- Syntax tokens per line text, warmed in chunks behind the title card: tokenizing a whole file in
-- one recompute overruns the 2.5 ms budget. Coloured and dimmed runs per line, dropped with the theme.
local tokens, highlighted, dimmed, highlighted_for = {}, {}, {}, nil

local function tokenize(line)
    local runs = tokens[line] or syntax.highlight(line)
    tokens[line] = runs
    return runs
end

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
            for _, run in ipairs(tokenize(line)) do
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

local code_pane = panel {
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

local caption_pane = panel {
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
                shadows = SHADOWS,
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

local wallpaper = panel {
    id = "wallpaper",
    layer = "background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    child = image { source = backdrop, width = "fill", height = "fill", fit = "cover" },
}

local card_pane = panel {
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

-- Script ------------------------------------------------------------------------------------

-- Set while a preview plays the steps before its FROM edit behind the title card: waits,
-- keystrokes and glides complete at once, so mock state still matches a full take.
local fast = false

-- `timer`, except fast-forwarding takes 1 ms: a fresh callback, so long chains never nest.
local function later(ms, fn)
    timer(fast and 1 or ms, fn)
end

-- Runs `mantle -c DEMO_DIR <args>` against the demo shell. A step right after a save can name
-- state or an action the edit adds before the reload lands, so a refusal, or output `accept`
-- rejects, retries for up to 3 s. `done` gets what `accept` returned last.
local function demo(args, done, accept)
    local tries = 0
    local function go()
        session.run("mantle", { "-c", DEMO_DIR, table.unpack(args) }, function(code, out)
            tries = tries + 1
            local ok = code == 0
            if accept then ok = accept(code, out or {}) end
            if not ok and tries < 30 and not finished then return timer(100, go) end
            if done then done(code, out, ok) end
        end)
    end
    go()
end

local function set_text(text)
    lines = edits.split(text)
    caret.line, caret.col = 1, 1
    focus = nil
    scroll_top(0)
    bump()
end

local function pause_after(op)
    return edits.pause(op, math.random(0, 26))
end

local function play(ops, done)
    local k = 0
    local step
    local function apply(op)
        progress = progress + 1
        focus = op.focus
        caret.line, caret.col = edits.apply(lines, op)
        if op.kind == "paste_block" then flash:set({ line = op.line, count = #op.lines, serial = progress }) end
        reveal(caret.line)
        bump()
        timer(pause_after(op), step)
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

local function wait(ms)
    return function(next) later(ms, next) end
end

-- With MANTLE_DEMO_SHOTS, captures the screen as `name`.png, then calls `done`.
local function shot(name, done)
    done = done or function() end
    if not SHOTS or fast then return done() end
    local screen = mantle.screens:get()[1]
    local args = { SHOTS .. "/" .. name .. ".png" }
    if screen and screen.name ~= "" then args = { "-o", screen.name, args[1] } end
    session.run("grim", args, function(code)
        if code ~= 0 then log.warn("grim could not save", args[#args]) end
        done()
    end)
end

-- Names each caption's shot: the last edit saved, and a count, as one edit spans several beats.
local last_edit, beats = takes.starter, 0

local function say(title, text)
    return function(next)
        if fast then return next() end
        beats = beats + 1
        shot(string.format("%s-end-%02d", last_edit, beats), function()
            log.info("beat", title)
            caption:set(title)
            detail:set(text or "")
            next()
        end)
    end
end

-- Each edit's file, texts and keystrokes, planned in `prepare` from a process callback: a diff
-- overruns the 2.5 ms budget a timer callback gets.
local plans = {}
-- The edit each `edit()` step plays, so a preview can find where to start.
local edit_steps = {}

local function prepare()
    for _, step in ipairs(takes.timeline(texts)) do
        plans[step.take.name] = {
            file = step.file,
            before = step.before,
            after = step.after,
            ops = edits.plan(step.before, step.after, step.take),
        }
    end
end

-- Tokenizes every line the take shows, 40 a callback.
local function warm(done)
    local shown, pending = { texts[takes.starter] }, {}
    for _, plan in pairs(plans) do
        shown[#shown + 1] = plan.after
    end
    for _, text in ipairs(shown) do
        local split = edits.split(text)
        table.move(split, 1, #split, #pending + 1, pending)
    end
    local function at(k)
        if k > #pending then return done() end
        for i = k, math.min(k + 39, #pending) do
            tokenize(pending[i])
        end
        timer(1, function() at(k + 40) end)
    end
    at(1)
end

local set_stage

-- Plays edit `name` in its file's buffer, switching the pane to that file first, then saves it.
-- Fast-forwarding, it waits for the reload, so later toggles and feeds land as in a full take.
local function edit(name)
    local function step(next)
        local plan = plans[name]
        if file_shown:get() ~= plan.file then
            file_shown:set(plan.file)
            set_text(plan.before)
        end
        local function save()
            last_edit, focus = name, nil
            bump()
            session.write(DEMO_DIR .. "/" .. plan.file, plan.after, function()
                status:set("saved")
                if fast then return timer(500, next) end
                if SHOTS then timer(900, function() shot(name .. "-saved") end) end
                next()
            end)
        end
        if fast then
            set_text(plan.after)
            return set_stage(false, save)
        end
        status:set("unsaved")
        set_stage(false, function() play(plan.ops, function() timer(450, save) end) end)
    end
    edit_steps[step] = name
    return step
end

local function toggle(name)
    return function(next)
        demo({ "toggle", name }, function() next() end)
    end
end

-- What the demo shell was last fed and the wallpaper last picked: a renderer killed on camera
-- comes back with neither, and `replay_mocks` pushes them again.
local last_fed, picked = {}, nil

-- Picks a wallpaper as a keybind would. The director
-- quantizes the same thumbnail the demo shell does, so its own panes re-theme in step.
local function pick(file)
    return function(next)
        picked = file
        backdrop:set(DEMO_DIR .. "/wallpapers/" .. file)
        theme.choose(DEMO_DIR .. "/wallpapers/thumbs/" .. file)
        demo({ "call", "wallpaper", file }, function() next() end)
    end
end

-- Mock feeds -------------------------------------------------------------------------------

local function feed(name, value)
    return function(next)
        last_fed[name] = value
        demo({ "set", name, json.encode(value) }, function() next() end)
    end
end

-- The take's windows in `mantle.windows`' shape: a terminal and an editor, then the mock apps. The
-- browser's title follows the page its mockup shows.
local WINDOWS = {
    { id = "0xa1", app_id = "kitty",                title = "~/Work/mantle" },
    { id = "0xa2", app_id = "dev.zed.Zed",          title = "overview.lua" },
    { id = "0xa3", app_id = "org.telegram.desktop", title = "Telegram" },
    { id = "0xa4", app_id = "zen",                  title = "Zen Browser" },
}
local APP_WINDOW = { [""] = "0xa1", chat = "0xa3", call = "0xa4", browser = "0xa4" }
local BROWSER_TITLE = { call = "Meet · Weekly sync", browser = "Aurora timelapse · 4K" }
local windows = { count = 0, focused = "0xa1", page = "" }

local function feed_windows(next)
    local out = {}
    for k = 1, windows.count do
        local w = WINDOWS[k]
        local title = w.id == "0xa4" and BROWSER_TITLE[windows.page] or w.title
        out[k] = { id = w.id, app_id = w.app_id, title = title, focused = w.id == windows.focused }
    end
    feed("mock_windows", { source = "hyprland", windows = out })(next)
end

-- Opens one window more, focused, as an app starting would.
local function launch(next)
    windows.count = windows.count + 1
    windows.focused = WINDOWS[windows.count].id
    feed_windows(next)
end

-- Moves focus without opening anything: the overview's selection.
local function select_window(id)
    return function(next)
        windows.focused = id
        feed_windows(next)
    end
end

-- Opens mockup `id` ("" closes it) and focuses its window, so the taskbar follows the take.
local function open_app(id)
    return function(next)
        windows.focused = APP_WINDOW[id]
        if BROWSER_TITLE[id] then windows.page = id end
        feed_windows(function() mockups.open(id)(next) end)
    end
end

local function notify(n)
    return function(next)
        local entry = {
            id = n.id,
            app_name = "Telegram",
            app_icon = "org.telegram.desktop",
            desktop_entry = "org.telegram.desktop",
            summary = n.from,
            body = n.body or { { kind = "text", text = n.text } },
            actions = { { key = "read", label = n.read } },
            has_default_action = true,
            has_reply = true,
            reply_placeholder = n.placeholder,
            urgency = "normal",
            expired = false,
            transient = false,
            timestamp = 0,
        }
        demo({ "call", "reply", "" }, function()
            feed("mock_notifications", { dnd = false, feed = { entry } })(next)
        end)
    end
end

-- Types `text` one character at a time through `mantle call name`, as a field `set_text` fills.
local function type_call(name, text)
    return function(next)
        if fast then return demo({ "call", name, text }, function() next() end) end
        local chars = {}
        for c in text:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
            chars[#chars + 1] = c
        end
        local function at(k)
            progress = progress + 1
            if k > #chars then return next() end
            demo({ "call", name, table.concat(chars, "", 1, k) }, function()
                timer(32 + math.random(0, 36) + (chars[k] == " " and 20 or 0), function() at(k + 1) end)
            end)
        end
        at(1)
    end
end

local function clear_search(next)
    demo({ "call", "search", "" }, function() next() end)
end

-- A password's length only, one key at a time: the mocks draw dots, never text.
local function type_dots(name, count)
    return function(next)
        if fast then return feed(name, count)(next) end
        local function at(k)
            progress = progress + 1
            if k > count then return next() end
            feed(name, k)(function() timer(70 + math.random(0, 50), function() at(k + 1) end) end)
        end
        at(1)
    end
end

local TRACKS = {
    {
        title = "Night Signals",
        artist = "Low Orbit",
        album = "Chevrons",
        art = "night-signals.svg",
        length = 214,
    },
    { title = "Warm Reload", artist = "Save State", album = "Hot Path", art = "warm-reload.svg", length = 187 },
}

local function player(k, at, play_state)
    local track = TRACKS[k]
    return {
        players = {
            {
                id = "spotify",
                identity = "Spotify",
                title = track.title,
                artist = track.artist,
                album = track.album,
                album_art_path = DEMO_DIR .. "/covers/" .. track.art,
                length = track.length * 1000000,
                position = at * 1000000,
                play_state = play_state,
            },
        },
    }
end

local TRAY = {
    items = {
        { id = "1", name = "Steam",    icon_name = "steam",                status = "active" },
        { id = "2", name = "Vesktop",  icon_name = "vesktop",              status = "active" },
        { id = "3", name = "Telegram", icon_name = "org.telegram.desktop", status = "active" },
    },
}

local PACKAGES = {
    { name = "linux",      old_version = "7.2.7.arch1-1", new_version = "7.2.8.arch1-1" },
    { name = "mesa",       old_version = "1:26.1.2-1",    new_version = "1:26.1.3-1" },
    { name = "mantle-git", old_version = "r1830",         new_version = "r1842" },
}

local function updates_state(step)
    if step == nil then return { packages = PACKAGES, installing = false } end
    if step > #PACKAGES then return { packages = {}, installing = false } end
    return {
        packages = {},
        installing = true,
        install_current_step = step,
        install_total_steps = #PACKAGES,
        install_current_package = PACKAGES[step].name,
    }
end

local function install(next)
    local function at(step)
        progress = progress + 1
        feed("mock_updates", updates_state(step))(function()
            if step > #PACKAGES then return next() end
            later(500, function() at(step + 1) end)
        end)
    end
    at(1)
end

local function lock_state(fields)
    local out = { active = true, attempts = 0, error = "", unlocking = false }
    for k, v in pairs(fields or {}) do
        out[k] = v
    end
    return out
end

-- Runs `mantle call name arg` on the demo shell, as a keybind would.
local function call(name, arg)
    return function(next) demo({ "call", name, arg }, function() next() end) end
end

local function privacy_users(camera, mic, screen)
    local function users(on) return on and { { app_name = "Meet" } } or {} end
    return { camera_users = users(camera), microphone_users = users(mic), screencast_users = users(screen) }
end

-- Pointer ------------------------------------------------------------------------------------

-- A drawn pointer, so a click the director fakes reads as one. The tip is the image's top left.
local pointer = state("demo_pointer", { x = 0, y = 0, shown = false })
local pointer_clicks = state("demo_pointer_clicks", 0)
local pressed = pulse(pointer_clicks, 320)
local pointer_at = pointer:map(function(p) return { x = p.x, y = p.y } end)
local spot = state("demo_spot", false)

local pointer_pane = panel {
    id = "pointer",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    visible = computed({ pointer, spot }, function(p, s) return p.shown or s ~= false end),
    child = rect {
        width = "fill",
        height = "fill",
        children = {
            rect {
                children = spot:map(function(s)
                    if not s then return {} end
                    return { rect {
                        id = "spot:" .. s.serial,
                        width = s.width + 16,
                        height = s.height + 16,
                        radius = 14,
                        border_width = 3,
                        border_color = theme.accent,
                        translate = { x = s.x - 8, y = s.y - 8 },
                        opacity = 0,
                        animate = { opacity = { duration = 400, keyframes = { 1, { value = 1, duration = 1600 }, 0 } } },
                    } }
                end),
            },
            rect {
                width = 44,
                height = 44,
                radius = 22,
                border_width = 3,
                border_color = theme.accent,
                translate = pointer:map(function(p) return { x = p.x - 22, y = p.y - 22 } end),
                opacity = pressed:map(function(on) return on and 1 or 0 end),
                scale = pressed:map(function(on) return on and 1 or 0.4 end),
                animate = {
                    translate = { duration = 600, easing = "in_out_cubic" },
                    opacity = 220,
                    scale = { duration = 320, easing = "out_cubic" },
                },
            },
            image {
                visible = pointer:map(function(p) return p.shown end),
                source = POINTER,
                width = 34,
                height = 44,
                translate = pointer_at,
                scale = pressed:map(function(on) return on and 0.86 or 1 end),
                origin = { x = 0, y = 0 },
                shadows = { { color = "#00000066", blur = 8, offset = { y = 2 } } },
                animate = {
                    translate = { duration = 600, easing = "in_out_cubic" },
                    scale = { duration = 160, easing = "out_cubic" },
                },
            },
        },
    },
}

-- Each demo surface's top-left on screen. The bar's exclusive zone adds BAR on top of the
-- margin `layout` gives the popup.
local function origin_of(surface)
    if surface == "bar" then return { x = 0, y = 0 } end
    local box = layout.popup(surface, layout.stage(mantle.screens:get(), stage_full:get()))
    return { x = box.left, y = BAR + box.top }
end

-- Hands `found` the demo shell's node `name` on `surface` as a box on screen, or calls `next`. A
-- node not laid out yet, as on a popup just opened, gets 1.5 s.
local function locate(name, surface, next, found, tries)
    if fast then return next() end
    demo({ "call", "where", name }, function(code, out)
        local ok, box = pcall(json.decode, table.concat(out or {}, "\n"))
        if code ~= 0 or not ok or type(box) ~= "table" or not box.width then
            if (tries or 0) < 15 then
                return timer(100, function() locate(name, surface, next, found, (tries or 0) + 1) end)
            end
            log.warn("no pointer target", name)
            return next()
        end
        local o = origin_of(surface)
        found({ x = o.x + box.x, y = o.y + box.y, width = box.width, height = box.height })
    end)
end

-- Rings node `name` on `surface` for a moment, to draw the eye to a payoff that is small.
local function spotlight(name, surface)
    return function(next)
        locate(name, surface, next, function(box)
            box.serial = progress
            spot:set(box)
            next()
        end)
    end
end

-- Glides the pointer onto the demo shell's node `name` on `surface`, then clicks. It maps again
-- first: a surface mapped later stacks above it, and the demo's popups open after it shows.
local function point(name, surface)
    return function(next)
        locate(name, surface, next, function(box)
            local x, y = box.x + box.width / 2, box.y + box.height / 2
            local last = pointer:get()
            local from = last.x ~= 0 and last or { x = x + 180, y = y + 240 }
            pointer:set({ x = from.x, y = from.y, shown = false })
            timer(34, function()
                pointer:set({ x = from.x, y = from.y, shown = true })
                timer(50, function()
                    pointer:set({ x = x, y = y, shown = true })
                    timer(640, function()
                        pointer_clicks:set(pointer_clicks:get() + 1)
                        timer(240, next)
                    end)
                end)
            end)
        end)
    end
end

local function hide_pointer(next)
    spot:set(false)
    local p = pointer:get()
    pointer:set({ x = p.x, y = p.y, shown = false })
    next()
end

-- Drags notification `id` right past the dismiss threshold: the fake pointer cannot drag, so the
-- glide and the card's `notif_drag` offset move together, eased as the pointer is.
local function swipe(id)
    return function(next)
        local function done()
            feed("mock_notifications", { dnd = false, feed = {} })(function()
                feed("notif_drag", { id = "", x = 0 })(next)
            end)
        end
        locate("notification", "notifications", done, function(box)
            -- Short of the module's 40% threshold, so the card is seen moving before it lets go.
            local x, y, reach = box.x + box.width * 0.3, box.y + box.height / 2, box.width * 0.36
            pointer:set({ x = x, y = y, shown = true })
            timer(700, function()
                pointer_clicks:set(pointer_clicks:get() + 1)
                pointer:set({ x = x + reach, y = y, shown = true })
                local function at(k)
                    if k > 12 then
                        pointer:set({ x = x + box.width * 0.6, y = y, shown = true })
                        return feed("notif_drag", { id = id, x = math.floor(box.width * 0.5) })(function()
                            timer(350, function() hide_pointer(done) end)
                        end)
                    end
                    local t = k / 12
                    local eased = t < 0.5 and 4 * t ^ 3 or 1 - (2 - 2 * t) ^ 3 / 2
                    feed("notif_drag", { id = id, x = math.floor(reach * eased) })(function()
                        timer(k == 12 and 250 or 30, function() at(k + 1) end)
                    end)
                end
                at(1)
            end)
        end)
    end
end

-- Agent terminal ----------------------------------------------------------------------------

-- What a coding agent would run, over the code pane: typed commands and the demo shell's real
-- output, `false` while hidden.
local term = state("demo_term", false)

local agent_pane = panel {
    id = "agent",
    layer = "top",
    anchor = { top = true, right = true, bottom = true },
    margin = { top = 16, right = 16, bottom = 16 },
    width = pane_width,
    height = "fill",
    visible = term:map(function(t) return t ~= false end),
    child = column {
        width = "fill",
        height = "fill",
        padding = 28,
        spacing = 8,
        background = theme.crust,
        radius = 18,
        opacity = 1,
        animate = { opacity = { duration = 300, from = 0 } },
        children = computed({ term, code_size }, function(rows, size)
            local out = { text { content = "agent · fish", font = MONO, font_size = 20, foreground = theme.muted } }
            for _, r in ipairs(rows or {}) do
                out[#out + 1] = rect {
                    margin = { top = r.cmd and 16 or 0 },
                    padding = { left = 8, right = 8 },
                    radius = 6,
                    background = r.hl and theme.accent or nil,
                    children = { text {
                        content = r.text,
                        font = MONO,
                        font_size = size,
                        foreground = r.hl and theme.crust or r.cmd and theme.accent or theme.text,
                    } },
                }
            end
            return out
        end),
    },
}

-- Types `shown` into the terminal, runs `args` on the demo shell and prints the rows `pick` makes
-- of its output, or nil to reject it. A reload can still be landing, so a rejection retries for
-- 3 s; past that it prints `canned` and warns, never an error or a stall on camera.
local function agent(shown, args, pick, canned)
    return function(next)
        if fast then return next() end
        local rows = term:get() or {}
        local row = { cmd = true }
        rows[#rows + 1] = row
        local function put() term:set({ table.unpack(rows) }) end
        local function key(i)
            row.text = "$ " .. shown:sub(1, i)
            put()
            if i < #shown then return later(30, function() key(i + 1) end) end
            demo(args, function(code, out, ok)
                local picked = ok and pick(code, out or {})
                if not picked then log.warn("agent beat fallback: " .. shown) end
                for _, r in ipairs(picked or canned) do
                    rows[#rows + 1] = r
                end
                put()
                next()
            end, function(code, out) return pick(code, out) ~= nil end)
        end
        later(200, function() key(1) end)
    end
end

-- Clicks the bar's Focus chip the way an agent would, at the box the pointer aims at: on the bar,
-- screen and surface pixels agree. Not found, it warns and turns focus off itself.
local function agent_click(next)
    if fast then return next() end
    locate("focus", "bar", function()
        log.warn("agent beat fallback: click")
        feed("focus_on", false)(next)
    end, function(box)
        local x, y = math.floor(box.x + box.width / 2), math.floor(box.y + box.height / 2)
        agent(string.format("mantle input bar click %d %d", x, y), { "input", "bar", "click", x, y },
            function(code) return code == 0 and {} or nil end, {})(next)
    end)
end

local AGENT_BEAT = {
    agent("mantle call", { "call" }, function(code, out)
        local rows, found = {}, false
        for _, line in ipairs(out) do
            rows[#rows + 1] = { text = line, hl = line == "focus" }
            found = found or line == "focus"
        end
        return code == 0 and found and rows or nil
    end, { { text = "focus", hl = true }, { text = "reply" }, { text = "search" } }),
    wait(900),
    agent("mantle check", { "check" }, function(code, out)
        local line = code == 0 and table.concat(out, "\n"):match("[^/\n]*: ok, %d+ surface%(s%)")
        return line and { { text = line } } or nil
    end, { { text = "shell.lua: ok, 13 surface(s)" } }),
    wait(700),
    agent("mantle call focus", { "call", "focus" }, function(code, out)
        return code == 0 and out[1] == "true" and { { text = "true" } } or nil
    end, { { text = "true" } }),
    wait(1100),
    agent("mantle log | grep focus", { "log" }, function(code, out)
        local last = ("\n" .. table.concat(out, "\n")):match(".*\n([^\n]*config: focus%s+on)")
        return code == 0 and last and { { text = (last:gsub("%s+", " ")) } } or nil
    end, { { text = "12:00:00 INFO renderer/config: focus on" } }),
    wait(900),
    spotlight("focus", "bar"),
    agent_click,
    hide_pointer,
    wait(1200),
}

local finish

-- Cost meter -------------------------------------------------------------------------------

-- A shell's Supervisor, `$1`, and its `mantle-renderer` child in `$r`: the director's other
-- children are the demo shell and the recorder.
local RENDERER_SCRIPT = [[
cd /proc || exit 1
renderer() {
    r=$(for c in $(cat "$1"/task/*/children 2>/dev/null); do
        [ "$(cat "$c/comm" 2>/dev/null)" = mantle-renderer ] && echo "$c"
    done)
}
renderer "$1"
]]
-- Real, unlike the mocks: proportional memory and CPU of the demo shell's two processes, from
-- /proc, then the renderer's pid: a respawned one restarts its tick count.
local METER_SCRIPT = RENDERER_SCRIPT .. [[
set -- "$1" $r
for p; do cat "$p/smaps_rollup"; done 2>/dev/null | awk '/^Pss:/ { s += $2 } END { print s + 0 }'
for p; do cat "$p/stat"; done 2>/dev/null | awk '{ t += $14 + $15 } END { print t + 0 }'
echo "$r"
]]
local METER_MS = 2000
local CLK_TCK = 100
-- The last reading, and a step waiting for the next one with a CPU figure.
local usage, on_sample

local function sample()
    local pid = session.demo_shell.pid:get()
    if not (pid and session.demo_shell.running:get()) then return end
    session.run("sh", { "-c", METER_SCRIPT, "sh", tostring(pid) }, function(_, out)
        local kb, ticks, renderer = tonumber(out[1]), tonumber(out[2]), out[3]
        if not (kb and kb > 0 and ticks) then return end
        local same = usage and usage.pid == pid and usage.renderer == renderer
        local cpu = same and (ticks - usage.ticks) * 100000 / (CLK_TCK * METER_MS) or nil
        usage = { pid = pid, renderer = renderer, mb = kb / 1024, ticks = ticks, cpu = cpu }
        if not cpu then return end
        meter:set(string.format("demo shell  %d MB  ·  %.1f%% CPU", math.floor(usage.mb + 0.5), cpu))
        local waiting = on_sample
        on_sample = nil
        if waiting then waiting(usage) end
    end)
end

-- Crash beat --------------------------------------------------------------------------------

-- Kills renderer `$3` of demo shell `$2` and prints the ms until a new one answers `mantle call`;
-- fails after 5 s.
local RESPAWN_SCRIPT = RENDERER_SCRIPT .. [[
[ "$r" = "$3" ] || exit 1
t0=$(date +%s%N)
kill -9 "$3" || exit 1
for _ in $(seq 250); do
    renderer "$1"
    if [ -n "$r" ] && [ "$r" != "$3" ] && mantle -c "$2" call where bar >/dev/null 2>&1; then
        echo $((($(date +%s%N) - t0) / 1000000))
        exit 0
    fi
    sleep 0.02
done
exit 1
]]

local function renderer_pid(done)
    local script = RENDERER_SCRIPT .. 'echo "$r"'
    session.run("sh", { "-c", script, "sh", tostring(session.demo_shell.pid:get()) }, function(_, out)
        done(tonumber(out[1]))
    end)
end

-- Pushes every mock again, the wallpaper last: a respawned renderer starts with no named state.
local function replay_mocks(next)
    local names = {}
    for name in pairs(last_fed) do
        names[#names + 1] = name
    end
    local function at(k)
        if names[k] then return feed(names[k], last_fed[names[k]])(function() at(k + 1) end) end
        if not picked then return next() end
        demo({ "call", "wallpaper", picked }, function() next() end)
    end
    at(1)
end

-- `kill -9` on the demo shell's renderer, captioned with the real pid, then with the real time the
-- Supervisor took to bring a new one up; the mocks go back in after. No respawn in 5 s ends the take.
local function kill_renderer(next)
    if fast then return next() end
    renderer_pid(function(pid)
        if not pid then
            log.warn("no demo renderer to kill")
            return next()
        end
        key_command:set("kill -9 " .. pid)
        timer(1200, function()
            local args = { "-c", RESPAWN_SCRIPT, "sh", tostring(session.demo_shell.pid:get()), DEMO_DIR, tostring(pid) }
            session.run("sh", args, function(code, out)
                local ms = tonumber(out[1])
                if code ~= 0 or not ms then
                    log.error("the demo renderer did not come back; ending the take")
                    return finish()
                end
                detail:set(string.format("Back in %d ms: the supervisor respawned the renderer.", ms))
                replay_mocks(next)
            end)
        end)
    end)
end

-- The output the recorder captures.
local function monitor_name()
    local screen = mantle.screens:get()[1]
    return env("MANTLE_DEMO_MONITOR", screen and screen.name or "screen")
end

-- The recorded output's workspaces, from the compositor.
local function stage_output()
    local ws, name = mantle.workspaces:get(), monitor_name()
    for _, output in ipairs(ws and ws.outputs or {}) do
        if output.name == name then return output, ws.compositor end
    end
end

-- The take runs on `stage_ws[1]`, where none of your windows are: on Hyprland two numbers no
-- workspace has yet, which focus creates; on niri the empty workspace it keeps last.
local stage_ws, origin_ws = {}, nil

local function pick_workspaces()
    local output, compositor = stage_output()
    stage_ws = {}
    if not output then return end
    origin_ws = origin_ws or output.active_workspace
    if compositor == "niri" then
        for _, w in ipairs(output.workspaces) do
            if not w.populated then stage_ws[1] = w.id end
        end
        return
    end
    -- Hyprland's ids are global: a number on any output is taken.
    local exists = {}
    for _, other in ipairs(mantle.workspaces:get().outputs) do
        for _, w in ipairs(other.workspaces) do
            exists[w.id] = true
        end
    end
    for number = 6, 99 do
        if not exists[tostring(number)] and #stage_ws < 2 then
            stage_ws[#stage_ws + 1] = tostring(number)
        end
    end
end

-- The tour's window: a terminal of its own app_id, opened on an empty workspace before recording
-- and closed after its hop, so no window of yours is ever in the shot.
-- ponytail: the take depends on kitty, fish and fastfetch; another app needs its own `--class`-style id.
local TOUR_ID = "mantle-demo-tour"
-- fish with fastfetch in place of its greeting, in $HOME so the prompt shows no checkout path.
local FETCH = "functions -e fish_greeting; fastfetch"
local TOUR = { "kitty", "--class", TOUR_ID, "--directory", env("HOME", "/"), "-e", "fish", "-C", FETCH }
local tour_ws

local function tour_windows()
    local out = {}
    for _, w in ipairs((mantle.windows:get() or { windows = {} }).windows) do
        if w.app_id == TOUR_ID then out[#out + 1] = w end
    end
    return out
end

local function close_tour()
    for _, w in ipairs(tour_windows()) do
        mantle.windows:close(w.id)
    end
end

-- Opens TOUR on niri's empty workspace or Hyprland's spare number, moving it there if it maps
-- elsewhere. Without it on that workspace in time, the take has no tour.
local function open_tour(done)
    tour_ws = stage_ws[2] or stage_ws[1]
    mantle.workspaces:focus(tour_ws)
    process.detach(TOUR[1], { table.unpack(TOUR, 2) })
    local moved
    local function placed()
        local w = tour_windows()[1]
        if w and w.workspace_id ~= tour_ws and moved ~= w.id then
            moved = w.id
            mantle.windows:move_to_workspace(w.id, tour_ws)
        end
        return w ~= nil and w.workspace_id == tour_ws
    end
    session.wait_for(placed, 8000, function(ok)
        if not ok then
            log.warn("no tour window on workspace", tour_ws, "; the take has no tour")
            close_tour()
            tour_ws = nil
        end
        done()
    end)
end

-- Why the recorded output must not be on camera, or nil: it shows a stage workspace holding no
-- window but the tour's.
local function exposed()
    local output = stage_output()
    if not output then return "no workspaces for " .. monitor_name() end
    local active = output.active_workspace
    if active ~= stage_ws[1] and active ~= tour_ws then return "workspace " .. active .. " is not a stage one" end
    local windows = mantle.windows:get()
    if not windows then
        for _, w in ipairs(output.workspaces) do
            if w.id == active and w.populated and active ~= tour_ws then return "a window is on the stage" end
        end
        return nil
    end
    for _, w in ipairs(windows.windows) do
        if w.workspace_id == active and w.app_id ~= TOUR_ID then return "window " .. w.id .. " is on the stage" end
    end
end

-- Set from just before the recorder starts until `finish`: any compositor change that exposes
-- one of your windows ends the take there and then.
local guarding = false

local function guard()
    local why = guarding and exposed()
    if not why then return end
    log.error("ending the take:", why)
    finish()
end

mantle.workspaces:on_change(guard)
mantle.windows:on_change(guard)

local function tour(next)
    if fast or not tour_ws then
        close_tour()
        return next()
    end
    mantle.workspaces:focus(tour_ws)
    timer(1200, function()
        mantle.workspaces:focus(stage_ws[1])
        timer(500, function()
            close_tour()
            local why = exposed()
            if why then
                log.error("ending the take:", why)
                return finish()
            end
            next()
        end)
    end)
end

-- Ends at the first edit after MANTLE_DEMO_TO's.
local function sequence(steps, done)
    local past_to = false
    local function at(k)
        if finished then return end
        progress, step_at = progress + 1, k
        local name = edit_steps[steps[k]]
        if k > #steps or (name and past_to) then return done() end
        past_to = past_to or (TO ~= nil and name == TO)
        steps[k](function() at(k + 1) end)
    end
    at(1)
end

local restore = {}
local volume_before

local function set_volume(level, done)
    session.run("wpctl", { "set-volume", "@DEFAULT_AUDIO_SINK@", string.format("%.2f", level) }, done)
end

-- The default output three steps and back, for the OSD beat, through `wpctl` like any other app;
-- `finish` puts it back if a take ends mid-nudge.
local function nudge_volume(next)
    if fast then return next() end
    session.run("wpctl", { "get-volume", "@DEFAULT_AUDIO_SINK@" }, function(_, out)
        volume_before = tonumber((out[1] or ""):match("Volume:%s*([%d%.]+)"))
        if not volume_before then return next() end
        local step = volume_before > 0.8 and -0.06 or 0.06
        local levels = { volume_before + step, volume_before + 2 * step, volume_before + 3 * step, volume_before }
        local function at(k)
            if not levels[k] then
                volume_before = nil
                return next()
            end
            set_volume(levels[k], function()
                timer(k == 3 and 900 or 350, function() at(k + 1) end)
            end)
        end
        at(1)
    end)
end

local function stopped(handle)
    return function() return handle.running:get() ~= true end
end

function finish()
    if finished then return end
    finished, guarding = true, false
    session.recorder:stop()
    if volume_before then set_volume(volume_before) end
    close_tour()
    session.wait_for(stopped(session.recorder), 10000, function()
        session.demo_shell:stop()
        session.wait_for(stopped(session.demo_shell), 8000, function()
            if origin_ws then mantle.workspaces:focus(origin_ws) end
            session.wait_for(function()
                local output = stage_output()
                return not origin_ws or (output ~= nil and output.active_workspace == origin_ws)
            end, 2000, function(back)
                if not back then log.warn("could not return to workspace", origin_ws) end
            end)
            session.restore_shells(restore)
            if not FROM then log.info("demo written to", OUT) end
            -- The restore list saves 1 s after its last write.
            timer(1500, function() session.run("mantle", { "stop", "--pid", tostring(mantle.pid) }) end)
        end)
    end)
end

-- Starts the recorder, and the guard, only while the stage shows nothing of yours.
local function record(next)
    local why = exposed()
    if why then
        log.error("not recording:", why)
        return finish()
    end
    guarding = true
    if FROM then return next() end
    local monitor = monitor_name()
    local folder = OUT:match("^(.*)/[^/]*$") or "."
    session.run("mkdir", { "-p", folder }, function()
        session.recorder:start("gpu-screen-recorder", {
            "-w", monitor, "-f", "60", "-cursor", "no", "-q", "very_high", "-o", OUT,
        })
        session.wait_for(function() return session.recorder.running:get() == true end, 5000, function(up)
            if not up then
                log.error("gpu-screen-recorder did not start:", session.recorder.start_error:get())
                return finish()
            end
            timer(1000, next)
        end)
    end)
end

local function setup(next)
    session.stop_others(DEMO_DIR, function(shells)
        restore = shells
        session.wait_for(function() return mantle.workspaces:get() ~= nil end, 3000, function()
            pick_workspaces()
            if not stage_ws[1] then return finish() end
            open_tour(function()
                -- niri's tour window took its empty workspace, so the stage is the new last one.
                session.wait_for(function()
                    pick_workspaces()
                    return stage_ws[1] ~= nil and stage_ws[1] ~= tour_ws
                end, 3000, function()
                    if stage_ws[1] then mantle.workspaces:focus(stage_ws[1]) end
                    -- Your windows must never reach the recording: no empty workspace, no take.
                    session.wait_for(function()
                        local output = stage_output()
                        return stage_ws[1] ~= nil and stage_ws[1] ~= tour_ws and output ~= nil
                            and output.active_workspace == stage_ws[1]
                    end, 3000, function(ok)
                        if ok then return next() end
                        log.error("could not focus an empty workspace; ending the take")
                        finish()
                    end)
                end)
            end)
        end)
    end)
end

-- Wide enough that `fit = "cover"` never upscales on a screen narrower than the art's aspect; the
-- 300 px copy is the picker tile and what `palette.quantize` decodes.
local function render_wallpapers(done)
    local screen = mantle.screens:get()[1] or { width = 1920, height = 1080 }
    local width = tostring(math.max(screen.width, math.ceil(screen.height * WALLPAPER_ASPECT)))
    local sizes = { [""] = width, ["thumbs/"] = "300" }
    local pending = 0
    for _ in pairs(WALLPAPERS) do
        pending = pending + 2
    end
    for name, svg in pairs(WALLPAPERS) do
        for dir, w in pairs(sizes) do
            local out = DEMO_DIR .. "/wallpapers/" .. dir .. name .. ".png"
            session.run("rsvg-convert", { "-w", w, "-o", out, svg }, function(code)
                if code ~= 0 then log.warn("rsvg-convert could not render", out) end
                pending = pending - 1
                if pending == 0 then done() end
            end)
        end
    end
end

-- Shows `text` as shell.lua and writes it with every module pruned to `played`, then starts the
-- demo shell on fresh state and gives it `settle` ms.
local function start_shell(text, played, settle, next)
    file_shown:set("shell.lua")
    set_text(text)
    local function at(k)
        local name = takes.modules[k]
        if name then
            return session.write(DEMO_DIR .. "/" .. name, takes.prune(texts[name], played), function() at(k + 1) end)
        end
        session.write(DEMO_DIR .. "/shell.lua", text, function()
            session.demo_shell:start("mantle", { "-c", DEMO_DIR })
            timer(settle, next)
        end)
    end
    -- The last take's saved switches would start this one with them on.
    session.run("rm", { "-rf", DEMO_DIR .. "/state" }, function() at(1) end)
end

-- The cold open's shell: the last stage with every module block in, beside fresh frags, theme,
-- layout, covers and wallpapers.
local function stage_final(next)
    local sources = { mantle.config_dir .. "/theme.lua", mantle.config_dir .. "/layout.lua" }
    for _, name in ipairs(takes.frags) do
        sources[#sources + 1] = STAGES .. name
    end
    sources[#sources + 1] = DEMO_DIR .. "/"
    if SHOTS then session.run("mkdir", { "-p", SHOTS }) end
    session.run("mkdir", { "-p", DEMO_DIR .. "/wallpapers/thumbs" }, function()
        session.run("cp", { "-r", COVERS, DEMO_DIR .. "/" }, function()
            session.run("cp", sources, function()
                render_wallpapers(function() start_shell(texts[takes.stages[#takes.stages]], nil, 2500, next) end)
            end)
        end)
    end)
end

local function hide_card(next)
    if fast then return next() end
    card_shown:set(false)
    timer(800, function()
        card:set("")
        next()
    end)
end

-- Clears what a take leaves on screen: keys, pointer, mock apps, pane, backdrop and theme.
local function reset_scene()
    keys:set("")
    key_command:set("")
    pointer:set({ x = 0, y = 0, shown = false })
    spot:set(false)
    flash:set(false)
    stage_full:set(false)
    mockups.app:set("")
    backdrop:set(WALLPAPER)
    theme.choose(DEMO_DIR .. "/wallpapers/thumbs/mantle.png")
end

-- Behind the title card: the demo shell starts over on the starter, and the director's scene and
-- mock records go back to where a take begins.
local function restart_starter(next)
    session.demo_shell:stop()
    session.wait_for(stopped(session.demo_shell), 8000, function(ok)
        if not ok then
            log.error("the demo shell did not stop; ending the take")
            return finish()
        end
        last_fed, picked = {}, nil
        windows.count, windows.focused, windows.page = 0, "0xa1", ""
        reset_scene()
        start_shell(texts[takes.starter], {}, 2000, function() hide_card(next) end)
    end)
end

local function show_card(kind)
    return function(next)
        caption:set("")
        card_shown:set(true)
        card:set(kind)
        next()
    end
end

local function press(combo, name)
    return function(next)
        if not fast then
            keys:set(combo)
            key_command:set("mantle toggle " .. name)
        end
        later(450, function()
            toggle(name)(function() later(500, next) end)
        end)
    end
end

-- Slides the code pane off, or back, and has every `layout.placed` box follow: the director's
-- mock windows here, the demo shell's popups through its `stage_full`. Surfaces do not animate
-- their margin or size, so the script calls it with every popup closed; edits bring the pane back.
function set_stage(full, next)
    if stage_full:get() == full then return next() end
    stage_full:set(full)
    feed("stage_full", full)(function() later(450, next) end)
end

local function full_stage(next) set_stage(true, next) end

local function clear_keys(next)
    keys:set("")
    key_command:set("")
    next()
end


-- Runs `steps` in order as one step.
local function chain(...)
    local steps = { ... }
    return function(next)
        local function at(k)
            if k > #steps then return next() end
            steps[k](function() at(k + 1) end)
        end
        at(1)
    end
end

-- Shows `text` in the caption as the command that runs the next step.
local function command(text)
    return function(next)
        key_command:set(text)
        next()
    end
end

-- The cold open on the final shell, its windows open and the code pane off. A preview plays it
-- behind the title card; a take opens on the shell itself.
local function cold_open(next)
    windows.count = #WINDOWS
    if not FROM then card:set("") end
    feed_windows(function() set_stage(true, next) end)
end

local function search(text, hold)
    return chain(toggle("launcher_open"), wait(250), type_call("search", text), wait(hold), clear_search,
        toggle("launcher_open"))
end

local function overview(hold)
    return chain(toggle("overview_open"), wait(hold), select_window("0xa3"), wait(hold), toggle("overview_open"))
end

local SARAH = {
    id = 1,
    from = "Sarah",
    body = {
        { kind = "text", text = "Still on for tonight? 8 pm at " },
        { kind = "text", text = "Luigi's",                       href = "https://maps.example.org/luigis" },
    },
    placeholder = "Reply to Sarah",
    read = "Mark as read",
}
local NO_NOTIFICATIONS = { dnd = false, feed = {} }

local script = {
    setup,
    stage_final,
    cold_open,
    record,
    say("Every pixel is Lua."),
    search("fi", 1000),
    toggle("picker_open"),
    wait(400),
    pick("ember.png"),
    wait(1300),
    toggle("picker_open"),
    notify(SARAH),
    wait(1600),
    feed("mock_notifications", NO_NOTIFICATIONS),
    overview(1000),
    show_card("title"),
    restart_starter,

    say("Save, and it's live.", "One Lua file. Mantle ships no shell."),
    edit("01-size"),
    wait(3200),
    say("Change a colour.", "No restart, no rebuild."),
    edit("02-color"),
    wait(3200),

    say("Live compositor state.", "mantle.workspaces: a signal the bar redraws from."),
    edit("03-workspaces"),
    wait(500),
    tour,
    wait(1200),

    say("A launcher, fuzzy search included.", "state() a keybind toggles; fuzzy() ranks."),
    edit("04-launcher"),
    press("Super+A", "launcher_open"),
    clear_keys,
    type_call("search", "tele"),
    wait(1500),
    clear_search,
    type_call("search", "files"),
    wait(1500),

    say("Reloads keep state.", "Restyled while open."),
    edit("05-restyle"),
    wait(1600),
    clear_search,
    toggle("launcher_open"),
    wait(300),

    say("Shaders, and glass over them.", "GLSL behind the desktop; blur that fades out."),
    edit("06-shader"),
    full_stage,
    wait(4200),

    say("Your wallpaper themes everything.", "palette.score picks a seed; palette.scheme paints."),
    edit("07-wallpaper"),
    full_stage,
    toggle("picker_open"),
    wait(500),
    point("thumb:ember.png", "picker"),
    pick("ember.png"),
    hide_pointer,
    wait(1900),
    command("mantle call wallpaper tide.png"),
    pick("tide.png"),
    wait(1900),
    command("mantle call wallpaper dusk.png"),
    pick("dusk.png"),
    wait(1900),
    clear_keys,
    toggle("picker_open"),
    wait(300),

    say("Windows, and live captures.", "mantle.windows and capture: the overview is Lua."),
    edit("08-windows"),
    full_stage,
    launch,
    wait(250),
    launch,
    wait(250),
    launch,
    wait(250),
    launch,
    wait(400),
    point("task:0xa3", "bar"),
    open_app("chat"),
    wait(900),
    hide_pointer,
    open_app(""),
    press("Super+Tab", "overview_open"),
    clear_keys,
    select_window("0xa2"),
    wait(800),
    select_window("0xa3"),
    wait(800),
    toggle("overview_open"),
    wait(300),

    say("Motion is a property.", "Morphs, springs and waves, eased by the engine."),
    edit("09-media"),
    edit("09-motion"),
    feed("mock_tray", TRAY),
    feed("mock_media", player(1, 61, "playing")),
    full_stage,
    toggle("media_open"),
    wait(400),
    point("media:play", "media"),
    feed("mock_media", player(1, 62, "paused")),
    wait(900),
    point("media:play", "media"),
    feed("mock_media", player(1, 62, "playing")),
    wait(700),
    hide_pointer,
    toggle("media_open"),
    wait(300),

    say("Quick settings, and an OSD.", "Network, Bluetooth, brightness; volume from any app."),
    edit("10-control"),
    full_stage,
    press("Super+C", "control_open"),
    clear_keys,
    feed("mock_network", { wifi_enabled = true, connected = false, strength = 0 }),
    feed("mock_bluetooth", { enabled = true, connected_devices = {} }),
    wait(500),
    feed("mock_network", { wifi_enabled = true, connected = true, ssid = "Home", strength = 82 }),
    feed("mock_bluetooth", { enabled = true, connected_devices = { { name = "WH-1000XM5", battery = 80 } } }),
    feed("mock_brightness", { percent = 85 }),
    point("control:dnd", "control"),
    call("setting", "dnd"),
    wait(700),
    point("control:dnd", "control"),
    call("setting", "dnd"),
    hide_pointer,
    toggle("control_open"),
    wait(300),
    nudge_volume,
    wait(1200),

    -- The aurora's two passes are over: a renderer respawned later starts still as well.
    feed("aurora_settled", true),
    say("Your notification server.", "Links, inline replies, any script, swipe away."),
    edit("11-notifications"),
    edit("11-links"),
    full_stage,
    notify(SARAH),
    wait(1100),
    type_call("reply", "On my way!"),
    wait(500),
    notify { id = 2, from = "أحمد", text = "وصلت؟ الكل بانتظارك", placeholder = "رد على أحمد", read = "تحديد كمقروء" },
    wait(700),
    type_call("reply", "خمس دقائق وأكون عندكم"),
    wait(700),
    notify { id = 3, from = "Mantle devs", text = "v0.9 is out.", placeholder = "Reply", read = "Mark as read" },
    wait(600),
    swipe(3),

    say("Know who's watching.", "mantle.privacy and mantle.idle in the bar."),
    edit("12-indicators"),
    full_stage,
    open_app("call"),
    feed("mock_privacy", privacy_users(true, true, false)),
    spotlight("privacy", "bar"),
    wait(2300),
    feed("mock_privacy", privacy_users(false, false, false)),
    open_app("browser"),
    feed("mock_idle", { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } }),
    spotlight("idle", "bar"),
    wait(1900),
    feed("mock_idle", { inhibited = false, inhibitors = {} }),
    open_app(""),

    say("Updates, with your polkit agent.", "The password prompt is Lua too."),
    edit("13-updates"),
    full_stage,
    feed("mock_updates", updates_state()),
    spotlight("updates", "bar"),
    point("updates", "bar"),
    toggle("updates_open"),
    wait(500),
    point("updates:install", "updates"),
    hide_pointer,
    feed("mock_polkit", {
        active = true,
        user = env("USER", "you"),
        message = "Authentication is required to update the system's packages.",
    }),
    wait(400),
    type_dots("polkit_typed", 6),
    wait(300),
    feed("mock_polkit", { active = false, user = env("USER", "you"), message = "" }),
    feed("polkit_typed", 0),
    install,
    wait(600),
    toggle("updates_open"),

    say("Real numbers, no polling code.", "mantle.sysinfo, drawn as trimmed paths."),
    edit("14-sysinfo"),
    wait(300),
    spotlight("sysinfo", "bar"),
    wait(2500),

    say("Even the lock screen.", "Same Lua, same theme. The take mocks ext-session-lock."),
    edit("15-lock"),
    feed("mock_lock", lock_state()),
    wait(600),
    type_dots("lock_typed", 6),
    feed("lock_typed", 0),
    feed("mock_lock", lock_state({ attempts = 1, error = "authentication failed" })),
    wait(1300),
    type_dots("lock_typed", 8),
    feed("mock_lock", lock_state({ attempts = 1, unlocking = true })),
    wait(500),
    feed("lock_typed", 0),
    feed("mock_lock", lock_state({ active = false })),
    wait(300),

    say("Built for agents, too.", "Actions, states, check and log: a CLI any coding agent can drive."),
    edit("16-agent"),
    wait(300),
    chain(table.unpack(AGENT_BEAT)),
    function(next)
        term:set(false)
        next()
    end,

    say("Break it on purpose.", "A typo never takes the desktop down."),
    edit("typo"),
    wait(3800),
    edit("fix"),
    wait(1200),

    say("Kill it. It comes back.", "kill -9 the renderer; the supervisor respawns it."),
    full_stage,
    wait(1200),
    kill_renderer,
    wait(4200),
    clear_keys,

    function(next)
        if fast then return next() end
        on_sample = function(u)
            say(string.format("Everything you saw: %d MB.", math.floor(u.mb + 0.5)),
                string.format("%.1f%% of one core, read live from /proc.", u.cpu))(next)
        end
    end,
    search("fi", 1800),
    pick("ember.png"),
    wait(2500),
    toggle("media_open"),
    wait(2500),
    toggle("media_open"),
    overview(1200),
    show_card("end"),
    wait(5000),
}

-- A callback that raises or overruns its budget is dropped, which would leave the take and the
-- recorder running with your shells stopped.
local function watchdog(seen)
    timer(20000, function()
        if finished then return end
        if progress == seen then
            log.error("demo step", step_at, "made no progress for 20 s; ending the take")
            return finish()
        end
        watchdog(progress)
    end)
end

-- Stage files by name, then modules by file name.
local SOURCES = {}
for _, name in ipairs(takes.stages) do
    SOURCES[#SOURCES + 1] = { name, name .. ".lua" }
end
for _, file in ipairs(takes.modules) do
    SOURCES[#SOURCES + 1] = { file, file }
end

-- One at a time: each output line is a frame, and all files at once overflow the Supervisor's
-- 1024-frame queue, which then drops this Renderer as wedged.
local function load_stages(done, k)
    k = k or 1
    local source = SOURCES[k]
    if not source then return done() end
    session.run("cat", { STAGES .. source[2] }, function(code, out)
        if code == 0 then texts[source[1]] = table.concat(out, "\n") .. "\n" end
        load_stages(done, k + 1)
    end)
end

-- A reload restarts the take from the top, so it first clears what the last one left running.
math.randomseed(7)
caption:set("")
reset_scene()
status:set("saved")
card:set("title")
card_shown:set(true)
meter:set("")
file_shown:set("shell.lua")
timer(1, function()
    session.recorder:stop()
    session.demo_shell:stop()
    session.wait_for(function() return stopped(session.recorder)() and stopped(session.demo_shell)() end, 8000,
        function()
            load_stages(function()
                for _, source in ipairs(SOURCES) do
                    if not texts[source[1]] then
                        log.error("could not read stage", source[2])
                        return finish()
                    end
                end
                local ok, err = pcall(prepare)
                if not ok then
                    log.error("could not plan the take:", err)
                    return finish()
                end
                -- Fast up to the edit before FROM, or the restart before the first edit, so its
                -- caption and setup play at speed.
                local at, first, to_at
                for k, step in ipairs(script) do
                    if edit_steps[step] == FROM then at = at or k end
                    if edit_steps[step] == TO then to_at = k end
                    if not at and (edit_steps[step] or step == restart_starter) then first = k end
                end
                if (FROM and not at) or (TO and (not to_at or to_at < (at or 0))) then
                    log.error("MANTLE_DEMO_FROM or MANTLE_DEMO_TO names no edit, or TO plays before FROM")
                    return finish()
                end
                if FROM then
                    fast = true
                    table.insert(script, first + 1, function(next)
                        fast = false
                        hide_card(next)
                    end)
                end
                watchdog(progress)
                sample()
                interval(METER_MS, sample)
                warm(function() sequence(script, finish) end)
            end)
        end)
end)

local surfaces = { wallpaper }
for _, window in ipairs(mockups.panels(backdrop)) do
    surfaces[#surfaces + 1] = window
end
for _, pane in ipairs({ card_pane, caption_pane, code_pane, agent_pane, pointer_pane }) do
    surfaces[#surfaces + 1] = pane
end
return surfaces
