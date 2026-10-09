-- The demo shell's files: stage sources, each edit's plan, the snapshots and wallpapers it starts from.

local syntax, edits, session, takes, run, panes = require("lua_syntax"), require("edits"), require("session"),
    require("takes"), require("run"), require("panes")

local M = {}

local STAGES = mantle.config_dir .. "/stages/"
local COVERS = mantle.config_dir .. "/covers"
-- Rendered to PNG in the demo's `wallpapers/` at setup: `palette.quantize` reads no SVG. `mantle`
-- is the logo art the take opens on, so the last pick comes back to it.
local WALLPAPERS = {}
for name, svg in pairs(takes.wallpapers) do
    WALLPAPERS[name] = mantle.config_dir .. "/" .. svg
end
local WALLPAPER_ASPECT = 3440 / 1440

M.texts = {}

-- Each edit's file, texts and keystrokes, planned in `prepare` from a process callback: a diff
-- overruns the 2.5 ms budget a timer callback gets.
M.plans = {}

function M.prepare()
    for _, step in ipairs(takes.timeline(M.texts)) do
        M.plans[step.take.name] = {
            file = step.file,
            before = step.before,
            after = step.after,
            ops = edits.plan(step.before, step.after, step.take),
        }
    end
end

-- Tokenizes every line the take shows, 40 a callback.
function M.warm(done)
    local shown, pending = { takes.prune(M.texts[takes.template], {}) }, {}
    for _, plan in pairs(M.plans) do
        shown[#shown + 1] = plan.after
    end
    for _, text in ipairs(shown) do
        local split = edits.split(text)
        table.move(split, 1, #split, #pending + 1, pending)
    end
    local function at(k)
        if k > #pending then return done() end
        for i = k, math.min(k + 39, #pending) do
            syntax.highlight(pending[i])
        end
        timer(1, function() at(k + 40) end)
    end
    at(1)
end

-- Wide enough that `fit = "cover"` never upscales on a screen narrower than the art's aspect; the
-- 300 px copy is the picker tile and what `palette.quantize` decodes.
local function render_wallpapers(done)
    local screen = mantle.screens:get()[1] or { width = 1920, height = 1080 }
    local width = tostring(math.max(screen.width, math.ceil(screen.height * WALLPAPER_ASPECT)))
    local sizes = { [""] = width, ["thumbs/"] = "300" }
    local pending = 0
    for _ in pairs(WALLPAPERS) do
        pending = pending + 2
    end
    for name, svg in pairs(WALLPAPERS) do
        for dir, w in pairs(sizes) do
            local out = run.DEMO_DIR .. "/wallpapers/" .. dir .. name .. ".png"
            session.run("rsvg-convert", { "-w", w, "-o", out, svg }, function(code)
                if code ~= 0 then log.warn("rsvg-convert could not render", out) end
                pending = pending - 1
                if pending == 0 then done() end
            end)
        end
    end
end

-- Shows `text` as shell.lua and writes it with every module pruned to `played`, then starts the
-- demo shell on fresh state and gives it `settle` ms.
function M.start_shell(text, played, settle, next)
    panes.file_shown:set("shell.lua")
    panes.set_text(text)
    local function write_modules(name, written)
        session.write(run.DEMO_DIR .. "/" .. name, takes.prune(M.texts[name], played), written)
    end
    local function start()
        session.write(run.DEMO_DIR .. "/shell.lua", text, function()
            session.demo_shell:start("mantle", { "-c", run.DEMO_DIR })
            timer(settle, next)
        end)
    end
    -- The last take's saved switches would start this one with them on.
    session.run("rm", { "-rf", run.DEMO_DIR .. "/state" },
        function() session.each(takes.modules, write_modules, start) end)
end

-- The cold open's shell: the last stage with every module block in, beside fresh frags, shared
-- modules, covers and wallpapers.
function M.stage_final(next)
    local sources = {}
    for _, name in ipairs(takes.shared) do
        sources[#sources + 1] = mantle.config_dir .. "/" .. name
    end
    for _, name in ipairs(takes.frags) do
        sources[#sources + 1] = STAGES .. name
    end
    sources[#sources + 1] = run.DEMO_DIR .. "/"
    if run.SHOTS then session.run("mkdir", { "-p", run.SHOTS }) end
    session.run("mkdir", { "-p", run.DEMO_DIR .. "/wallpapers/thumbs" }, function()
        session.run("cp", { "-r", COVERS, run.DEMO_DIR .. "/" }, function()
            session.run("cp", sources, function()
                render_wallpapers(function() M.start_shell(takes.prune(M.texts[takes.template]), nil, 2500, next) end)
            end)
        end)
    end)
end

-- One at a time: each output line is a frame, and all files at once overflow the Supervisor's
-- 1024-frame queue, which then drops this Renderer as wedged.
function M.load_sources(done)
    session.each(takes.sources, function(name, next_source)
        session.run("cat", { STAGES .. name }, function(code, out)
            if code == 0 then M.texts[name] = table.concat(out, "\n") .. "\n" end
            next_source()
        end)
    end, done)
end

return M
