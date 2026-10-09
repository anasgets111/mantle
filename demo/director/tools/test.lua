-- Checks the director's pure-Lua planning: `lua demo/director/tools/test.lua`. Every edit in
-- `takes.lua` replays to its target with and without typed planning, every module prunes, and
-- malformed markers raise.

local dir = (arg[0]:match("^(.*)/tools/[^/]*$") or ".") .. "/"
package.path = dir .. "?.lua;" .. package.path
local takes, edits = require("takes"), require("edits")

local function read(path)
    local file = assert(io.open(dir .. "stages/" .. path))
    local text = file:read("a")
    file:close()
    return text
end

-- Plays `plan(old, new, opts)` on `old` and checks it lands on `new` with every op focused.
local function replays(old, new, opts, what)
    local lines, kinds = edits.split(old), {}
    for _, op in ipairs(edits.plan(old, new, opts)) do
        assert(op.focus and op.focus.first <= op.focus.last, what .. ": op without focus")
        edits.apply(lines, op)
        kinds[op.kind] = (kinds[op.kind] or 0) + 1
    end
    local got = #lines > 0 and table.concat(lines, "\n") .. "\n" or ""
    assert(got == new, what .. ": replay differs from the target")
    return kinds
end

-- Every edit, as the director plans it, with and without its `type` patterns.
local texts = {}
for _, name in ipairs(takes.stages) do
    texts[name] = read(name .. ".lua")
end
local last = texts[takes.stages[#takes.stages]]
local from, played = { ["shell.lua"] = texts[takes.starter] }, {}
for _, take in ipairs(takes.edits) do
    local file = take.file
    played[take.name] = true
    if not from[file] then
        texts[file] = read(file)
        from[file] = takes.prune(texts[file], {})
    end
    local after = take.derived and takes.derive(take.name, last)
        or file == "shell.lua" and texts[take.name]
        or takes.prune(texts[file], played)
    replays(from[file], after, nil, take.name)
    replays(from[file], after, take, take.name .. " typed")
    from[file] = after
end
assert(takes.derive("typo", last):find("aling_v", 1, true), "typo misspells nothing")
assert(takes.derive("fix", last) == last, "fix does not restore the last stage")

-- Every module prunes with nothing and with everything played, leaving no marker.
for _, file in ipairs(takes.modules) do
    for _, set in ipairs({ {}, false }) do
        assert(not takes.prune(read(file), set or nil):find("%-%-@"), file .. ": marker left")
    end
end

-- Plan edge cases.
replays("", "", nil, "empty")
replays("a\nb\n", "a\nb\n", nil, "equal")
replays("a\nb\nc\n", "", nil, "all removed")
replays("", "a\nb\n", { type = { "a" } }, "from empty, typed")
replays("a\nb\nc\nd\ne\n", "", { type = { "x" } }, "all removed, typed")
local kinds = replays("a\nX\nY\nZ\nW\nb\n", "a\nb\n", nil, "plain")
assert(not kinds.paste_block, "plain planning pasted a block")
kinds = replays(
    "a\nheight = 40\nkeep\nold1\nold2\nz\n",
    "a\nheight = 56\nkeep\nnew1\nnew2\nnew3\nz\n",
    { type = { "height = 56" } },
    "typed mix"
)
assert(kinds.type == 2 and kinds.erase == 2 and kinds.paste_block == 1 and kinds.remove_line == 1, "typed mix")

-- Markers: indented, `else`, all played, and the malformed ones raise.
local marked = "x\n    --@ e1\n    A\n    --@ else\n    B\n    --@ end\n  --@ e2\n  C\n  --@ end\ny\n"
assert(takes.prune(marked, {}) == "x\n    B\ny\n")
assert(takes.prune(marked, { e1 = true }) == "x\n    A\ny\n")
assert(takes.prune(marked, nil) == "x\n    A\n  C\ny\n")
replays(takes.prune(marked, {}), takes.prune(marked, { e1 = true }), { type = { "A" } }, "module edit")
for what, bad in pairs({
    stray_end = "x\n--@ end\n",
    stray_else = "x\n--@ else\n",
    double_else = "--@ e\n--@ else\n--@ else\n--@ end\n",
    nested = "--@ a\n--@ b\n--@ end\n--@ end\n",
    unterminated = "--@ a\nx\n",
}) do
    assert(not pcall(takes.prune, bad, {}), what .. " did not raise")
end

print("director planning: ok")
