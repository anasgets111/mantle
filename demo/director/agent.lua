-- The agent beat: a terminal over the code pane that drives the demo shell through its CLI.

local theme, run, panes, pointer, beats = require("theme"), require("run"), require("panes"), require("pointer"),
    require("beats")

local MONO, later, demo, feed, wait = panes.MONO, run.later, run.demo, beats.feed, beats.wait

-- What a coding agent would run, over the code pane: typed commands and the demo shell's real
-- output, `false` while hidden.
local term = state("demo_term", false)

local agent_pane = panel {
    id = "agent",
    layer = "top",
    anchor = { top = true, right = true, bottom = true },
    margin = { top = 16, right = 16, bottom = 16 },
    width = panes.pane_width,
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
        children = computed({ term, panes.code_size }, function(rows, size)
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
-- of its output, or nil to reject it. A reload can still be landing, so what `accept` (or `pick`)
-- rejects retries for 3 s; then `canned` prints with a warning, never an error or a stall on camera.
local function agent(shown, args, pick, canned, accept)
    return function(next)
        if run.fast then return next() end
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
            end, accept or function(code, out) return pick(code, out) ~= nil end)
        end
        later(200, function() key(1) end)
    end
end

-- Clicks the bar's Focus chip the way an agent would, at the box the pointer aims at: on the bar,
-- screen and surface pixels agree. Focus ends off either way: a missed click must not leave the
-- desktop dimmed for the rest of the take.
local function agent_click(next)
    if run.fast then return next() end
    local function off() feed("focus_on", false)(next) end
    pointer.locate("focus", "bar", function()
        log.warn("agent beat fallback: click")
        off()
    end, function(box)
        local x, y = math.floor(box.x + box.width / 2), math.floor(box.y + box.height / 2)
        agent(string.format("mantle input bar click %d %d", x, y), { "input", "bar", "click", x, y },
            function(code) return code == 0 and {} or nil end, {})(off)
    end)
end

-- `focus` toggles, so its call is never retried on a `false`; the state listing says whether the
-- bar shows focus on, and a feed puts it there if not.
local function ensure_focus(next)
    if run.fast then return next() end
    demo({ "set" }, function(_, out)
        for _, line in ipairs(out or {}) do
            if line == "focus_on\ttrue" then return next() end
        end
        log.warn("agent beat fallback: focus")
        feed("focus_on", true)(next)
    end)
end

return {
    pane = agent_pane,
    beat = beats.chain(
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
        end, { { text = "true" } }, function(code) return code == 0 end),
        ensure_focus,
        wait(1100),
        agent("mantle log | grep focus", { sh = 'mantle -c "$1" log | grep "config: focus" | tail -1' },
            function(code, out)
                local last = code == 0 and out[1] and out[1]:match(".*config: focus%s+on")
                return last and { { text = (last:gsub("%s+", " ")) } } or nil
            end, { { text = "12:00:00 INFO renderer/config: focus on" } }),
        wait(900),
        pointer.spotlight("focus", "bar"),
        agent_click,
        pointer.hide_pointer,
        wait(1200)
    ),
    -- Hides the terminal, the code pane back under it.
    close = function(next)
        term:set(false)
        next()
    end,
}
