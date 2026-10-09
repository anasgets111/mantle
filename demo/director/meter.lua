-- The demo shell's real cost from /proc, in the code pane's header, and the crash beat that kills it.

local session, run, panes, beats, guard = require("session"), require("run"), require("panes"), require("beats"),
    require("guard")

local M = {}

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
M.METER_MS = 2000
local CLK_TCK = 100
-- The last reading; `M.on_sample` is a step waiting for the next one with a CPU figure.
local usage

function M.sample()
    local pid = session.demo_shell.pid:get()
    if not (pid and session.demo_shell.running:get()) then return end
    session.run("sh", { "-c", METER_SCRIPT, "sh", tostring(pid) }, function(_, out)
        local kb, ticks, renderer = tonumber(out[1]), tonumber(out[2]), out[3]
        if not (kb and kb > 0 and ticks) then return end
        local same = usage and usage.pid == pid and usage.renderer == renderer
        local cpu = same and (ticks - usage.ticks) * 100000 / (CLK_TCK * M.METER_MS) or nil
        usage = { pid = pid, renderer = renderer, mb = kb / 1024, ticks = ticks, cpu = cpu }
        if not cpu then return end
        panes.meter:set(string.format("demo shell  %d MB  ·  %.1f%% CPU", math.floor(usage.mb + 0.5), cpu))
        local waiting = M.on_sample
        M.on_sample = nil
        if waiting then waiting(usage) end
    end)
end

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

-- `kill -9` on the demo shell's renderer, captioned with the real pid, then with the real time the
-- Supervisor took to bring a new one up; the mocks go back in after. No respawn in 5 s ends the take.
function M.kill_renderer(next)
    if run.fast then return next() end
    renderer_pid(function(pid)
        if not pid then
            log.warn("no demo renderer to kill")
            return next()
        end
        panes.key_command:set("kill -9 " .. pid)
        timer(1200, function()
            local args = {
                "-c", RESPAWN_SCRIPT, "sh", tostring(session.demo_shell.pid:get()), run.DEMO_DIR, tostring(pid),
            }
            session.run("sh", args, function(code, out)
                local ms = tonumber(out[1])
                if code ~= 0 or not ms then
                    log.error("the demo renderer did not come back; ending the take")
                    return guard.finish()
                end
                panes.detail:set(string.format("Back in %d ms: the supervisor respawned the renderer.", ms))
                beats.replay_mocks(next)
            end)
        end)
    end)
end

return M
