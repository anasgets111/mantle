-- The storyboard's steps: mock feeds, keybind calls and typing into the demo shell, the stage slide.

local session, feeds, mockups, theme, run, panes = require("session"), require("feeds"), require("mockups"),
    require("theme"), require("run"), require("panes")

local M = {}

function M.wait(ms)
    return function(next) run.later(ms, next) end
end

function M.toggle(name)
    return function(next)
        run.demo({ "toggle", name }, function() next() end)
    end
end

-- What the demo shell was last fed and the wallpaper last picked: a renderer killed on camera
-- comes back with neither, and `replay_mocks` pushes them again.
local last_fed, picked = {}, nil
-- Edits saved so far: each save reloads the demo shell.
local saves = 0

-- Picks a wallpaper as a keybind would. The director
-- quantizes the same thumbnail the demo shell does, so its own panes re-theme in step.
function M.pick(file)
    return function(next)
        picked = file
        panes.backdrop:set(run.DEMO_DIR .. "/wallpapers/" .. file)
        theme.choose(run.DEMO_DIR .. "/wallpapers/thumbs/" .. file)
        run.demo({ "call", "wallpaper", file }, function() next() end)
    end
end

function M.feed(name, value)
    return function(next)
        last_fed[name] = value
        run.demo({ "set", name, json.encode(value) }, function() next() end)
    end
end

-- The aurora runs two 16 s passes from a reload, and stilling it mid-pass snaps it. From
-- `settle_aurora` on it is stilled once 32 s pass with no save, or at once on a respawned
-- renderer, through `replay_mocks`.
local function settle_later()
    if not last_fed.aurora_settled then return end
    local at = saves
    timer(32500, function()
        if at == saves and not run.finished then M.feed("aurora_settled", true)(function() end) end
    end)
end

-- An edit just saved.
function M.saved()
    saves = saves + 1
    settle_later()
end

function M.settle_aurora(next)
    last_fed.aurora_settled = true
    settle_later()
    next()
end

-- The take's windows open one by one; the browser's title follows the page its mockup shows.
local APP_WINDOW = { [""] = "0xa1", chat = "0xa3", call = "0xa4", browser = "0xa4" }
local BROWSER_TITLE = { call = "Meet · Weekly sync", browser = "Aurora timelapse · 4K" }
local windows = { count = 0, focused = "0xa1", page = "" }
M.windows = windows

-- Back to a take's start: nothing fed, picked or open.
function M.forget()
    last_fed, picked = {}, nil
    windows.count, windows.focused, windows.page = 0, "0xa1", ""
end

function M.feed_windows(next)
    local out = {}
    for k = 1, windows.count do
        local w = feeds.WINDOWS[k]
        local title = w.id == "0xa4" and BROWSER_TITLE[windows.page] or w.title
        out[k] = { id = w.id, app_id = w.app_id, title = title, focused = w.id == windows.focused }
    end
    M.feed("mock_windows", { source = "hyprland", windows = out })(next)
end

-- Opens one window more, focused, as an app starting would.
function M.launch(next)
    windows.count = windows.count + 1
    windows.focused = feeds.WINDOWS[windows.count].id
    M.feed_windows(next)
end

-- Moves focus without opening anything: the overview's selection.
function M.select_window(id)
    return function(next)
        windows.focused = id
        M.feed_windows(next)
    end
end

-- Opens mockup `id` ("" closes it) and focuses its window, so the taskbar follows the take.
function M.open_app(id)
    return function(next)
        windows.focused = APP_WINDOW[id]
        if BROWSER_TITLE[id] then windows.page = id end
        M.feed_windows(function() mockups.open(id)(next) end)
    end
end

-- Runs `mantle call name arg` on the demo shell, as a keybind would.
function M.call(name, arg)
    return function(next) run.demo({ "call", name, arg }, function() next() end) end
end

function M.notify(n)
    return function(next)
        M.call("reply", "")(function()
            M.feed("mock_notifications", { dnd = false, feed = { feeds.notification(n) } })(next)
        end)
    end
end

-- Types `text` one character at a time through `mantle call name`, as a field `set_text` fills.
function M.type_call(name, text)
    return function(next)
        if run.fast then return run.demo({ "call", name, text }, function() next() end) end
        local chars = {}
        for c in text:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
            chars[#chars + 1] = c
        end
        local function at(k)
            run.progress = run.progress + 1
            if k > #chars then return next() end
            run.demo({ "call", name, table.concat(chars, "", 1, k) }, function()
                timer(32 + math.random(0, 36) + (chars[k] == " " and 20 or 0), function() at(k + 1) end)
            end)
        end
        at(1)
    end
end

-- A password's length only, one key at a time: the mocks draw dots, never text.
function M.type_dots(name, count)
    return function(next)
        if run.fast then return M.feed(name, count)(next) end
        local function at(k)
            run.progress = run.progress + 1
            if k > count then return next() end
            M.feed(name, k)(function() timer(70 + math.random(0, 50), function() at(k + 1) end) end)
        end
        at(1)
    end
end

function M.install(next)
    local function at(step)
        run.progress = run.progress + 1
        M.feed("mock_updates", feeds.updates(step))(function()
            if not feeds.updates(step).installing then return next() end
            run.later(500, function() at(step + 1) end)
        end)
    end
    at(1)
end

-- Pushes every mock again, the wallpaper last: a respawned renderer starts with no named state.
function M.replay_mocks(next)
    local names = {}
    for name in pairs(last_fed) do
        names[#names + 1] = name
    end
    session.each(names, function(name, done) M.feed(name, last_fed[name])(done) end, function()
        if not picked then return next() end
        run.demo({ "call", "wallpaper", picked }, function() next() end)
    end)
end

function M.press(combo, name)
    return function(next)
        if not run.fast then
            panes.keys:set(combo)
            panes.key_command:set("mantle toggle " .. name)
        end
        run.later(450, function()
            M.toggle(name)(function() run.later(500, next) end)
        end)
    end
end

-- Slides the code pane off, or back, and has every `layout.placed` box follow: the director's
-- mock windows here, the demo shell's popups through its `stage_full`. Surfaces do not animate
-- their margin or size, so the script calls it with every popup closed; edits bring the pane back.
function M.set_stage(full, next)
    if panes.stage_full:get() == full then return next() end
    panes.stage_full:set(full)
    M.feed("stage_full", full)(function() run.later(450, next) end)
end

function M.full_stage(next) M.set_stage(true, next) end

function M.clear_keys(next)
    panes.keys:set("")
    panes.key_command:set("")
    next()
end

-- Runs `steps` in order as one step.
function M.chain(...)
    local steps = { ... }
    return function(next)
        session.each(steps, function(step, done) step(done) end, next)
    end
end

-- Shows `text` in the caption as the command that runs the next step.
function M.command(text)
    return function(next)
        panes.key_command:set(text)
        next()
    end
end

return M
