-- Everything the recording touches outside this config: the shells already running, the demo
-- shell, the recorder and the files the demo shell reloads from.

local state_home = os.getenv("XDG_STATE_HOME")
if not state_home or state_home == "" then
    state_home = (os.getenv("HOME") or "") .. "/.local/state"
end

-- On disk before any shell stops, so a director reload or crash mid-take still knows what to
-- bring back on the next run.
local store = persistent_table {
    path = state_home .. "/mantle-demo",
    name = "restore.json",
    defaults = { shells = {} },
}

local demo_shell = session_process { name = "demo-shell" }
local recorder = session_process { name = "demo-recorder", stop_signal = "INT" }

local function run(cmd, args, done)
    local out = {}
    process.run(cmd, args, function(line, stream)
        if stream == "stdout" then out[#out + 1] = line end
    end, function(code)
        if done then done(code, out) end
    end)
end

-- Calls `done` once `probe()` is truthy, polling every 100 ms, or with `false` after `ms`.
local function wait_for(probe, ms, done)
    if probe() then return done(true) end
    if ms <= 0 then return done(false) end
    timer(100, function() wait_for(probe, ms - 100, done) end)
end

local function each(items, step, done)
    local function at(k)
        if k > #items then return done() end
        step(items[k], function() at(k + 1) end)
    end
    at(1)
end

-- Stops every other running shell and hands `done` the full list to restore: the ones stopped
-- now plus any a failed earlier take left behind. The demo shell's own directory is never restored.
local function stop_others(demo_dir, done)
    wait_for(function() return store.shells:get() ~= nil end, 3000, function()
        local restore = {}
        for _, shell in ipairs(store.shells:get() or {}) do
            restore[#restore + 1] = shell
        end
        run("mantle", { "list" }, function(_, lines)
            local running = {}
            for _, line in ipairs(lines) do
                local pid, config = line:match("^(%d+)%s+%S+%s+%S+%s+(.+)$")
                if pid and tonumber(pid) ~= mantle.pid then
                    running[#running + 1] = { pid = pid, config = config }
                end
            end
            each(running, function(shell, next_shell)
                run("readlink", { "/proc/" .. shell.pid .. "/exe" }, function(_, exe)
                    local known = false
                    for _, saved in ipairs(restore) do
                        known = known or saved.config == shell.config
                    end
                    if exe[1] and shell.config ~= demo_dir and not known then
                        restore[#restore + 1] = { exe = (exe[1]:gsub(" %(deleted%)$", "")), config = shell.config }
                        store:set("shells", restore)
                    end
                    run("mantle", { "stop", "--pid", shell.pid }, next_shell)
                end)
            end, function() done(restore) end)
        end)
    end)
end

-- An empty list leaves the store alone: a take that failed before `stop_others` ran must not
-- forget the shells an earlier one stopped.
local function restore_shells(shells)
    if #shells == 0 then return end
    for _, shell in ipairs(shells) do
        process.detach(shell.exe, { "-d", "-c", shell.config })
    end
    store:set("shells", {})
end

-- `text` lands through a rename, so the watcher never evaluates a half-written file.
local function write(path, text, done)
    run("sh", { "-c", 'printf "%s" "$1" > "$2.part" && mv "$2.part" "$2"', "sh", text, path }, done)
end

-- Writes the demo shell's `state(name)` the way a keybind would.
local function set_state(dir, name, value, done)
    run("mantle", { "-c", dir, "set", name, json.encode(value) }, done)
end

return {
    set_state = set_state,
    run = run,
    wait_for = wait_for,
    stop_others = stop_others,
    restore_shells = restore_shells,
    write = write,
    demo_shell = demo_shell,
    recorder = recorder,
}
