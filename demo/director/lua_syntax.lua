-- One line of Lua as text runs tagged with a token kind; `roles` names the theme colour of each.
-- Line by line on purpose: a string or comment left open mid-typing colours to the end of its own
-- line and no further.

local roles = {
    text = "text",
    keyword = "accent2",
    constant = "warm",
    string = "success",
    comment = "muted",
    call = "accent",
    field = "accent3",
    punct = "subtle",
}

local keywords = {}
for word in ("and break do else elseif end for function goto if in local not or repeat return then until while"):gmatch("%S+") do
    keywords[word] = true
end
local constants = { ["true"] = true, ["false"] = true, ["nil"] = true }

local function word_kind(line, word, after)
    if keywords[word] then return "keyword" end
    if constants[word] then return "constant" end
    local next_char = line:match("^%s*(.)", after)
    if next_char == "(" or next_char == "{" or next_char == '"' then return "call" end
    if next_char == "=" and line:sub(after):match("^%s*==") == nil then return "field" end
    return "text"
end

local cache = {}

local function highlight(line)
    local cached = cache[line]
    if cached then return cached end
    local runs = {}
    local function push(text, kind)
        local last = runs[#runs]
        if last and last.kind == kind then
            last.text = last.text .. text
        else
            runs[#runs + 1] = { text = text, kind = kind }
        end
    end
    local at = 1
    while at <= #line do
        local rest = line:sub(at)
        local token, kind
        if rest:match("^%-%-") then
            token, kind = rest, "comment"
        elseif rest:match('^["\']') then
            local quote = rest:sub(1, 1)
            local close = 2
            while close <= #rest and rest:sub(close, close) ~= quote do
                close = close + (rest:sub(close, close) == "\\" and 2 or 1)
            end
            token, kind = rest:sub(1, close), "string"
        elseif rest:match("^%d") then
            token, kind = rest:match("^[%d%.xXa-fA-F]+"), "constant"
        elseif rest:match("^[%a_]") then
            token = rest:match("^[%a_][%w_]*")
            kind = word_kind(line, token, at + #token)
        elseif rest:match("^%s") then
            token, kind = rest:match("^%s+"), "text"
        else
            token, kind = rest:sub(1, 1), "punct"
        end
        push(token, kind)
        at = at + #token
    end
    cache[line] = runs
    return runs
end

return { highlight = highlight, roles = roles }
