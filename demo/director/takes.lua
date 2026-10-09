-- The take's edits in order, and the files the demo shell loads beside them. Pure Lua: the
-- director requires it, and tools load it with plain `lua`.
--
-- Every edit saves a snapshot of the demo's shell.lua, pruned from `stages/take.tpl` to the edits
-- played so far, unless `file` names a module: then it plays that module's `--@ <name>` blocks.
-- `type` lists plain substrings; changed lines holding one are typed and the rest pasted (see
-- `edits.plan`). `derived` edits have no source: `derive` builds them from the last snapshot.

local M = {
    starter = "00-starter",
    template = "take.tpl",
    -- Written beside shell.lua, pruned to the edits played so far.
    modules = {
        "banner.lua", "osd.lua", "notifications.lua", "privacy.lua", "idle.lua", "wallpaper.lua",
        "taskbar.lua", "overview.lua", "targets.lua", "media.lua", "tray.lua", "control.lua", "updates.lua",
        "polkit.lua", "lock.lua", "sysinfo.lua",
    },
    -- Copied as they are, from `stages/` and from the director's own directory.
    frags = { "aurora.frag", "chevron.frag" },
    shared = { "theme.lua", "layout.lua", "marks.lua" },
    -- Art for `wallpaper.lua`'s picker, as SVG paths from the director's directory.
    wallpapers = {
        dusk = "wallpapers/dusk.svg",
        ember = "wallpapers/ember.svg",
        tide = "wallpapers/tide.svg",
        mantle = "wallpaper.svg",
    },
    edits = {
        { name = "01-size" },
        { name = "02-color" },
        { name = "03-workspaces",    type = { "workspaces," } },
        { name = "04-launcher",      type = { "local open", "fuzzy(" } },
        { name = "05-restyle",       type = { "behind_blur", "radius = 24" } },
        { name = "06-shader",        type = { "/aurora.frag", "mask = FADE" } },
        { name = "07-wallpaper",     type = { 'require("theme")' } },
        { name = "08-windows",       type = { "taskbar.bar,", "overview," } },
        { name = "09-media",         type = {} },
        { name = "09-motion",        type = { "trim_end = progress", "keyframes = { 0, 360 }" }, file = "media.lua" },
        { name = "10-control",       type = { "control.panel,", "osd," } },
        { name = "11-notifications", type = {} },
        { name = "11-links",         type = { "runs(entry)", "on_link" },                        file = "notifications.lua" },
        { name = "12-indicators",    type = { "privacy,", "idle.indicator," } },
        { name = "13-updates",       type = { "updates.badge," } },
        { name = "14-sysinfo",       type = { "sysinfo," } },
        { name = "15-lock",          type = { "lock," } },
        { name = "16-agent",         type = { 'action("focus"' } },
        { name = "typo",             derived = true },
        { name = "fix",              derived = true },
    },
}

M.sources = { M.template, table.unpack(M.modules) }
for _, take in ipairs(M.edits) do
    take.file = take.file or "shell.lua"
end

-- Every source file under `dir`/stages, keyed by name: the template, then the modules.
function M.load(dir)
    local texts = {}
    for _, name in ipairs(M.sources) do
        ---@diagnostic disable-next-line: undefined-global -- tools only, under plain lua
        local file = assert(io.open(dir .. "stages/" .. name, "rb"))
        texts[name] = file:read("a")
        file:close()
    end
    return texts
end

-- A derived edit's text from the last stage's `good` one: `typo` misspells the clock's `align_v`,
-- so the rescue banner shows the engine's "did you mean"; `fix` puts it back.
function M.derive(name, good)
    if name == "fix" then return good end
    local anchor = good:find('return os.date("%a', 1, true)
    local at = anchor and good:find("align_v", anchor, true)
    if not at then error("typo: no align_v after the clock's os.date") end
    return good:sub(1, at - 1) .. "aling_v" .. good:sub(at + #"align_v")
end

-- Every edit in order as { take, file, before, after, played }: the file's text either side of it
-- and the edits played so far. `texts` maps each source file to its text, as `load` returns it.
function M.timeline(texts)
    local last = M.prune(texts[M.template])
    local from, played, out = {}, {}, {}
    for _, take in ipairs(M.edits) do
        local name, file = take.name, take.file
        local source = texts[file == "shell.lua" and M.template or file]
        played[name] = true
        from[file] = from[file] or M.prune(source, {})
        local after = take.derived and M.derive(name, last) or M.prune(source, played)
        local so_far = {}
        for k in pairs(played) do
            so_far[k] = true
        end
        out[#out + 1] = { take = take, file = file, before = from[file], after = after, played = so_far }
        from[file] = after
    end
    return out
end

-- A source as it stands once the edits in `played` (name -> true; nil means all) have played.
-- `--@ <edit>` A `--@ else` B `--@ end` keeps A once <edit> played and B before; markers go. Further
-- `--@ <edit>` lines add branches: the first one played wins, so list the latest edit first.
-- Blocks are flat: a nested, stray or unterminated marker raises.
function M.prune(text, played)
    local out, open, in_else, keep, taken, n = {}, nil, false, true, false, 0
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        local marker = line:match("^%s*%-%-@(.*)$")
        local tag = marker and marker:match("^%s+(%S+)%s*$")
        if marker and not tag then error("line " .. n .. ": malformed marker: " .. line) end
        if tag == "end" then
            if not open then error("line " .. n .. ": stray --@ end") end
            open, in_else, keep = nil, false, true
        elseif tag == "else" then
            if not open or in_else then error("line " .. n .. ": stray --@ else") end
            in_else, keep = true, not taken
        elseif tag then
            if in_else then error("line " .. n .. ": --@ " .. tag .. " after --@ else") end
            if not open then open, taken = tag, false end
            keep = not taken and (not played or played[tag] == true)
            taken = taken or keep
        elseif keep then
            out[#out + 1] = line
        end
    end
    if open then error("--@ " .. open .. " has no --@ end") end
    return (table.concat(out, "\n"))
end

return M
