-- The take's edits in order, and the files the demo shell loads beside them. Pure Lua: the
-- director requires it, and tools load it with plain `lua`.
--
-- An edit names a stage file in `stages/` and is saved as the demo's shell.lua, unless `file`
-- names a module: then it plays that module's `--@ <name>` blocks. `type` lists plain substrings;
-- changed lines holding one are typed and the rest pasted (see `edits.plan`). `derived` edits have
-- no stage file: `derive` builds them from the last stage.

local M = {
    starter = "00-starter",
    -- Written beside shell.lua, pruned to the edits played so far.
    modules = {
        "banner.lua", "osd.lua", "notifications.lua", "privacy.lua", "idle.lua", "wallpaper.lua",
        "taskbar.lua", "overview.lua", "targets.lua", "media.lua", "tray.lua", "control.lua", "updates.lua",
        "polkit.lua", "lock.lua", "sysinfo.lua",
    },
    -- Copied as they are.
    frags = { "aurora.frag", "chevron.frag" },
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
        { name = "typo",             derived = true },
        { name = "fix",              derived = true },
    },
}

-- The stage files: the starter, then every shell.lua edit with one.
M.stages = { M.starter }
for _, take in ipairs(M.edits) do
    take.file = take.file or "shell.lua"
    if take.file == "shell.lua" and not take.derived then M.stages[#M.stages + 1] = take.name end
end

-- A derived edit's text from the last stage's `good` one: `typo` misspells the clock's `align_v`,
-- so the rescue banner shows the engine's "did you mean"; `fix` puts it back.
function M.derive(name, good)
    if name == "fix" then return good end
    local at = good:find("align_v", good:find('return os.date("%a', 1, true), true)
    return good:sub(1, at - 1) .. "aling_v" .. good:sub(at + #"align_v")
end

-- Every edit in order as { take, file, before, after, played }: the file's text either side of it
-- and the edits played so far. `texts` maps each stage name and module file to its source.
function M.timeline(texts)
    local last = texts[M.stages[#M.stages]]
    local from, played, out = { ["shell.lua"] = texts[M.starter] }, {}, {}
    for _, take in ipairs(M.edits) do
        local name, file = take.name, take.file
        played[name] = true
        from[file] = from[file] or M.prune(texts[file], {})
        local after = take.derived and M.derive(name, last)
            or file == "shell.lua" and texts[name]
            or M.prune(texts[file], played)
        local so_far = {}
        for k in pairs(played) do
            so_far[k] = true
        end
        out[#out + 1] = { take = take, file = file, before = from[file], after = after, played = so_far }
        from[file] = after
    end
    return out
end

-- A module as it stands once the edits in `played` (name -> true; nil means all) have played.
-- `--@ <edit>` A `--@ else` B `--@ end` keeps A once <edit> played and B before; markers go.
-- Blocks are flat: a nested, stray or unterminated marker raises.
function M.prune(text, played)
    local out, open, in_else, keep, n = {}, nil, false, true, 0
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        local tag = line:match("^%s*%-%-@%s+(%S+)%s*$")
        if tag == "else" then
            if not open or in_else then error("line " .. n .. ": stray --@ else") end
            in_else, keep = true, not keep
        elseif tag == "end" then
            if not open then error("line " .. n .. ": stray --@ end") end
            open, in_else, keep = nil, false, true
        elseif tag then
            if open then error("line " .. n .. ": --@ " .. tag .. " inside --@ " .. open) end
            open, keep = tag, not played or played[tag] == true
        elseif keep then
            out[#out + 1] = line
        end
    end
    if open then error("--@ " .. open .. " has no --@ end") end
    return (table.concat(out, "\n"))
end

return M
