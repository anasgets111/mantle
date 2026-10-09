-- The keystrokes that turn one file into the next: a line diff, then per changed line the
-- backspaces and characters between their common prefix and suffix. Typing a whole hunk would
-- outlast the shot, so a hunk adding more than PASTE_LINES lines streams in line by line, and a
-- changed line needing more than RETYPE_KEYS keystrokes is replaced whole.
--
-- With `opts.type`, a list of plain substrings, the plan types only the changed lines holding one;
-- each run of other added lines lands as one `paste_block` and each run of other removed lines goes
-- as one `remove_line` with a `count`, so the camera dwells on the lines that matter. Every op carries `focus`,
-- its hunk's lines in the new file.

local PASTE_LINES = 3
local RETYPE_KEYS = 16

local function split(text)
    local lines = {}
    for line in (text .. "\n"):gmatch("(.-)\n") do
        lines[#lines + 1] = line
    end
    if lines[#lines] == "" then lines[#lines] = nil end
    return lines
end

-- ponytail: O(n*m) LCS table; fine for the ~150-line stages, a Myers diff past a few thousand.
local function common(old, new)
    local lcs = {}
    for i = #old + 1, 1, -1 do
        lcs[i] = {}
        for j = #new + 1, 1, -1 do
            if i > #old or j > #new then
                lcs[i][j] = 0
            elseif old[i] == new[j] then
                lcs[i][j] = lcs[i + 1][j + 1] + 1
            else
                lcs[i][j] = math.max(lcs[i + 1][j], lcs[i][j + 1])
            end
        end
    end
    return lcs
end

local function hunks(old, new)
    local lcs, out = common(old, new), {}
    local i, j = 1, 1
    while i <= #old or j <= #new do
        if i <= #old and j <= #new and old[i] == new[j] then
            i, j = i + 1, j + 1
        else
            local hunk = { removed = {}, added = {}, at = j }
            while i <= #old or j <= #new do
                if i <= #old and j <= #new and old[i] == new[j] then break end
                if j > #new or (i <= #old and lcs[i + 1][j] >= lcs[i][j + 1]) then
                    hunk.removed[#hunk.removed + 1] = old[i]
                    i = i + 1
                else
                    hunk.added[#hunk.added + 1] = new[j]
                    j = j + 1
                end
            end
            out[#out + 1] = hunk
        end
    end
    return out
end

local function retype(ops, line, old, new)
    local prefix = 0
    while prefix < #old and prefix < #new and old:byte(prefix + 1) == new:byte(prefix + 1) do
        prefix = prefix + 1
    end
    local suffix = 0
    while suffix < #old - prefix and suffix < #new - prefix and old:byte(#old - suffix) == new:byte(#new - suffix) do
        suffix = suffix + 1
    end
    for col = #old - suffix + 1, prefix + 2, -1 do
        ops[#ops + 1] = { kind = "erase", line = line, col = col }
    end
    for k = prefix + 1, #new - suffix do
        ops[#ops + 1] = { kind = "type", line = line, col = k, text = new:sub(k, k) }
    end
end

local function typed(text, patterns)
    for _, pattern in ipairs(patterns) do
        if text:find(pattern, 1, true) then return true end
    end
    return false
end

local function type_line(ops, line, text)
    local indent = text:match("^%s*")
    ops[#ops + 1] = { kind = "new_line", line = line, text = indent }
    retype(ops, line, indent, text)
end

-- Retypes `old` into `new`, or replaces it whole past RETYPE_KEYS keystrokes.
local function change(ops, line, old, new)
    local keys = {}
    retype(keys, line, old, new)
    if #keys > RETYPE_KEYS then
        ops[#ops + 1] = { kind = "remove_line", line = line }
        ops[#ops + 1] = { kind = "paste_line", line = line, text = new }
    else
        table.move(keys, 1, #keys, #ops + 1, ops)
    end
end

local function plan_hunk(ops, hunk)
    local line = hunk.at
    if #hunk.added > PASTE_LINES then
        for _ = 1, #hunk.removed do
            ops[#ops + 1] = { kind = "remove_line", line = line }
        end
        for k, text in ipairs(hunk.added) do
            ops[#ops + 1] = { kind = "paste_line", line = line + k - 1, text = text }
        end
        return
    end
    local paired = math.min(#hunk.removed, #hunk.added)
    for k = 1, paired do
        change(ops, line + k - 1, hunk.removed[k], hunk.added[k])
    end
    for _ = paired + 1, #hunk.removed do
        ops[#ops + 1] = { kind = "remove_line", line = line + paired }
    end
    for k = paired + 1, #hunk.added do
        type_line(ops, line + k - 1, hunk.added[k])
    end
end

-- A typed added line retypes the removed line at its index, if any. The other removed lines go
-- first, bottom run up, so the kept ones close up in order; then the added lines fill in from the
-- top, so each lands where it ends.
local function plan_typed(ops, hunk, patterns)
    local line, pair = hunk.at, {}
    for k = 1, math.min(#hunk.removed, #hunk.added) do
        pair[k] = typed(hunk.added[k], patterns)
    end
    local k = #hunk.removed
    while k >= 1 do
        local last = k
        while k >= 1 and not pair[k] do
            k = k - 1
        end
        if k < last then ops[#ops + 1] = { kind = "remove_line", line = line + k, count = last - k } end
        k = k - 1
    end
    k = 1
    while k <= #hunk.added do
        local at, text = line + k - 1, hunk.added[k]
        if pair[k] then
            change(ops, at, hunk.removed[k], text)
        elseif typed(text, patterns) then
            type_line(ops, at, text)
        else
            local block = {}
            while hunk.added[k] and not pair[k] and not typed(hunk.added[k], patterns) do
                block[#block + 1] = hunk.added[k]
                k = k + 1
            end
            ops[#ops + 1] = { kind = "paste_block", line = at, lines = block }
            k = k - 1
        end
        k = k + 1
    end
end

-- `at` is the hunk's first line in the new file, which is also its line in the buffer once every
-- earlier hunk has played.
local function plan(old_text, new_text, opts)
    local ops = {}
    for _, hunk in ipairs(hunks(split(old_text), split(new_text))) do
        local first = #ops + 1
        if opts and opts.type then
            plan_typed(ops, hunk, opts.type)
        else
            plan_hunk(ops, hunk)
        end
        local focus = { first = hunk.at, last = math.max(hunk.at, hunk.at + #hunk.added - 1) }
        for k = first, #ops do
            ops[k].focus = focus
        end
    end
    return ops
end

-- Plays `op` on `lines` in place and returns where the caret lands.
local function apply(lines, op)
    if op.kind == "type" then
        local text = lines[op.line]
        lines[op.line] = text:sub(1, op.col - 1) .. op.text .. text:sub(op.col)
        return op.line, op.col + 1
    elseif op.kind == "erase" then
        local text = lines[op.line]
        lines[op.line] = text:sub(1, op.col - 2) .. text:sub(op.col)
        return op.line, op.col - 1
    elseif op.kind == "remove_line" then
        for _ = 1, op.count or 1 do
            table.remove(lines, op.line)
        end
        return math.max(1, math.min(op.line, #lines)), 1
    elseif op.kind == "paste_block" then
        table.move(lines, op.line, #lines, op.line + #op.lines)
        table.move(op.lines, 1, #op.lines, op.line, lines)
        local last = op.line + #op.lines - 1
        return last, #lines[last] + 1
    else
        table.insert(lines, op.line, op.text)
        return op.line, #op.text + 1
    end
end

-- Milliseconds the director waits after `op`. `jitter` is a keystroke's random 0..26 ms; the mean
-- by default, which is what the timing estimate uses.
local function pause(op, jitter)
    if op.kind == "type" then
        return 24 + (jitter or 13) + (op.text == " " and 14 or 0)
    elseif op.kind == "erase" then
        return 32
    elseif op.kind == "paste_line" then
        return 55
    elseif op.kind == "paste_block" then
        return 350
    end
    return 140
end

return { split = split, plan = plan, apply = apply, pause = pause }
