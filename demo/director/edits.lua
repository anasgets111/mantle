-- The keystrokes that turn one file into the next: a line diff, then per changed line the
-- backspaces and characters between their common prefix and suffix. A hunk adding more than
-- PASTE_LINES lines streams in line by line, as a paste would, and a changed line needing more than
-- RETYPE_KEYS keystrokes is replaced whole; typing either out would outlast the shot.

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

-- `at` is the hunk's first line in the new file, which is also its line in the buffer once every
-- earlier hunk has played.
local function plan(old_text, new_text)
    local ops = {}
    for _, hunk in ipairs(hunks(split(old_text), split(new_text))) do
        local line = hunk.at
        if #hunk.added > PASTE_LINES then
            for _ = 1, #hunk.removed do
                ops[#ops + 1] = { kind = "remove_line", line = line }
            end
            for k, text in ipairs(hunk.added) do
                ops[#ops + 1] = { kind = "paste_line", line = line + k - 1, text = text }
            end
        else
            local paired = math.min(#hunk.removed, #hunk.added)
            for k = 1, paired do
                local keys = {}
                retype(keys, line + k - 1, hunk.removed[k], hunk.added[k])
                if #keys > RETYPE_KEYS then
                    ops[#ops + 1] = { kind = "remove_line", line = line + k - 1 }
                    ops[#ops + 1] = { kind = "paste_line", line = line + k - 1, text = hunk.added[k] }
                else
                    table.move(keys, 1, #keys, #ops + 1, ops)
                end
            end
            for _ = paired + 1, #hunk.removed do
                ops[#ops + 1] = { kind = "remove_line", line = line + paired }
            end
            for k = paired + 1, #hunk.added do
                local text = hunk.added[k]
                local indent = text:match("^%s*")
                ops[#ops + 1] = { kind = "new_line", line = line + k - 1, text = indent }
                retype(ops, line + k - 1, indent, text)
            end
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
        table.remove(lines, op.line)
        return math.min(op.line, #lines), 1
    else
        table.insert(lines, op.line, op.text)
        return op.line, #op.text + 1
    end
end

return { split = split, plan = plan, apply = apply }
