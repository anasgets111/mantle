-- One line of Lua as coloured text runs. Line by line on purpose: a string or comment left open
-- mid-typing colours to the end of its own line and no further.

local palette = {
    text = "#cdd6f4",
    keyword = "#cba6f7",
    constant = "#fab387",
    string = "#a6e3a1",
    comment = "#6c7086",
    call = "#89b4fa",
    field = "#b4befe",
    punct = "#9399b2",
}

local keywords = {}
for word in ("and break do else elseif end for function goto if in local not or repeat return then until while"):gmatch("%S+") do
    keywords[word] = true
end
local constants = { ["true"] = true, ["false"] = true, ["nil"] = true }

local function word_color(line, word, after)
    if keywords[word] then return palette.keyword end
    if constants[word] then return palette.constant end
    local next_char = line:match("^%s*(.)", after)
    if next_char == "(" or next_char == "{" or next_char == '"' then return palette.call end
    if next_char == "=" and line:sub(after):match("^%s*==") == nil then return palette.field end
    return palette.text
end

local cache = {}

local function highlight(line)
    local cached = cache[line]
    if cached then return cached end
    local runs = {}
    local function push(text, color)
        local last = runs[#runs]
        if last and last.color == color then
            last.text = last.text .. text
        else
            runs[#runs + 1] = { text = text, color = color }
        end
    end
    local at = 1
    while at <= #line do
        local rest = line:sub(at)
        local token, color
        if rest:match("^%-%-") then
            token, color = rest, palette.comment
        elseif rest:match('^["\']') then
            local quote = rest:sub(1, 1)
            local close = 2
            while close <= #rest and rest:sub(close, close) ~= quote do
                close = close + (rest:sub(close, close) == "\\" and 2 or 1)
            end
            token, color = rest:sub(1, close), palette.string
        elseif rest:match("^%d") then
            token, color = rest:match("^[%d%.xXa-fA-F]+"), palette.constant
        elseif rest:match("^[%a_]") then
            token = rest:match("^[%a_][%w_]*")
            color = word_color(line, token, at + #token)
        elseif rest:match("^%s") then
            token, color = rest:match("^%s+"), palette.text
        else
            token, color = rest:sub(1, 1), palette.punct
        end
        push(token, color)
        at = at + #token
    end
    cache[line] = runs
    return runs
end

return { highlight = highlight, palette = palette }
