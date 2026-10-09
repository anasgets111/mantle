-- The take's run: its environment, the flags every step reads, and the CLI that drives the demo shell.

local session = require("session")

local M = {
    -- Set while a preview plays the steps before its FROM edit behind the title card: waits,
    -- keystrokes and glides complete at once, so mock state still matches a full take.
    fast = false,
    -- Bumped by every script step and keystroke; the watchdog ends a take that stops moving.
    progress = 0,
    finished = false,
}

function M.env(name, fallback)
    local value = os.getenv(name)
    return (value and value ~= "") and value or fallback
end

M.DEMO_DIR = M.env("MANTLE_DEMO_DIR", M.env("XDG_RUNTIME_DIR", "/tmp") .. "/mantle-demo")
M.OUT = M.env("MANTLE_DEMO_OUT", M.env("HOME", "") .. "/Videos/mantle-demo.mp4")
-- A preview records nothing: it plays every step before edit FROM at once behind the title card,
-- stops at the edit after TO, and with SHOTS set saves a screenshot after each save and each beat.
M.FROM = M.env("MANTLE_DEMO_FROM", nil)
M.TO = M.env("MANTLE_DEMO_TO", nil)
M.SHOTS = M.env("MANTLE_DEMO_SHOTS", nil)

-- `timer`, except fast-forwarding takes 1 ms: a fresh callback, so long chains never nest.
function M.later(ms, fn)
    timer(M.fast and 1 or ms, fn)
end

-- Runs `mantle -c DEMO_DIR <args>` against the demo shell, or `args.sh` with DEMO_DIR as `$1`. A
-- step right after a save can name state or an action the edit adds before the reload lands, so
-- a refusal, or output `accept` rejects, retries for 3 s. `done` gets what `accept` returned last.
function M.demo(args, done, accept)
    local cmd, argv = "mantle", { "-c", M.DEMO_DIR, table.unpack(args) }
    if args.sh then cmd, argv = "sh", { "-c", args.sh, "sh", M.DEMO_DIR } end
    local expired = false
    timer(3000, function() expired = true end)
    local function go()
        session.run(cmd, argv, function(code, out)
            local ok = code == 0
            if accept then ok = accept(code, out or {}) end
            if not ok and not expired and not M.finished then
                M.progress = M.progress + 1
                return timer(100, go)
            end
            if done then done(code, out, ok) end
        end)
    end
    go()
end

-- The output the recorder captures.
function M.monitor_name()
    local screen = mantle.screens:get()[1]
    return M.env("MANTLE_DEMO_MONITOR", screen and screen.name or "screen")
end

-- With MANTLE_DEMO_SHOTS, captures the recorded output as `name`.png, then calls `done`. An
-- unnamed output gets no shot: grim without `-o` captures every output, yours included.
function M.shot(name, done)
    done = done or function() end
    local monitor = M.monitor_name()
    if not M.SHOTS or M.fast or monitor == "" or monitor == "screen" then return done() end
    local path = M.SHOTS .. "/" .. name .. ".png"
    session.run("grim", { "-o", monitor, path }, function(code)
        if code ~= 0 then log.warn("grim could not save", path) end
        done()
    end)
end

return M
