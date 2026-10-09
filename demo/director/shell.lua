-- Records the demo video: types each file in `stages/` into a code pane, saves it into a scratch
-- shell that reloads on camera, and captions each beat. `just demo` runs it. It stops every other
-- running shell for the take and starts them again after, from `session.lua`'s restore list.

local session, mockups, theme, takes, feeds = require("session"), require("mockups"), require("theme"),
    require("takes"), require("feeds")

fonts {
    "CaskaydiaCove Nerd Font Propo",
    "Noto Sans",
    "Noto Sans Arabic",
}

local run, panes, beats, pointer, agent = require("run"), require("panes"), require("beats"), require("pointer"),
    require("agent")
local meter, guard, sources = require("meter"), require("guard"), require("sources")

local wait, toggle, pick, feed, call, chain, set_stage = beats.wait, beats.toggle, beats.pick, beats.feed,
    beats.call, beats.chain, beats.set_stage
local point, spotlight, hide_pointer = pointer.point, pointer.spotlight, pointer.hide_pointer
local finish, texts = guard.finish, sources.texts
-- The step running, for the watchdog's log.
local step_at = 0

-- Names each caption's shot: the last edit saved, and a count, as one edit spans several beats.
local last_edit, said = takes.starter, 0

local function say(title, text)
    return function(next)
        if run.fast then return next() end
        said = said + 1
        run.shot(string.format("%s-end-%02d", last_edit, said), function()
            log.info("beat", title)
            panes.caption:set(title)
            panes.detail:set(text or "")
            next()
        end)
    end
end

-- The edit each `edit()` step plays, so a preview can find where to start.
local edit_steps = {}

-- Plays edit `name` in its file's buffer, switching the pane to that file first, then saves it.
-- Fast-forwarding, it waits for the reload, so later toggles and feeds land as in a full take.
local function edit(name)
    local function step(next)
        local plan = sources.plans[name]
        if panes.file_shown:get() ~= plan.file then
            panes.file_shown:set(plan.file)
            panes.set_text(plan.before)
        end
        local function save()
            last_edit = name
            beats.saved()
            panes.unfocus()
            session.write(run.DEMO_DIR .. "/" .. plan.file, plan.after, function()
                panes.status:set("saved")
                if run.fast then return timer(500, next) end
                if run.SHOTS then timer(900, function() run.shot(name .. "-saved") end) end
                next()
            end)
        end
        if run.fast then
            panes.set_text(plan.after)
            return set_stage(false, save)
        end
        panes.status:set("unsaved")
        set_stage(false, function() panes.play(plan.ops, function() timer(450, save) end) end)
    end
    edit_steps[step] = name
    return step
end

local function hide_card(next)
    if run.fast then return next() end
    panes.card_shown:set(false)
    timer(800, function()
        panes.card:set("")
        next()
    end)
end

-- Clears what a take leaves on screen: keys, pointer, mock apps, pane, backdrop and theme.
local function reset_scene()
    panes.keys:set("")
    panes.key_command:set("")
    pointer.reset()
    panes.flash:set(false)
    panes.stage_full:set(false)
    mockups.app:set("")
    panes.backdrop:set(panes.WALLPAPER)
    theme.choose(run.DEMO_DIR .. "/wallpapers/thumbs/mantle.png")
end

-- Behind the title card: the demo shell starts over on the starter, and the director's scene and
-- mock records go back to where a take begins.
local function restart_starter(next)
    session.demo_shell:stop()
    session.wait_for(guard.stopped(session.demo_shell), 8000, function(ok)
        if not ok then
            log.error("the demo shell did not stop; ending the take")
            return finish()
        end
        beats.forget()
        reset_scene()
        sources.start_shell(takes.prune(texts[takes.template], {}), {}, 2000, function() hide_card(next) end)
    end)
end

local function show_card(kind)
    return function(next)
        panes.caption:set("")
        panes.card_shown:set(true)
        panes.card:set(kind)
        next()
    end
end

-- The cold open on the final shell, its windows open and the code pane off. A preview plays it
-- behind the title card; a take opens on the shell itself.
local function cold_open(next)
    beats.windows.count = #feeds.WINDOWS
    if not run.FROM then panes.card:set("") end
    beats.feed_windows(function() set_stage(true, next) end)
end

local function search(text, hold)
    return chain(toggle("launcher_open"), wait(250), beats.type_call("search", text), wait(hold), call("search", ""),
        toggle("launcher_open"))
end

local function overview(hold)
    return chain(toggle("overview_open"), wait(hold), beats.select_window("0xa3"), wait(hold), toggle("overview_open"))
end

local steps = {
    guard.setup,
    sources.stage_final,
    cold_open,
    guard.record,
    say("Every pixel is Lua."),
    search("fi", 1000),
    toggle("picker_open"),
    wait(400),
    pick("ember.png"),
    wait(1300),
    toggle("picker_open"),
    beats.notify(feeds.SARAH),
    wait(1600),
    feed("mock_notifications", { dnd = false, feed = {} }),
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
    guard.tour,
    wait(1200),

    say("A launcher, fuzzy search included.", "state() a keybind toggles; fuzzy() ranks."),
    edit("04-launcher"),
    beats.press("Super+A", "launcher_open"),
    beats.clear_keys,
    beats.type_call("search", "tele"),
    wait(1500),
    call("search", ""),
    beats.type_call("search", "files"),
    wait(1500),

    say("Reloads keep state.", "Restyled while open."),
    edit("05-restyle"),
    wait(1600),
    call("search", ""),
    toggle("launcher_open"),
    wait(300),

    say("Shaders, and glass over them.", "GLSL behind the desktop; blur that fades out."),
    edit("06-shader"),
    beats.full_stage,
    wait(4200),

    say("Your wallpaper themes everything.", "palette.score picks a seed; palette.scheme paints."),
    edit("07-wallpaper"),
    beats.full_stage,
    toggle("picker_open"),
    wait(500),
    point("thumb:ember.png", "picker"),
    pick("ember.png"),
    hide_pointer,
    wait(1900),
    beats.command("mantle call wallpaper tide.png"),
    pick("tide.png"),
    wait(1900),
    beats.command("mantle call wallpaper dusk.png"),
    pick("dusk.png"),
    wait(1900),
    beats.clear_keys,
    toggle("picker_open"),
    wait(300),

    say("Windows, and live captures.", "mantle.windows and capture: the overview is Lua."),
    edit("08-windows"),
    beats.full_stage,
    beats.launch,
    wait(250),
    beats.launch,
    wait(250),
    beats.launch,
    wait(250),
    beats.launch,
    wait(400),
    point("task:0xa3", "bar"),
    beats.open_app("chat"),
    wait(900),
    hide_pointer,
    beats.open_app(""),
    beats.press("Super+Tab", "overview_open"),
    beats.clear_keys,
    beats.select_window("0xa2"),
    wait(800),
    beats.select_window("0xa3"),
    wait(800),
    toggle("overview_open"),
    wait(300),

    say("Motion is a property.", "Morphs, springs and waves, eased by the engine."),
    edit("09-media"),
    edit("09-motion"),
    feed("mock_tray", feeds.TRAY),
    feed("mock_media", feeds.player(61, "playing", run.DEMO_DIR)),
    beats.full_stage,
    toggle("media_open"),
    wait(400),
    point("media:play", "media"),
    feed("mock_media", feeds.player(62, "paused", run.DEMO_DIR)),
    wait(900),
    point("media:play", "media"),
    feed("mock_media", feeds.player(62, "playing", run.DEMO_DIR)),
    wait(700),
    hide_pointer,
    toggle("media_open"),
    wait(300),

    say("Quick settings, and an OSD.", "Network, Bluetooth, brightness; volume from any app."),
    edit("10-control"),
    beats.full_stage,
    beats.press("Super+C", "control_open"),
    beats.clear_keys,
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
    guard.nudge_volume,
    wait(1800),

    beats.settle_aurora,
    say("Your notification server.", "Links, inline replies, any script, swipe away."),
    edit("11-notifications"),
    edit("11-links"),
    beats.full_stage,
    beats.notify(feeds.SARAH),
    wait(1100),
    beats.type_call("reply", "On my way!"),
    wait(500),
    beats.notify { id = 2, from = "أحمد", text = "وصلت؟ الكل بانتظارك", placeholder = "رد على أحمد", read = "تحديد كمقروء" },
    wait(700),
    beats.type_call("reply", "خمس دقائق وأكون عندكم"),
    wait(700),
    beats.notify { id = 3, from = "Mantle devs", text = "v0.9 is out.", placeholder = "Reply", read = "Mark as read" },
    wait(600),
    pointer.swipe(3),

    say("Know who's watching.", "mantle.privacy and mantle.idle in the bar."),
    edit("12-indicators"),
    beats.full_stage,
    beats.open_app("call"),
    feed("mock_privacy", feeds.privacy(true, true, false)),
    spotlight("privacy", "bar"),
    wait(2300),
    feed("mock_privacy", feeds.privacy(false, false, false)),
    beats.open_app("browser"),
    feed("mock_idle", { inhibited = true, inhibitors = { { who = "Zen Browser", why = "Playing video" } } }),
    spotlight("idle", "bar"),
    wait(1900),
    feed("mock_idle", { inhibited = false, inhibitors = {} }),
    beats.open_app(""),

    say("Updates, with your polkit agent.", "The password prompt is Lua too."),
    edit("13-updates"),
    beats.full_stage,
    feed("mock_updates", feeds.updates()),
    spotlight("updates", "bar"),
    point("updates", "bar"),
    toggle("updates_open"),
    wait(500),
    point("updates:install", "updates"),
    hide_pointer,
    feed("mock_polkit", {
        active = true,
        user = run.env("USER", "you"),
        message = "Authentication is required to update the system's packages.",
    }),
    wait(400),
    beats.type_dots("polkit_typed", 6),
    wait(300),
    feed("mock_polkit", { active = false, user = run.env("USER", "you"), message = "" }),
    feed("polkit_typed", 0),
    beats.install,
    wait(600),
    toggle("updates_open"),

    say("Real numbers, no polling code.", "mantle.sysinfo, drawn as trimmed paths."),
    edit("14-sysinfo"),
    wait(300),
    spotlight("sysinfo", "bar"),
    wait(2500),

    say("Even the lock screen.", "Same Lua, same theme. The take mocks ext-session-lock."),
    edit("15-lock"),
    feed("mock_lock", feeds.lock()),
    wait(600),
    beats.type_dots("lock_typed", 6),
    feed("lock_typed", 0),
    feed("mock_lock", feeds.lock({ attempts = 1, error = "authentication failed" })),
    wait(1300),
    beats.type_dots("lock_typed", 8),
    feed("mock_lock", feeds.lock({ attempts = 1, unlocking = true })),
    wait(500),
    feed("lock_typed", 0),
    feed("mock_lock", feeds.lock({ active = false })),
    wait(300),

    say("Built for agents, too.", "Actions, states, check and log: a CLI any coding agent can drive."),
    edit("16-agent"),
    wait(300),
    agent.beat,
    agent.close,

    say("Break it on purpose.", "A typo never takes the desktop down."),
    edit("typo"),
    wait(3800),
    edit("fix"),
    wait(1200),

    say("Kill it. It comes back.", "kill -9 the renderer; the supervisor respawns it."),
    beats.full_stage,
    wait(1200),
    meter.kill_renderer,
    wait(4200),
    beats.clear_keys,

    function(next)
        if run.fast then return next() end
        meter.on_sample = function(u)
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

-- Ends at the first edit after MANTLE_DEMO_TO's.
local function sequence(steps, done)
    local past_to = false
    local function at(k)
        if run.finished then return end
        run.progress, step_at = run.progress + 1, k
        local name = edit_steps[steps[k]]
        if k > #steps or (name and past_to) then return done() end
        past_to = past_to or (run.TO ~= nil and name == run.TO)
        steps[k](function() at(k + 1) end)
    end
    at(1)
end

-- A callback that raises or overruns its budget is dropped, which would leave the take and the
-- recorder running with your shells stopped. Setup, which stops your shells and waits on the
-- compositor for the tour, gets a minute.
local function watchdog(seen)
    local budget = step_at <= 1 and 60000 or 20000
    timer(budget, function()
        if run.finished then return end
        if run.progress == seen then
            log.error("demo step", step_at, "made no progress for", budget // 1000, "s; ending the take")
            return finish()
        end
        watchdog(run.progress)
    end)
end

-- A reload restarts the take from the top, so it first clears what the last one left running:
-- the recorder, the demo shell, the tour and a nudged volume.
math.randomseed(7)
panes.caption:set("")
reset_scene()
panes.status:set("saved")
panes.card:set("title")
panes.card_shown:set(true)
panes.meter:set("")
panes.file_shown:set("shell.lua")
timer(1, function()
    session.recorder:stop()
    session.demo_shell:stop()
    session.wait_for(function()
        return guard.stopped(session.recorder)() and guard.stopped(session.demo_shell)() and
            session.store.shells:get() ~= nil
    end, 8000, function()
        guard.close_tour()
        guard.restore_volume()
        sources.load_sources(function()
            for _, name in ipairs(takes.sources) do
                if not texts[name] then
                    log.error("could not read stage", name)
                    return finish()
                end
            end
            local ok, err = pcall(sources.prepare)
            if not ok then
                log.error("could not plan the take:", err)
                return finish()
            end
            -- Fast up to the edit before FROM, or the restart before the first edit, so its
            -- caption and setup play at speed.
            local at, first, to_at
            for k, step in ipairs(steps) do
                if edit_steps[step] == run.FROM then at = at or k end
                if edit_steps[step] == run.TO then to_at = k end
                if not at and (edit_steps[step] or step == restart_starter) then first = k end
            end
            if (run.FROM and not at) or (run.TO and (not to_at or to_at < (at or 0))) then
                log.error("MANTLE_DEMO_FROM or MANTLE_DEMO_TO names no edit, or TO plays before FROM")
                return finish()
            end
            if run.FROM then
                run.fast = true
                table.insert(steps, first + 1, function(next)
                    run.fast = false
                    hide_card(next)
                end)
            end
            watchdog(run.progress)
            meter.sample()
            interval(meter.METER_MS, meter.sample)
            sources.warm(function() sequence(steps, finish) end)
        end)
    end)
end)

local surfaces = { panes.wallpaper }
for _, window in ipairs(mockups.panels(panes.backdrop)) do
    surfaces[#surfaces + 1] = window
end
for _, pane in ipairs({ panes.card_pane, panes.caption_pane, panes.code_pane, agent.pane, pointer.pane }) do
    surfaces[#surfaces + 1] = pane
end
return surfaces
