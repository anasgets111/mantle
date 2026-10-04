-- Records the demo video: types each file in `stages/` into a code pane, saves it into a scratch
-- shell that reloads on camera, and captions each beat. `just demo` runs it. It stops every other
-- running shell for the take and starts them again after, from `session.lua`'s restore list.

local syntax = require("lua_syntax")
local edits = require("edits")
local session = require("session")
local mockups = require("mockups")
local theme = require("theme")
local layout = require("layout")

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
local STAGES = mantle.config_dir .. "/stages/"
local WORDMARK = mantle.config_dir .. "/../../docs/theme/m.png"
local WALLPAPER = mantle.config_dir .. "/wallpaper.svg"
local POINTER = mantle.config_dir .. "/pointer.svg"
local COVERS = mantle.config_dir .. "/covers"
local STAGE_NAMES = {
    "00-starter", "01-style", "02-workspaces", "03-launcher", "04-restyle", "05-search", "06-shader",
    "07-wallpaper", "08-taskbar", "09-overview", "10-osd", "11-media", "12-control", "13-notifications",
    "14-privacy", "15-idle", "16-updates", "17-lock", "18-sysinfo", "19-banner",
}
-- Copied beside the demo's shell.lua at setup, for the stages that require or read them.
local MODULES = {
    "banner.lua", "osd.lua", "notifications.lua", "privacy.lua", "idle.lua", "wallpaper.lua",
    "taskbar.lua", "overview.lua", "targets.lua", "media.lua", "tray.lua", "control.lua", "updates.lua",
    "polkit.lua", "lock.lua", "sysinfo.lua",
    "aurora.frag", "chevron.frag",
}
-- Rendered to PNG in the demo's `wallpapers/` at setup: `palette.quantize` reads no SVG. `mantle`
-- is the logo art the take opens on, so the last pick comes back to it.
local WALLPAPERS = {
    dusk = mantle.config_dir .. "/wallpapers/dusk.svg",
    ember = mantle.config_dir .. "/wallpapers/ember.svg",
    tide = mantle.config_dir .. "/wallpapers/tide.svg",
    mantle = WALLPAPER,
}
local WALLPAPER_ASPECT = 3440 / 1440

-- The buffer lives in these locals; `version` is the signal that says it changed.
local lines = {}
local caret = { line = 1, col = 1 }
local current = ""
local texts = {}
-- Bumped by every script step and keystroke; the watchdog ends a take that stops moving.
local progress, step_at, finished = 0, 0, false

local version = state("demo_version", 0)
local top = state("demo_top", 0)
local status = state("demo_status", "saved")
local caption = state("demo_caption", "")
local detail = state("demo_detail", "")
local keys = state("demo_keys", "")
local key_command = state("demo_key_command", "")
local card = state("demo_card", "title")
local card_shown = state("demo_card_shown", true)
local char_box = geometry("demo_char_box")
local meter = state("demo_meter", "")
local meter_hot = state("demo_meter_hot", false)
-- The picked wallpaper, for the cards and the browser mockup; the logo art until the first pick.
local backdrop = state("demo_backdrop", WALLPAPER)

local function bump() version:set(version:get() + 1) end

-- Scrolls so `line` sits a third of the way down whenever it strays near an edge.
local function reveal(line)
    local shown, count = top:get(), row_count:get()
    if line > shown + 3 and line <= shown + count - 4 then return end
    top:set(math.max(0, math.min(line - math.floor(count / 3), #lines - count + 4)))
end

mantle.screens:on_change(function() reveal(caret.line) end)

-- Code pane ---------------------------------------------------------------------------------

-- Only the lines in view: a translated full-length text is not cut by its parent's clip.
local code = computed({ version, top, theme.state, row_count }, function(_, first, t, count)
    local runs = {}
    for n = first + 1, math.min(#lines, first + count) do
        runs[#runs + 1] = { text = string.format("%4d  ", n), color = n == caret.line and t.subtext or t.overlay }
        for _, run in ipairs(syntax.highlight(lines[n])) do
            runs[#runs + 1] = { text = run.text, color = t[syntax.roles[run.kind]] }
        end
        runs[#runs + 1] = { text = "\n" }
    end
    return runs
end)

local caret_row = computed({ version, top, line_px }, function(_, first, line)
    return (caret.line - first - 1) * line
end)

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
    background = theme.fade("crust", "f2"),
    radius = 18,
    child = column {
        width = "fill",
        height = "fill",
        children = {
            row {
                width = "fill",
                height = HEADER,
                padding = { left = 24, right = 24 },
                spacing = 12,
                children = {
                    text { content = "shell.lua", align_v = "center", font = MONO, font_size = 20, foreground = theme.text },
                    rect { width = "fill" },
                    rect {
                        visible = meter:map(function(m) return m ~= "" end),
                        height = 40,
                        align_v = "center",
                        padding = { left = 16, right = 16 },
                        radius = 12,
                        background = computed({ meter_hot, theme.success, theme.base }, function(hot, on, off)
                            return hot and
                                on or off
                        end),
                        scale = meter_hot:map(function(hot) return hot and 1.08 or 1 end),
                        animate = { background = 300, scale = { duration = 400, easing = "out_back" } },
                        children = {
                            text {
                                content = meter,
                                align_v = "center",
                                font = MONO,
                                font_size = 18,
                                foreground = computed({ meter_hot, theme.crust, theme.subtext }, function(hot, on, off)
                                    return
                                        hot and on or off
                                end),
                                animate = { foreground = 300 },
                            },
                        },
                    },
                    text {
                        content = status:map(function(s) return s == "unsaved" and "●  unsaved" or "✓  saved" end),
                        foreground = computed({ status, theme.warm, theme.success }, function(s, dirty, clean)
                            return s == "unsaved" and dirty or clean
                        end),
                        animate = { foreground = 200 },
                        align_v = "center",
                        font = MONO,
                        font_size = 18,
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
                    rect {
                        width = "fill",
                        height = line_px,
                        background = theme.fade("text", "0a"),
                        translate = caret_row:map(function(y) return { x = 0, y = y } end),
                        animate = { translate = 80 },
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
        content = "→  " .. command,
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
    margin = { bottom = 48, left = 48 },
    visible = caption:map(function(title) return title ~= "" end),
    child = computed({ caption, frame }, function(title, m)
        local cap = layout.caption(m)
        return column {
            id = "caption:" .. title,
            width = cap.width,
            padding = { left = 30, right = 30, top = 24, bottom = 24 },
            spacing = 12,
            radius = 18,
            background = theme.fade("crust", "e6"),
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
                    visible = keys:map(function(k) return k ~= "" end),
                    margin = { top = 6 },
                    spacing = 10,
                    children = key_row,
                },
            },
        }
    end),
}

-- Title and end cards -----------------------------------------------------------------------

local function line_of(content, size, color, font)
    return text { content = content, align_h = "center", font = font, font_size = size, foreground = color }
end

-- What the take showed, two rows of six so a 1920 px screen fits them.
local FEATURES = {
    "Signals", "Shaders", "Wallpapers", "Screen capture", "Notifications", "MPRIS",
    "Tray", "Network", "Bluetooth", "Updates", "Lock screen", "Sysinfo",
}

-- Each chip holds invisible for its turn, then fades up: a stagger without a timer per chip.
local function feature_chip(index, label)
    local wait_ms = 250 + index * 60
    return rect {
        padding = { left = 20, right = 20, top = 10, bottom = 10 },
        radius = 22,
        background = theme.fade("surface", "cc"),
        border_width = 1,
        border_color = theme.overlay,
        opacity = 1,
        translate = { x = 0, y = 0 },
        animate = {
            opacity = { duration = 350, keyframes = { 0, { value = 0, duration = wait_ms }, 1 } },
            translate = {
                duration = 450,
                easing = "out_cubic",
                keyframes = {
                    { value = { x = 0, y = 18 } },
                    { value = { x = 0, y = 18 }, duration = wait_ms },
                    { value = { x = 0, y = 0 } },
                },
            },
        },
        children = { text { content = label, font_size = 24, foreground = theme.text } },
    }
end

local chips = {}
for index, label in ipairs(FEATURES) do
    chips[index] = feature_chip(index, label)
end

local card_lines = {
    title = {
        image { source = WORDMARK, width = 191, height = 160, fit = "contain", align_h = "center" },
        line_of("Mantle", 132, theme.text),
        line_of("Desktop shells in Lua, on Wayland.", 44, theme.subtext),
        line_of("Save the file. The shell changes.", 30, theme.muted),
    },
    ["end"] = {
        image { source = WORDMARK, width = 143, height = 120, fit = "contain", align_h = "center" },
        line_of("Write your shell in Lua.", 64, theme.text),
        row { align_h = "center", margin = { top = 12 }, spacing = 12, children = { table.unpack(chips, 1, 6) } },
        row { align_h = "center", margin = { bottom = 12 }, spacing = 12, children = { table.unpack(chips, 7, 12) } },
        line_of("anasgets111.github.io/mantle", 34, theme.accent, MONO),
        line_of("AUR: mantle-git", 30, theme.subtext, MONO),
        line_of("Typed, reloaded, captioned and recorded by a Mantle shell.", 24, theme.muted),
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

local function set_text(text)
    current = text
    lines = edits.split(text)
    caret.line, caret.col = 1, 1
    top:set(0)
    bump()
end

local function pause_after(op)
    if op.kind == "type" then
        return 24 + math.random(0, 26) + (op.text == " " and 14 or 0)
    elseif op.kind == "erase" then
        return 32
    elseif op.kind == "paste_line" then
        return 55
    end
    return 140
end

local function play(ops, done)
    local k = 0
    local step
    local function apply(op)
        progress = progress + 1
        caret.line, caret.col = edits.apply(lines, op)
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
    return function(next) timer(ms, next) end
end

local function say(title, text)
    return function(next)
        caption:set(title)
        detail:set(text or "")
        next()
    end
end

-- Each edit in take order, planned in `prepare` from a process callback: a diff overruns the
-- 2.5 ms budget a timer callback gets.
local EDITS = {
    "01-style", "02-workspaces", "03-launcher", "04-restyle", "05-search", "06-shader", "07-wallpaper",
    "08-taskbar", "09-overview", "10-osd", "11-media", "12-control", "13-notifications", "14-privacy",
    "15-idle", "16-updates", "17-lock", "18-sysinfo", "19-banner", "typo", "fix",
}
local planned = {}

local function prepare()
    local good = texts["19-banner"]
    -- The clock's `align_v`, misspelt: the rescue banner shows the engine's "did you mean".
    local clock = good:find('return os.date("%a', 1, true)
    local at = good:find("align_v", clock, true)
    texts.typo = good:sub(1, at - 1) .. "aling_v" .. good:sub(at + #"align_v")
    texts.fix = good
    local from = texts["00-starter"]
    for _, name in ipairs(EDITS) do
        planned[name] = edits.plan(from, texts[name])
        from = texts[name]
    end
end

local function edit(name)
    return function(next)
        status:set("unsaved")
        play(planned[name], function()
            current = texts[name]
            timer(450, function()
                session.write(DEMO_DIR .. "/shell.lua", current, function()
                    status:set("saved")
                    next()
                end)
            end)
        end)
    end
end

local function toggle(name)
    return function(next)
        session.run("mantle", { "-c", DEMO_DIR, "toggle", name }, function() next() end)
    end
end

-- Picks a wallpaper as a keybind would, and captions the command that did it. The director
-- quantizes the same thumbnail the demo shell does, so its own panes re-theme in step.
local function pick(file)
    return function(next)
        detail:set("click, or: mantle call wallpaper " .. file)
        backdrop:set(DEMO_DIR .. "/wallpapers/" .. file)
        theme.choose(DEMO_DIR .. "/wallpapers/thumbs/" .. file)
        session.run("mantle", { "-c", DEMO_DIR, "call", "wallpaper", file }, function() next() end)
    end
end

-- Mock feeds -------------------------------------------------------------------------------

local function feed(name, value)
    return function(next) session.set_state(DEMO_DIR, name, value, function() next() end) end
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
            body = { { kind = "text", text = n.text } },
            actions = { { key = "read", label = n.read } },
            has_default_action = true,
            has_reply = true,
            reply_placeholder = n.placeholder,
            urgency = "normal",
            expired = false,
            transient = false,
            timestamp = 0,
        }
        session.run("mantle", { "-c", DEMO_DIR, "call", "reply", "" }, function()
            feed("mock_notifications", { dnd = false, feed = { entry } })(next)
        end)
    end
end

-- Types `text` into state `name` one character at a time, as `mantle set` writes.
-- `type_call` does the same through `mantle call`, for a field `set_text` fills.
local function type_into(name, text)
    return function(next)
        local chars = {}
        for c in text:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
            chars[#chars + 1] = c
        end
        local function at(k)
            progress = progress + 1
            if k > #chars then return next() end
            session.set_state(DEMO_DIR, name, table.concat(chars, "", 1, k), function()
                timer(32 + math.random(0, 36) + (chars[k] == " " and 20 or 0), function() at(k + 1) end)
            end)
        end
        at(1)
    end
end

local function type_call(name, text)
    return function(next)
        local chars = {}
        for c in text:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
            chars[#chars + 1] = c
        end
        local function at(k)
            progress = progress + 1
            if k > #chars then return next() end
            session.run("mantle", { "-c", DEMO_DIR, "call", name, table.concat(chars, "", 1, k) }, function()
                timer(32 + math.random(0, 36) + (chars[k] == " " and 20 or 0), function() at(k + 1) end)
            end)
        end
        at(1)
    end
end

-- Sends the reply: the card fades, then the chat opens with the reply arriving under the message.
local function deliver(who, rtl, theirs, mine)
    return function(next)
        feed("reply_sent", true)(function()
            timer(400, function()
                feed("mock_notifications", { dnd = false, feed = {} })(function()
                    feed("reply_sent", false)(function()
                        mockups.chat:set({ name = who, rtl = rtl, messages = { { mine = false, text = theirs } } })
                        open_app("chat")(function()
                            timer(900, function()
                                mockups.chat:set({
                                    name = who,
                                    rtl = rtl,
                                    messages = { { mine = false, text = theirs }, { mine = true, text = mine } },
                                })
                                next()
                            end)
                        end)
                    end)
                end)
            end)
        end)
    end
end

-- A password's length only, one key at a time: the mocks draw dots, never text.
local function type_dots(name, count)
    return function(next)
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

-- Plays track `k` from `from` seconds for `seconds`, one `mock_media` push a second as a player's
-- position would advance.
local function play_track(k, from, seconds)
    return function(next)
        local track = TRACKS[k]
        local function at(t)
            progress = progress + 1
            if t > seconds then return next() end
            feed("mock_media", {
                players = {
                    {
                        id = "spotify",
                        identity = "Spotify",
                        title = track.title,
                        artist = track.artist,
                        album = track.album,
                        album_art_path = DEMO_DIR .. "/covers/" .. track.art,
                        length = track.length * 1000000,
                        position = (from + t) * 1000000,
                        play_state = "playing",
                    },
                },
            })(function() timer(1000, function() at(t + 1) end) end)
        end
        at(0)
    end
end

local TRAY = {
    { id = "1", name = "Steam",    icon_name = "steam",                status = "active" },
    { id = "2", name = "Vesktop",  icon_name = "vesktop",              status = "active" },
    { id = "3", name = "Telegram", icon_name = "org.telegram.desktop", status = "active" },
}

local function tray_items(count, calling)
    local out = {}
    for k = 1, count do
        local item = TRAY[k]
        out[k] = {
            id = item.id,
            name = item.name,
            icon_name = item.icon_name,
            status = item.name == calling and "needs_attention" or item.status,
        }
    end
    return { items = out }
end

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
            timer(700, function() at(step + 1) end)
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
    return function(next) session.run("mantle", { "-c", DEMO_DIR, "call", name, arg }, function() next() end) end
end

-- Captions the switches file as the demo shell saved it.
local function show_settings(next)
    session.run("cat", { DEMO_DIR .. "/state/settings.json" }, function(code, out)
        local saved = code == 0 and table.concat(out, " "):gsub("%s+", " ") or "(not saved yet)"
        caption:set("Switches that outlive restarts.")
        detail:set("persistent_table wrote state/settings.json:  " .. saved)
        next()
    end)
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

local pointer_pane = panel {
    id = "pointer",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    visible = pointer:map(function(p) return p.shown end),
    child = rect {
        width = "fill",
        height = "fill",
        children = {
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
    local screen = mantle.screens:get()[1]
    local box
    if surface == "picker" then
        box = layout.picker(screen)
    elseif surface == "overview" then
        box = layout.overview(screen)
    elseif surface == "media" then
        box = layout.center(screen, 760)
    elseif surface == "control" or surface == "updates" then
        box = layout.dock(screen, 620)
    else
        return { x = 0, y = 0 }
    end
    return { x = box.left, y = BAR + box.top }
end

-- Glides the pointer onto the demo shell's node `name` on `surface`, then clicks. It maps again
-- first: a surface mapped later stacks above it, and the demo's popups open after it shows.
local function point(name, surface)
    return function(next)
        session.run("mantle", { "-c", DEMO_DIR, "call", "where", name }, function(code, out)
            local ok, box = pcall(json.decode, table.concat(out or {}, "\n"))
            if code ~= 0 or not ok or type(box) ~= "table" or not box.width then
                log.warn("no pointer target", name)
                return next()
            end
            local o = origin_of(surface)
            local x, y = o.x + box.x + box.width / 2, o.y + box.y + box.height / 2
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
    local p = pointer:get()
    pointer:set({ x = p.x, y = p.y, shown = false })
    next()
end

-- Cost meter -------------------------------------------------------------------------------

-- Real, unlike the mocks: proportional memory and CPU of a shell's Supervisor and its Renderer,
-- from /proc. Only the `mantle-renderer` child counts: the director's other children are the demo
-- shell and the recorder.
local METER_SCRIPT = [[
cd /proc || exit 1
kids=$(cat "$1"/task/*/children 2>/dev/null)
set -- "$1" $(for c in $kids; do [ "$(cat "$c/comm" 2>/dev/null)" = mantle-renderer ] && echo "$c"; done)
for p; do cat "$p/smaps_rollup"; done 2>/dev/null | awk '/^Pss:/ { s += $2 } END { print s + 0 }'
for p; do cat "$p/stat"; done 2>/dev/null | awk '{ t += $14 + $15 } END { print t + 0 }'
]]
local METER_MS = 2000
local CLK_TCK = 100
local usage = {}

local function measure(key, pid, done)
    session.run("sh", { "-c", METER_SCRIPT, "sh", tostring(pid) }, function(_, out)
        local kb, ticks = tonumber(out[1]), tonumber(out[2])
        local last = usage[key]
        if kb and kb > 0 and ticks then
            local cpu = (last and last.pid == pid) and (ticks - last.ticks) * 100000 / (CLK_TCK * METER_MS) or 0
            usage[key] = { pid = pid, mb = kb / 1024, ticks = ticks, cpu = math.max(cpu, 0) }
        end
        done()
    end)
end

local function sample()
    local pid = session.demo_shell.pid:get()
    measure("director", mantle.pid, function()
        local function show()
            local demo = usage.demo
            if demo and session.demo_shell.running:get() then
                meter:set(string.format("demo shell  %d MB  ·  %.1f%% CPU", math.floor(demo.mb + 0.5), demo.cpu))
            end
        end
        if pid and session.demo_shell.running:get() then return measure("demo", pid, show) end
        show()
    end)
end

local function say_cost(title)
    return function(next)
        local demo = usage.demo or { mb = 0, cpu = 0 }
        caption:set(title)
        detail:set(string.format(
            "The demo shell right now: %d MB of memory and %.1f%% of one core, read live from /proc.",
            math.floor(demo.mb + 0.5),
            demo.cpu
        ))
        meter_hot:set(true)
        timer(3400, function()
            meter_hot:set(false)
            next()
        end)
    end
end

local function say_made_with()
    return function(next)
        local director = usage.director or { mb = 0 }
        caption:set("One more thing: this video is a Mantle shell too.")
        detail:set(string.format(
            "The code pane, the mock apps, these captions and the recorder: demo/director, in Lua, %d MB.",
            math.floor(director.mb + 0.5)
        ))
        next()
    end
end

-- The tour's first stop: the workspace this app's window is on, so the switch shows a real window.
local TOUR_APP = "org.gnome.Nautilus"

-- The take runs on `stage_ws[1]`, an id no workspace has yet, so nothing else of yours is in the
-- shot. Hyprland creates a missing number; niri ignores it and the take stays put.
local stage_ws, tour_ws, origin_ws = {}, nil, nil

local function pick_workspaces()
    local ws = mantle.workspaces:get()
    local exists = {}
    stage_ws, tour_ws = {}, nil
    if not ws then return log.warn("no workspaces from the compositor; the take stays put") end
    for _, output in ipairs(ws.outputs) do
        for _, w in ipairs(output.workspaces) do
            exists[w.id] = true
            if w.app_id == TOUR_APP then tour_ws = w.id end
        end
    end
    origin_ws = ws.outputs[1] and ws.outputs[1].active_workspace
    for id = 6, 99 do
        if not exists[id] and #stage_ws < 2 then stage_ws[#stage_ws + 1] = id end
    end
end

local function tour(next)
    local stops = { tour_ws or stage_ws[2], stage_ws[1] }
    local function hop(k)
        if not stops[k] then return next() end
        mantle.workspaces:focus(stops[k])
        timer(1600, function() hop(k + 1) end)
    end
    hop(1)
end

local function sequence(steps, done)
    local function at(k)
        progress, step_at = progress + 1, k
        if k > #steps then return done() end
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
                timer(k == 3 and 1400 or 450, function() at(k + 1) end)
            end)
        end
        at(1)
    end)
end

local function stopped(handle)
    return function() return handle.running:get() ~= true end
end

local function finish()
    if finished then return end
    finished = true
    if volume_before then set_volume(volume_before) end
    session.recorder:stop()
    session.wait_for(stopped(session.recorder), 10000, function()
        session.demo_shell:stop()
        session.wait_for(stopped(session.demo_shell), 8000, function()
            if origin_ws then mantle.workspaces:focus(origin_ws) end
            session.restore_shells(restore)
            log.info("demo written to", OUT)
            -- The restore list saves 1 s after its last write.
            timer(1500, function() session.run("mantle", { "stop", "--pid", tostring(mantle.pid) }) end)
        end)
    end)
end

local function record(next)
    local screen = mantle.screens:get()[1]
    local monitor = env("MANTLE_DEMO_MONITOR", screen and screen.name or "screen")
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
            if stage_ws[1] then mantle.workspaces:focus(stage_ws[1]) end
            next()
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

local function stage(next)
    local sources = { mantle.config_dir .. "/theme.lua", mantle.config_dir .. "/layout.lua" }
    for _, name in ipairs(MODULES) do
        sources[#sources + 1] = STAGES .. name
    end
    sources[#sources + 1] = DEMO_DIR .. "/"
    -- The last take's saved switches would start this one with them on.
    session.run("rm", { "-rf", DEMO_DIR .. "/state" }, function()
        session.run("mkdir", { "-p", DEMO_DIR .. "/wallpapers/thumbs" }, function()
            session.run("cp", { "-r", COVERS, DEMO_DIR .. "/" }, function()
                session.run("cp", sources, function()
                    render_wallpapers(function()
                        set_text(texts["00-starter"])
                        session.write(DEMO_DIR .. "/shell.lua", current, function()
                            session.demo_shell:start("mantle", { "-c", DEMO_DIR })
                            timer(2500, next)
                        end)
                    end)
                end)
            end)
        end)
    end)
end

local function hide_card(next)
    card_shown:set(false)
    timer(800, function()
        card:set("")
        next()
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
        keys:set(combo)
        key_command:set("mantle toggle " .. name)
        timer(700, function()
            toggle(name)(function() timer(1200, next) end)
        end)
    end
end

local script = {
    setup,
    stage,
    record,
    wait(600),
    hide_card,
    say("This is the whole shell.", "One Lua file. Mantle ships no shell of its own: you write it."),
    wait(2600),
    say("Save, and it's live.", "No restart. The file reloads in place."),
    edit("01-style"),
    wait(1800),
    say("Live system state.", "Workspaces from the compositor, as signals the bar redraws from."),
    edit("02-workspaces"),
    wait(1200),
    tour,
    wait(800),
    say("State a keybind can drive.", 'state("launcher_open") is writable from any compositor bind.'),
    edit("03-launcher"),
    wait(600),
    press("Super+A", "launcher_open"),
    function(next)
        keys:set("")
        next()
    end,
    say("Reloads keep state.", "The launcher stays open while you restyle it, and behind_blur = true asks for glass."),
    edit("04-restyle"),
    wait(2200),
    say("Fuzzy search, built in.", "fuzzy() scores each app as fzf does; the ranking stays in Lua."),
    edit("05-search"),
    wait(400),
    type_into("launcher_query", "tele"),
    wait(1600),
    feed("launcher_query", ""),
    type_into("launcher_query", "files"),
    wait(1600),
    feed("launcher_query", ""),
    toggle("launcher_open"),
    wait(700),
    say("Shaders on any node.", "A GLSL fragment behind the whole desktop, animated by the engine."),
    edit("06-shader"),
    wait(3000),
    say("Wallpapers that theme the shell.",
        "mantle.files lists the folder; palette.score picks a seed and palette.scheme paints the shell."),
    edit("07-wallpaper"),
    wait(600),
    toggle("picker_open"),
    wait(900),
    point("thumb:dusk.png", "picker"),
    pick("dusk.png"),
    wait(2000),
    point("thumb:ember.png", "picker"),
    pick("ember.png"),
    wait(2000),
    point("thumb:tide.png", "picker"),
    pick("tide.png"),
    wait(2000),
    point("thumb:mantle.png", "picker"),
    pick("mantle.png"),
    wait(2200),
    hide_pointer,
    toggle("picker_open"),
    wait(900),

    say("Every window, as a list.", "mantle.windows: app, title and focus from the compositor; a click focuses."),
    edit("08-taskbar"),
    wait(500),
    launch,
    wait(350),
    launch,
    wait(350),
    launch,
    wait(350),
    launch,
    wait(1200),
    point("task:0xa3", "bar"),
    open_app("chat"),
    wait(1100),
    point("task:0xa4", "bar"),
    open_app("browser"),
    wait(1100),
    point("task:0xa1", "bar"),
    open_app(""),
    hide_pointer,
    wait(900),

    say("An overview from a screen capture.", "capture draws any output through screencopy, the code pane included."),
    edit("09-overview"),
    wait(600),
    press("Super+Tab", "overview_open"),
    select_window("0xa2"),
    wait(650),
    select_window("0xa3"),
    wait(650),
    select_window("0xa4"),
    wait(650),
    select_window("0xa3"),
    wait(500),
    point("card:0xa3", "overview"),
    hide_pointer,
    toggle("overview_open"),
    function(next)
        keys:set("")
        mockups.chat:set({ name = "Mantle devs", rtl = false, messages = { { mine = false, text = "v0.9 is out" } } })
        next()
    end,
    open_app("chat"),
    wait(1300),
    open_app(""),
    wait(500),

    say("React to the system.", 'require("osd"): a volume OSD that follows every change, from any app.'),
    edit("10-osd"),
    wait(900),
    nudge_volume,
    wait(1400),

    say("Now playing, from MPRIS.", "mantle.mpris: title, artist and cover art from any player. The card is yours."),
    edit("11-media"),
    wait(500),
    play_track(1, 61, 1),
    point("media", "bar"),
    toggle("media_open"),
    play_track(1, 63, 2),
    point("media:next", "media"),
    play_track(2, 0, 2),
    hide_pointer,
    toggle("media_open"),
    wait(400),
    say("A tray, too.", "mantle.tray: every StatusNotifierItem and its menu. One of them wants you."),
    feed("mock_tray", tray_items(1)),
    wait(350),
    feed("mock_tray", tray_items(2)),
    wait(350),
    feed("mock_tray", tray_items(3)),
    wait(800),
    feed("mock_tray", tray_items(3, "Telegram")),
    wait(2000),
    feed("mock_tray", tray_items(3)),

    say("Quick settings.", "mantle.network, mantle.bluetooth and mantle.brightness, drawn as tiles."),
    edit("12-control"),
    wait(500),
    press("Super+C", "control_open"),
    function(next)
        keys:set("")
        next()
    end,
    feed("mock_network", { wifi_enabled = true, connected = false, strength = 0 }),
    feed("mock_bluetooth", { enabled = true, connected_devices = {} }),
    wait(800),
    feed("mock_network", { wifi_enabled = true, connected = true, ssid = "Home", strength = 82 }),
    feed("mock_bluetooth", { enabled = true, connected_devices = { { name = "WH-1000XM5", battery = 80 } } }),
    wait(600),
    feed("mock_brightness", { percent = 85 }),
    wait(600),
    point("control:dnd", "control"),
    call("setting", "dnd"),
    wait(400),
    point("control:night_light", "control"),
    call("setting", "night_light"),
    wait(800),
    show_settings,
    wait(2800),
    point("control:dnd", "control"),
    call("setting", "dnd"),
    hide_pointer,
    wait(500),
    toggle("control_open"),
    wait(500),

    say("Your notification server.", "Mantle serves org.freedesktop.Notifications. The popup is yours to draw."),
    edit("13-notifications"),
    wait(600),
    notify {
        id = 1,
        from = "Sarah",
        text = "Still on for tonight? 8 pm at the usual place.",
        placeholder = "Reply to Sarah",
        read = "Mark as read",
    },
    wait(1500),
    say("Reply inline.", "has_reply marks a sender that takes mantle.notifications:reply(id, text)."),
    type_call("reply", "On my way, see you in ten!"),
    wait(400),
    deliver("Sarah", false, "Still on for tonight? 8 pm at the usual place.", "On my way, see you in ten!"),
    wait(1800),
    open_app(""),
    wait(400),

    say("Any script, either direction.", "Arabic shapes and runs right to left in the same text node."),
    notify {
        id = 2,
        from = "أحمد",
        text = "وصلت؟ الكل بانتظارك",
        placeholder = "رد على أحمد",
        read = "تحديد كمقروء",
    },
    wait(1500),
    type_call("reply", "خمس دقائق وأكون عندكم"),
    wait(400),
    deliver("أحمد", true, "وصلت؟ الكل بانتظارك", "خمس دقائق وأكون عندكم"),
    wait(2000),
    open_app(""),
    wait(400),

    say("Know who's watching and listening.", "mantle.privacy: every app on the camera, the mic or a screen share."),
    edit("14-privacy"),
    wait(500),
    open_app("call"),
    wait(700),
    feed("mock_privacy", privacy_users(true, true, false)),
    wait(2000),
    function(next)
        mockups.sharing:set(true)
        feed("mock_privacy", privacy_users(true, true, true))(next)
    end,
    wait(2000),
    feed("mock_privacy", privacy_users(false, false, false)),
    function(next)
        mockups.sharing:set(false)
        open_app("")(next)
    end,
    wait(800),

    say("Idle, on your terms.", "mantle.idle names whoever keeps the screen awake."),
    edit("15-idle"),
    wait(500),
    function(next)
        mockups.playing:set(true)
        open_app("browser")(next)
    end,
    wait(700),
    feed("mock_idle", { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } }),
    wait(2400),
    function(next)
        mockups.playing:set(false)
        feed("mock_idle", { inhibited = false, inhibitors = {} })(next)
    end,
    wait(900),
    open_app(""),
    wait(600),

    say("Updates, through your polkit agent.",
        "mantle.updates checks pacman, dnf or apt. The password prompt is Lua too."),
    edit("16-updates"),
    wait(500),
    feed("mock_updates", updates_state()),
    wait(700),
    point("updates", "bar"),
    toggle("updates_open"),
    wait(1100),
    point("updates:install", "updates"),
    hide_pointer,
    feed("mock_polkit", {
        active = true,
        user = env("USER", "you"),
        message = "Authentication is required to update the system's packages.",
    }),
    wait(700),
    type_dots("polkit_typed", 9),
    wait(500),
    feed("mock_polkit", { active = false, user = env("USER", "you"), message = "" }),
    feed("polkit_typed", 0),
    install,
    wait(1400),
    toggle("updates_open"),
    wait(500),

    say("A lock screen, drawn in Lua.",
        "The real one holds the session through ext-session-lock and PAM. This take mocks it."),
    edit("17-lock"),
    wait(500),
    feed("mock_lock", lock_state()),
    wait(1500),
    type_dots("lock_typed", 7),
    wait(300),
    feed("lock_typed", 0),
    feed("mock_lock", lock_state({ attempts = 1, error = "authentication failed" })),
    wait(1400),
    type_dots("lock_typed", 10),
    wait(300),
    feed("mock_lock", lock_state({ attempts = 1, unlocking = true })),
    wait(900),
    feed("lock_typed", 0),
    feed("mock_lock", lock_state({ active = false })),
    wait(700),

    say("Real numbers, no polling code.", "mantle.sysinfo reads /proc for you: this machine's CPU and memory, live."),
    edit("18-sysinfo"),
    wait(3000),

    say_cost("Light by design."),
    wait(600),

    say("Draw your own error banner.", "mantle.rescue holds the error of the last failed reload."),
    edit("19-banner"),
    wait(900),
    say("Now break it.", "A typo never takes the desktop down, and the error says what it meant."),
    edit("typo"),
    wait(4400),
    say("Fix it, and it's back.", "The next good save clears the error."),
    edit("fix"),
    wait(2000),
    say_made_with(),
    wait(3800),
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

-- One at a time: each output line is a frame, and all stages at once overflow the Supervisor's
-- 1024-frame queue, which then drops this Renderer as wedged.
local function load_stages(done, k)
    k = k or 1
    local name = STAGE_NAMES[k]
    if not name then return done() end
    session.run("cat", { STAGES .. name .. ".lua" }, function(code, out)
        if code == 0 then texts[name] = table.concat(out, "\n") .. "\n" end
        load_stages(done, k + 1)
    end)
end

-- A reload restarts the take from the top, so it first clears what the last one left running.
math.randomseed(7)
caption:set("")
keys:set("")
key_command:set("")
pointer:set({ x = 0, y = 0, shown = false })
status:set("saved")
card:set("title")
card_shown:set(true)
meter:set("")
meter_hot:set(false)
mockups.app:set("")
timer(1, function()
    session.recorder:stop()
    session.demo_shell:stop()
    session.wait_for(function() return stopped(session.recorder)() and stopped(session.demo_shell)() end, 8000,
        function()
            load_stages(function()
                for _, name in ipairs(STAGE_NAMES) do
                    if not texts[name] then
                        log.error("could not read stage", name)
                        return finish()
                    end
                end
                prepare()
                sequence(script, finish)
                watchdog(progress)
                sample()
                interval(METER_MS, sample)
            end)
        end)
end)

local surfaces = { wallpaper }
for _, window in ipairs(mockups.panels(backdrop)) do
    surfaces[#surfaces + 1] = window
end
for _, pane in ipairs({ card_pane, caption_pane, code_pane, pointer_pane }) do
    surfaces[#surfaces + 1] = pane
end
return surfaces
