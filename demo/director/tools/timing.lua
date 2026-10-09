-- Replays the edit planner over every edit and prints its keystrokes and estimated seconds, as the
-- director would play it. Exits 1 when an edit types over MAX_CHARS or runs over MAX_SECONDS.
-- Usage: lua timing.lua
local dir = (arg[0]:match("^(.*)/tools/[^/]*$") or ".") .. "/"
package.path = dir .. "?.lua;" .. package.path
local takes, edits = require("takes"), require("edits")
local MAX_CHARS, MAX_SECONDS, JUMP_MS = 120, 5, 380

local texts = {}
for _, name in ipairs(takes.stages) do
    texts[name] = assert(io.open(dir .. "stages/" .. name .. ".lua")):read("a")
end
for _, file in ipairs(takes.modules) do
    texts[file] = assert(io.open(dir .. "stages/" .. file)):read("a")
end

local over = 0
print(("%-18s %6s %7s %7s %8s"):format("edit", "typed", "pasted", "removed", "seconds"))
for _, step in ipairs(takes.timeline(texts)) do
    local lines, line = edits.split(step.before), 1
    local typed, pasted, removed, ms = 0, 0, 0, 0
    for _, op in ipairs(edits.plan(step.before, step.after, step.take)) do
        if math.abs(op.line - line) > 1 then ms = ms + JUMP_MS end
        line = edits.apply(lines, op)
        ms = ms + edits.pause(op)
        if op.kind == "type" or op.kind == "erase" then typed = typed + 1 end
        if op.kind == "paste_line" then pasted = pasted + 1 end
        if op.kind == "paste_block" then pasted = pasted + #op.lines end
        if op.kind == "remove_line" then removed = removed + (op.count or 1) end
    end
    local flag = (typed > MAX_CHARS or ms > MAX_SECONDS * 1000) and "  <- over budget" or ""
    if flag ~= "" then over = over + 1 end
    print(("%-18s %6d %7d %7d %8.1f%s"):format(step.take.name, typed, pasted, removed, ms / 1000, flag))
end
print(("timing: %d edit(s) over %d chars or %d s"):format(over, MAX_CHARS, MAX_SECONDS))
os.exit(over == 0 and 0 or 1)
