-- Writes one config dir per edit under OUT (starter first), each as the demo shell would see it
-- once that edit has saved: shell.lua, the modules pruned to the edits played, frags, shared
-- modules, covers and wallpapers. Usage: lua checkpoints.lua OUT
local dir = (arg[0]:match("^(.*)/tools/[^/]*$") or ".") .. "/"
package.path = dir .. "?.lua;" .. package.path
local takes = require("takes")
local out = assert(arg[1], "usage: checkpoints.lua OUT")

local function q(path)
    return "'" .. path:gsub("'", "'\\''") .. "'"
end
local function sh(cmd)
    assert(os.execute(cmd), cmd)
end
local function write(path, text)
    local f = assert(io.open(path, "wb"))
    f:write(text)
    f:close()
end

local texts = takes.load(dir)

-- Rendered once and copied; the check lays out at its own size, so 300 px is enough.
local assets = out .. "/_assets"
sh(("rm -rf %s && mkdir -p %s/wallpapers/thumbs"):format(q(out), q(assets)))
for name, svg in pairs(takes.wallpapers) do
    for _, to in ipairs { "wallpapers/", "wallpapers/thumbs/" } do
        sh(("rsvg-convert -w 300 -o %s %s"):format(q(assets .. "/" .. to .. name .. ".png"), q(dir .. svg)))
    end
end
sh(("cp -r %s %s"):format(q(dir .. "covers"), q(assets)))
for _, file in ipairs(takes.frags) do
    sh(("cp %s %s"):format(q(dir .. "stages/" .. file), q(assets)))
end
-- Check only: `targets.lua` requires the sample under MANTLE_DEMO_MOCKS, and it requires `feeds`.
for _, file in ipairs { "feeds.lua", "tools/mocks_sample.lua", table.unpack(takes.shared) } do
    sh(("cp %s %s"):format(q(dir .. file), q(assets)))
end

local function checkpoint(name, shell, played)
    local to = out .. "/" .. name
    sh(("cp -r %s %s"):format(q(assets), q(to)))
    for _, file in ipairs(takes.modules) do
        write(to .. "/" .. file, takes.prune(texts[file], played))
    end
    write(to .. "/shell.lua", shell)
end

local shell, count = takes.prune(texts[takes.template], {}), 1
checkpoint(takes.starter, shell, {})
for _, step in ipairs(takes.timeline(texts)) do
    if step.file == "shell.lua" then shell = step.after end
    checkpoint(step.take.name, shell, step.played)
    count = count + 1
end
sh("rm -rf " .. q(assets))
print(("wrote %d checkpoints to %s"):format(count, out))
