-- Records the demo video: types each file in `stages/` into a code pane, saves it into a scratch
-- shell that reloads on camera, and captions each beat. `just demo` runs it. It stops every other
-- running shell for the take and starts them again after, from `session.lua`'s restore list.

local syntax = require("lua_syntax")
local edits = require("edits")
local session = require("session")
local mockups = require("mockups")

fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local MONO = "CaskaydiaCove Nerd Font Mono"
local CODE_SIZE = 22
-- CODE_SIZE * 1.5, whole so line n sits at exactly (n - 1) * LINE for the caret and the scroll.
local LINE = 33
local GUTTER = 6
-- The demo bar's final height, cleared through its exclusive zone.
local BAR = 56
local HEADER = 64

local function env(name, fallback)
    local value = os.getenv(name)
    return (value and value ~= "") and value or fallback
end

local DEMO_DIR = env("MANTLE_DEMO_DIR", env("XDG_RUNTIME_DIR", "/tmp") .. "/mantle-demo")
local OUT = env("MANTLE_DEMO_OUT", env("HOME", "") .. "/Videos/mantle-demo.mp4")
local STAGES = mantle.config_dir .. "/stages/"
local WORDMARK = mantle.config_dir .. "/../../docs/theme/m.png"
local WALLPAPER = mantle.config_dir .. "/wallpaper.svg"
local STAGE_NAMES = {
    "00-starter", "01-style", "02-workspaces", "03-launcher", "04-restyle", "05-shader", "06-osd",
    "07-notifications", "08-privacy", "09-idle", "10-banner",
}
-- Copied beside the demo's shell.lua at setup, for the stages that require or read them.
local MODULES = { "banner.lua", "osd.lua", "notifications.lua", "privacy.lua", "idle.lua", "aurora.frag" }

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
local card = state("demo_card", "title")
local card_shown = state("demo_card_shown", true)
local char_box = geometry("demo_char_box")
local meter = state("demo_meter", "")
local meter_hot = state("demo_meter_hot", false)

local function bump() version:set(version:get() + 1) end

local function rows()
    local screen = mantle.screens:get()[1]
    return math.floor(((screen and screen.height or 1080) - BAR - 32 - HEADER) / LINE)
end

-- Scrolls so `line` sits a third of the way down whenever it strays near an edge.
local function reveal(line)
    local shown, count = top:get(), rows()
    if line > shown + 3 and line <= shown + count - 4 then return end
    top:set(math.max(0, math.min(line - math.floor(count / 3), #lines - count + 4)))
end

-- Code pane ---------------------------------------------------------------------------------

-- Only the lines in view: a translated full-length text is not cut by its parent's clip.
local code = computed({ version, top }, function(_, first)
    local runs = {}
    for n = first + 1, math.min(#lines, first + rows()) do
        runs[#runs + 1] = { text = string.format("%4d  ", n), color = n == caret.line and "#a6adc8" or "#45475a" }
        for _, run in ipairs(syntax.highlight(lines[n])) do
            runs[#runs + 1] = run
        end
        runs[#runs + 1] = { text = "\n" }
    end
    return runs
end)

local caret_row = computed({ version, top }, function(_, first) return (caret.line - first - 1) * LINE end)

local caret_at = computed({ caret_row, char_box }, function(y, box)
    local width = (box and box.width or 0) / 100
    return { x = (GUTTER + caret.col - 1) * width, y = y + 5 }
end)

local pane_width = mantle.screens:map(function(screens)
    return math.floor((screens[1] and screens[1].width or 1920) * 0.44)
end)

local code_pane = panel {
    id = "code",
    layer = "Top",
    anchor = { top = true, right = true, bottom = true },
    margin = { top = 16, right = 16, bottom = 16 },
    width = pane_width,
    height = "Fill",
    background = "#11111bf2",
    radius = 18,
    child = column {
        width = "Fill",
        height = "Fill",
        children = {
            row {
                width = "Fill",
                height = HEADER,
                padding = { left = 24, right = 24 },
                spacing = 12,
                children = {
                    text { content = "shell.lua", align_v = "Center", font = MONO, font_size = 20, foreground = "#cdd6f4" },
                    rect { width = "Fill" },
                    rect {
                        visible = meter:map(function(m) return m ~= "" end),
                        height = 40,
                        align_v = "Center",
                        padding = { left = 16, right = 16 },
                        radius = 12,
                        background = meter_hot:map(function(hot) return hot and "#a6e3a1" or "#1e1e2e" end),
                        scale = meter_hot:map(function(hot) return hot and 1.08 or 1 end),
                        animate = { background = 300, scale = { duration = 400, easing = "OutBack" } },
                        children = {
                            text {
                                content = meter,
                                align_v = "Center",
                                font = MONO,
                                font_size = 18,
                                foreground = meter_hot:map(function(hot) return hot and "#11111b" or "#a6adc8" end),
                                animate = { foreground = 300 },
                            },
                        },
                    },
                    text {
                        content = status:map(function(s) return s == "unsaved" and "●  unsaved" or "✓  saved" end),
                        foreground = status:map(function(s) return s == "unsaved" and "#fab387" or "#a6e3a1" end),
                        animate = { foreground = 200 },
                        align_v = "Center",
                        font = MONO,
                        font_size = 18,
                    },
                },
            },
            rect { width = "Fill", height = 1, background = "#313244" },
            rect {
                width = "Fill",
                height = "Fill",
                clip = "Box",
                padding = { top = 12 },
                children = {
                    text {
                        content = string.rep("0", 100),
                        geometry = char_box,
                        opacity = 0,
                        font = MONO,
                        font_size = CODE_SIZE,
                    },
                    rect {
                        width = "Fill",
                        height = LINE,
                        background = "#cdd6f40a",
                        translate = caret_row:map(function(y) return { x = 0, y = y } end),
                        animate = { translate = 80 },
                    },
                    text {
                        content = code,
                        font = MONO,
                        font_size = CODE_SIZE,
                        line_height = 1.5,
                        foreground = syntax.palette.text,
                    },
                    rect {
                        width = 3,
                        height = LINE - 10,
                        background = "#f5e0dc",
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
        background = "#313244",
        border_width = 1,
        border_color = "#585b70",
        scale = 1,
        animate = { scale = { duration = 260, easing = "OutBack", from = 0.6 } },
        children = { text { content = label, font = MONO, font_size = 24, foreground = "#cdd6f4" } },
    }
end

local key_row = keys:map(function(combo)
    local out = {}
    for key in combo:gmatch("[^+]+") do
        if #out > 0 then
            out[#out + 1] = text { content = "+", align_v = "Center", font_size = 24, foreground = "#6c7086" }
        end
        out[#out + 1] = chip(key)
    end
    out[#out + 1] = text {
        content = "→  mantle toggle launcher_open",
        align_v = "Center",
        font = MONO,
        font_size = 22,
        foreground = "#89b4fa",
    }
    return out
end)

local caption_pane = panel {
    id = "caption",
    layer = "Overlay",
    anchor = { bottom = true, left = true },
    margin = { bottom = 48, left = 48 },
    visible = caption:map(function(title) return title ~= "" end),
    child = caption:map(function(title)
        return column {
            id = "caption:" .. title,
            padding = { left = 30, right = 30, top = 24, bottom = 24 },
            spacing = 12,
            radius = 18,
            background = "#11111be6",
            opacity = 1,
            translate = { x = 0, y = 0 },
            animate = {
                opacity = { duration = 350, from = 0 },
                translate = { duration = 500, easing = "OutCubic", from = { x = 0, y = 30 } },
            },
            children = {
                text { content = title, font_size = 52, font_weight = 800, foreground = "#cdd6f4" },
                text {
                    content = detail,
                    visible = detail:map(function(d) return d ~= "" end),
                    font_size = 26,
                    foreground = "#a6adc8",
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
    return text { content = content, align_h = "Center", font = font, font_size = size, foreground = color }
end

local FEATURES = {
    "Signals", "Animations", "Shaders", "Lock screen", "Notifications", "Tray",
    "Audio", "Network", "Bluetooth", "MPRIS", "Idle", "Updates",
}

-- Each chip holds invisible for its turn, then fades up: a stagger without a timer per chip.
local function feature_chip(index, label)
    local wait_ms = 500 + index * 90
    return rect {
        padding = { left = 20, right = 20, top = 10, bottom = 10 },
        radius = 22,
        background = "#313244cc",
        border_width = 1,
        border_color = "#45475a",
        opacity = 1,
        translate = { x = 0, y = 0 },
        animate = {
            opacity = { duration = 350, keyframes = { 0, { value = 0, duration = wait_ms }, 1 } },
            translate = {
                duration = 450,
                easing = "OutCubic",
                keyframes = {
                    { value = { x = 0, y = 18 } },
                    { value = { x = 0, y = 18 }, duration = wait_ms },
                    { value = { x = 0, y = 0 } },
                },
            },
        },
        children = { text { content = label, font_size = 24, foreground = "#cdd6f4" } },
    }
end

local chips = {}
for index, label in ipairs(FEATURES) do
    chips[index] = feature_chip(index, label)
end

local card_lines = {
    title = {
        image { source = WORDMARK, width = 191, height = 160, fit = "contain", align_h = "Center" },
        line_of("Mantle", 132, "#cdd6f4"),
        line_of("Desktop shells in Lua, on Wayland.", 44, "#a6adc8"),
        line_of("Save the file. The shell changes.", 30, "#6c7086"),
    },
    ["end"] = {
        image { source = WORDMARK, width = 143, height = 120, fit = "contain", align_h = "Center" },
        line_of("Write your shell in Lua.", 64, "#cdd6f4"),
        row { align_h = "Center", margin = { top = 12, bottom = 12 }, spacing = 12, children = chips },
        line_of("anasgets111.github.io/mantle", 34, "#89b4fa", MONO),
        line_of("AUR: mantle-git", 30, "#a6adc8", MONO),
        line_of("Typed, reloaded, captioned and recorded by a Mantle shell.", 24, "#6c7086"),
    },
}

local wallpaper = panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    exclusive = "Ignore",
    child = image { source = WALLPAPER, width = "Fill", height = "Fill", fit = "cover" },
}

local card_pane = panel {
    id = "card",
    layer = "Overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    exclusive = "Ignore",
    visible = card:map(function(kind) return kind ~= "" end),
    child = card:map(function(kind)
        return rect {
            id = "card:" .. kind,
            width = "Fill",
            height = "Fill",
            opacity = card_shown:map(function(on) return on and 1 or 0 end),
            animate = { opacity = { duration = 700, easing = "OutCubic", from = 0 } },
            children = {
                image { source = WALLPAPER, width = "Fill", height = "Fill", fit = "cover" },
                rect { width = "Fill", height = "Fill", background = "#0b0b14b8" },
                column {
                    align_h = "Center",
                    align_v = "Center",
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
        return 34 + math.random(0, 40) + (op.text == " " and 20 or 0)
    elseif op.kind == "erase" then
        return 45
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
    "01-style", "02-workspaces", "03-launcher", "04-restyle", "05-shader", "06-osd",
    "07-notifications", "08-privacy", "09-idle", "10-banner", "typo", "fix",
}
local planned = {}

local function prepare()
    local good = texts["10-banner"]
    local _, paren = good:find("s and s.time)", 1, true)
    texts.typo = good:sub(1, paren - 1) .. good:sub(paren + 1)
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

-- Mock feeds -------------------------------------------------------------------------------

local function feed(name, value)
    return function(next) session.set_state(DEMO_DIR, name, value, function() next() end) end
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
        session.set_state(DEMO_DIR, "reply_draft", "", function()
            feed("mock_notifications", { dnd = false, feed = { entry } })(next)
        end)
    end
end

-- Types `reply` into the card's field one character at a time, as `mantle set` writes.
local function type_reply(reply)
    return function(next)
        local chars = {}
        for c in reply:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
            chars[#chars + 1] = c
        end
        local function at(k)
            progress = progress + 1
            if k > #chars then return next() end
            session.set_state(DEMO_DIR, "reply_draft", table.concat(chars, "", 1, k), function()
                timer(45 + math.random(0, 55) + (chars[k] == " " and 30 or 0), function() at(k + 1) end)
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
                        mockups.open("chat")(function()
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

local function privacy_users(camera, mic, screen)
    local function users(on) return on and { { app_name = "Meet" } } or {} end
    return { camera_users = users(camera), microphone_users = users(mic), screencast_users = users(screen) }
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
            timer(METER_MS, sample)
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
        timer(4200, function()
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

local function stage(next)
    local sources = {}
    for k, name in ipairs(MODULES) do
        sources[k] = STAGES .. name
    end
    sources[#sources + 1] = DEMO_DIR .. "/"
    session.run("mkdir", { "-p", DEMO_DIR }, function()
        session.run("cp", sources, function()
            set_text(texts["00-starter"])
            session.write(DEMO_DIR .. "/shell.lua", current, function()
                session.demo_shell:start("mantle", { "-c", DEMO_DIR })
                timer(2500, next)
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
        timer(700, function()
            toggle(name)(function() timer(1600, next) end)
        end)
    end
end

local script = {
    setup,
    stage,
    record,
    wait(2200),
    hide_card,
    say("This is the whole shell.", "One Lua file. Mantle ships no shell of its own: you write it."),
    wait(3200),
    say("Save, and it's live.", "No restart. The file reloads in place."),
    edit("01-style"),
    wait(2400),
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
    say("Reloads keep state.", "The launcher stays open while you restyle it."),
    edit("04-restyle"),
    wait(3600),
    toggle("launcher_open"),
    wait(700),
    say("Shaders on any node.", "A GLSL fragment behind the whole desktop, animated by the engine."),
    edit("05-shader"),
    wait(4000),
    say("React to the system.", 'require("osd"): a volume OSD that follows every change, from any app.'),
    edit("06-osd"),
    wait(900),
    nudge_volume,
    wait(1400),

    say("Your notification server.", "Mantle serves org.freedesktop.Notifications. The popup is yours to draw."),
    edit("07-notifications"),
    wait(600),
    notify {
        id = 1,
        from = "Sarah",
        text = "Still on for tonight? 8 pm at the usual place.",
        placeholder = "Reply to Sarah",
        read = "Mark as read",
    },
    wait(2000),
    say("Reply inline.", "has_reply marks a sender that takes mantle.notifications:reply(id, text)."),
    type_reply("On my way, see you in ten!"),
    wait(500),
    deliver("Sarah", false, "Still on for tonight? 8 pm at the usual place.", "On my way, see you in ten!"),
    wait(2400),
    mockups.open(""),
    wait(500),

    say("Any script, either direction.", "Arabic shapes and runs right to left in the same text node."),
    notify {
        id = 2,
        from = "أحمد",
        text = "وصلت؟ الكل بانتظارك",
        placeholder = "رد على أحمد",
        read = "تحديد كمقروء",
    },
    wait(2000),
    type_reply("خمس دقائق وأكون عندكم"),
    wait(500),
    deliver("أحمد", true, "وصلت؟ الكل بانتظارك", "خمس دقائق وأكون عندكم"),
    wait(2600),
    mockups.open(""),
    wait(500),

    say("Know who's watching and listening.", "mantle.privacy: every app on the camera, the mic or a screen share."),
    edit("08-privacy"),
    wait(500),
    mockups.open("call"),
    wait(700),
    feed("mock_privacy", privacy_users(true, true, false)),
    wait(2600),
    function(next)
        mockups.sharing:set(true)
        feed("mock_privacy", privacy_users(true, true, true))(next)
    end,
    wait(2600),
    feed("mock_privacy", privacy_users(false, false, false)),
    function(next)
        mockups.sharing:set(false)
        mockups.open("")(next)
    end,
    wait(1200),

    say("Idle, on your terms.", "mantle.idle names whoever keeps the screen awake."),
    edit("09-idle"),
    wait(500),
    function(next)
        mockups.playing:set(true)
        mockups.open("browser")(next)
    end,
    wait(700),
    feed("mock_idle", { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } }),
    wait(3000),
    function(next)
        mockups.playing:set(false)
        feed("mock_idle", { inhibited = false, inhibitors = {} })(next)
    end,
    wait(1400),
    mockups.open(""),
    wait(600),
    say("Then it steps away.",
        "register_threshold(seconds, on_idle, on_resume) drives your away screen. A real lock stays behind PAM."),
    wait(2600),
    feed("away", "on"),
    wait(3600),
    feed("away", "leaving"),
    wait(600),
    feed("away", "off"),
    wait(800),

    say_cost("Light by design."),
    wait(600),

    say("Draw your own error banner.", "mantle.rescue holds the error of the last failed reload."),
    edit("10-banner"),
    wait(1200),
    say("Now break it.", "A typo never takes the desktop down."),
    edit("typo"),
    wait(4200),
    say("Fix it, and it's back.", "The next good save clears the error."),
    edit("fix"),
    wait(2600),
    say_made_with(),
    wait(4500),
    show_card("end"),
    wait(5500),
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

local function load_stages(done)
    local pending = #STAGE_NAMES
    for _, name in ipairs(STAGE_NAMES) do
        session.run("cat", { STAGES .. name .. ".lua" }, function(code, out)
            if code == 0 then texts[name] = table.concat(out, "\n") .. "\n" end
            pending = pending - 1
            if pending == 0 then done() end
        end)
    end
end

-- A reload restarts the take from the top, so it first clears what the last one left running.
math.randomseed(7)
caption:set("")
keys:set("")
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
            end)
        end)
end)

local surfaces = { wallpaper }
for _, window in ipairs(mockups.panels(WALLPAPER)) do
    surfaces[#surfaces + 1] = window
end
for _, pane in ipairs({ card_pane, caption_pane, code_pane }) do
    surfaces[#surfaces + 1] = pane
end
return surfaces
