-- Keeps your desktop out of the take: stage workspaces, tour, privacy guard, recorder, volume, `finish`.

local session, run = require("session"), require("run")

local M = {}

-- The recorded output's workspaces, from the compositor.
local function stage_output()
    local ws, name = mantle.workspaces:get(), run.monitor_name()
    if not ws then return end
    for _, output in ipairs(ws.outputs) do
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
    -- A reload mid-take finds itself on a stage workspace: the stored origin is where you were.
    origin_ws = origin_ws or session.store.origin_ws:get() or output.active_workspace
    session.store:set("origin_ws", origin_ws)
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
local TOUR = { "kitty", "--class", TOUR_ID, "--directory", run.env("HOME", "/"), "-e", "fish", "-C", FETCH }
local tour_ws

local function tour_windows()
    local out = {}
    for _, w in ipairs((mantle.windows:get() or { windows = {} }).windows) do
        if w.app_id == TOUR_ID then out[#out + 1] = w end
    end
    return out
end

function M.close_tour()
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
            M.close_tour()
            tour_ws = nil
        end
        done()
    end)
end

-- Why the recorded output must not be on camera, or nil: it shows a stage workspace holding no
-- window but the tour's.
local function exposed()
    local output = stage_output()
    if not output then return "no workspaces for " .. run.monitor_name() end
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
    M.finish(why)
end

mantle.workspaces:on_change(guard)
mantle.windows:on_change(guard)

function M.tour(next)
    if run.fast or not tour_ws then
        M.close_tour()
        return next()
    end
    mantle.workspaces:focus(tour_ws)
    timer(1200, function()
        mantle.workspaces:focus(stage_ws[1])
        timer(500, function()
            M.close_tour()
            guard()
            if not run.finished then next() end
        end)
    end)
end

local restore = {}
local volume_before

local function set_volume(level, done)
    session.run("wpctl", { "set-volume", "@DEFAULT_AUDIO_SINK@", string.format("%.2f", level) }, done)
end

-- Puts back a volume a take, or one a reload cut short, left nudged.
function M.restore_volume()
    local level = volume_before or session.store.volume:get()
    if level then set_volume(level) end
    volume_before = nil
    session.store:set("volume", nil)
end

-- The default output three steps and back, for the OSD beat, through `wpctl` like any other app;
-- `finish` puts it back if a take ends mid-nudge.
function M.nudge_volume(next)
    if run.fast then return next() end
    session.run("wpctl", { "get-volume", "@DEFAULT_AUDIO_SINK@" }, function(_, out)
        volume_before = tonumber((out[1] or ""):match("Volume:%s*([%d%.]+)"))
        if not volume_before then return next() end
        session.store:set("volume", volume_before)
        local step = volume_before > 0.8 and -0.06 or 0.06
        local levels = { volume_before + step, volume_before + 2 * step, volume_before + 3 * step, volume_before }
        local function at(k)
            if run.finished then return end
            if not levels[k] then
                volume_before = nil
                session.store:set("volume", nil)
                return next()
            end
            set_volume(levels[k], function()
                timer(k == 3 and 900 or 350, function() at(k + 1) end)
            end)
        end
        at(1)
    end)
end

function M.stopped(handle)
    return function() return handle.running:get() ~= true end
end

-- `exposure` names what of yours the stage showed: the video then holds it, so it goes.
function M.finish(exposure)
    if run.finished then return end
    run.finished, guarding = true, false
    session.recorder:stop()
    M.restore_volume()
    M.close_tour()
    session.wait_for(M.stopped(session.recorder), 10000, function(ok)
        if not ok then session.recorder:signal("KILL") end
        session.wait_for(M.stopped(session.recorder), 2000, function(gone)
            session.demo_shell:stop()
            session.wait_for(M.stopped(session.demo_shell), 8000, function()
                -- Back to your workspace only once nothing can record it.
                origin_ws = origin_ws or session.store.origin_ws:get()
                if gone and origin_ws then
                    mantle.workspaces:focus(origin_ws)
                    session.store:set("origin_ws", nil)
                elseif not gone then
                    log.error("the recorder is still running; staying on the stage")
                end
                session.restore_shells(restore)
                if exposure and not run.FROM then
                    session.run("rm", { "-f", run.OUT })
                    log.error("discarded", run.OUT, "as it may show", exposure)
                elseif not run.FROM then
                    log.info("demo written to", run.OUT)
                end
                -- The restore list saves 1 s after its last write.
                timer(1500, function() session.run("mantle", { "stop", "--pid", tostring(mantle.pid) }) end)
            end)
        end)
    end)
end

-- Starts the recorder, and the guard, only while the stage shows nothing of yours.
function M.record(next)
    local why = exposed()
    if why then
        log.error("not recording:", why)
        return M.finish()
    end
    guarding = true
    if run.FROM then return next() end
    local monitor = run.monitor_name()
    local folder = run.OUT:match("^(.*)/[^/]*$") or "."
    session.run("mkdir", { "-p", folder }, function()
        session.recorder:start("gpu-screen-recorder", {
            "-w", monitor, "-f", "60", "-cursor", "no", "-q", "very_high", "-o", run.OUT,
        })
        session.wait_for(function() return session.recorder.running:get() == true end, 5000, function(up)
            if not up then
                log.error("gpu-screen-recorder did not start:", session.recorder.start_error:get())
                return M.finish()
            end
            timer(1000, next)
        end)
    end)
end

function M.setup(next)
    session.stop_others(run.DEMO_DIR, function(shells)
        restore = shells
        session.wait_for(function() return mantle.workspaces:get() ~= nil end, 3000, function()
            pick_workspaces()
            if not stage_ws[1] then return M.finish() end
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
                        M.finish()
                    end)
                end)
            end)
        end)
    end)
end

return M
