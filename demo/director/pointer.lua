-- A drawn pointer and spotlight over the demo shell, aimed at its nodes through `mantle call where`.

local theme, layout, run, panes, beats = require("theme"), require("layout"), require("run"), require("panes"),
    require("beats")

local M = {}

local POINTER = mantle.config_dir .. "/pointer.svg"

-- A drawn pointer, so a click the director fakes reads as one. The tip is the image's top left.
local pointer = state("demo_pointer", { x = 0, y = 0, shown = false })
local pointer_clicks = state("demo_pointer_clicks", 0)
local pressed = pulse(pointer_clicks, 320)
local pointer_at = pointer:map(function(p) return { x = p.x, y = p.y } end)
local spot = state("demo_spot", false)

M.pane = panel {
    id = "pointer",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    visible = computed({ pointer, spot }, function(p, s) return p.shown or s ~= false end),
    child = rect {
        width = "fill",
        height = "fill",
        children = {
            rect {
                children = spot:map(function(s)
                    if not s then return {} end
                    return { rect {
                        id = "spot:" .. s.serial,
                        width = s.width + 16,
                        height = s.height + 16,
                        radius = 14,
                        border_width = 3,
                        border_color = theme.accent,
                        translate = { x = s.x - 8, y = s.y - 8 },
                        opacity = 0,
                        animate = { opacity = { duration = 400, keyframes = { 1, { value = 1, duration = 1600 }, 0 } } },
                    } }
                end),
            },
            rect {
                width = 44,
                height = 44,
                radius = 22,
                border_width = 3,
                border_color = theme.accent,
                translate = pointer:map(function(p) return { x = p.x - 22, y = p.y - 22 } end),
                opacity = pressed:map(function(on) return on and 1 or 0 end),
                scale = pressed:map(function(on) return on and 1 or 0.4 end),
                animate = {
                    translate = { duration = 600, easing = "in_out_cubic" },
                    opacity = 220,
                    scale = { duration = 320, easing = "out_cubic" },
                },
            },
            image {
                visible = pointer:map(function(p) return p.shown end),
                source = POINTER,
                width = 34,
                height = 44,
                translate = pointer_at,
                scale = pressed:map(function(on) return on and 0.86 or 1 end),
                origin = { x = 0, y = 0 },
                shadows = { { color = "#00000066", blur = 8, offset = { y = 2 } } },
                animate = {
                    translate = { duration = 600, easing = "in_out_cubic" },
                    scale = { duration = 160, easing = "out_cubic" },
                },
            },
        },
    },
}

-- Each demo surface's top-left on screen. The bar's exclusive zone adds BAR on top of the
-- margin `layout` gives the popup.
local function origin_of(surface)
    if surface == "bar" then return { x = 0, y = 0 } end
    local box = layout.popup(surface, layout.stage(mantle.screens:get(), panes.stage_full:get()))
    return { x = box.left, y = panes.BAR + box.top }
end

-- Hands `found` the demo shell's node `name` on `surface` as a box on screen, or calls `next`. A
-- node not laid out yet, as on a popup just opened, gets 1.5 s.
function M.locate(name, surface, next, found, tries)
    if run.fast then return next() end
    run.demo({ "call", "where", name }, function(code, out)
        local ok, box = pcall(json.decode, table.concat(out or {}, "\n"))
        if code ~= 0 or not ok or type(box) ~= "table" or not box.width then
            if (tries or 0) < 15 then
                return timer(100, function() M.locate(name, surface, next, found, (tries or 0) + 1) end)
            end
            log.warn("no pointer target", name)
            return next()
        end
        local o = origin_of(surface)
        found({ x = o.x + box.x, y = o.y + box.y, width = box.width, height = box.height })
    end)
end

-- Rings node `name` on `surface` for a moment, to draw the eye to a payoff that is small.
function M.spotlight(name, surface)
    return function(next)
        M.locate(name, surface, next, function(box)
            box.serial = run.progress
            spot:set(box)
            next()
        end)
    end
end

-- Glides the pointer onto the demo shell's node `name` on `surface`, then clicks. It maps again
-- first: a surface mapped later stacks above it, and the demo's popups open after it shows.
function M.point(name, surface)
    return function(next)
        M.locate(name, surface, next, function(box)
            local x, y = box.x + box.width / 2, box.y + box.height / 2
            local last = pointer:get()
            local from = last.x ~= 0 and last or { x = x + 180, y = y + 240 }
            pointer:set({ x = from.x, y = from.y, shown = false })
            timer(34, function()
                pointer:set({ x = from.x, y = from.y, shown = true })
                timer(50, function()
                    pointer:set({ x = x, y = y, shown = true })
                    timer(640, function()
                        pointer_clicks:set(pointer_clicks:get() + 1)
                        timer(240, next)
                    end)
                end)
            end)
        end)
    end
end

function M.hide_pointer(next)
    spot:set(false)
    local p = pointer:get()
    pointer:set({ x = p.x, y = p.y, shown = false })
    next()
end

-- Drags notification `id` right past the dismiss threshold: the fake pointer cannot drag, so the
-- glide and the card's `notif_drag` offset move together, eased as the pointer is.
function M.swipe(id)
    return function(next)
        local function done()
            beats.feed("mock_notifications", { dnd = false, feed = {} })(function()
                beats.feed("notif_drag", { id = "", x = 0 })(next)
            end)
        end
        M.locate("notification", "notifications", done, function(box)
            -- Short of the module's 40% threshold, so the card is seen moving before it lets go.
            local x, y, reach = box.x + box.width * 0.3, box.y + box.height / 2, box.width * 0.36
            pointer:set({ x = x, y = y, shown = true })
            timer(700, function()
                pointer_clicks:set(pointer_clicks:get() + 1)
                pointer:set({ x = x + reach, y = y, shown = true })
                local function at(k)
                    if k > 12 then
                        pointer:set({ x = x + box.width * 0.6, y = y, shown = true })
                        return beats.feed("notif_drag", { id = id, x = math.floor(box.width * 0.5) })(function()
                            timer(350, function() M.hide_pointer(done) end)
                        end)
                    end
                    local t = k / 12
                    local eased = t < 0.5 and 4 * t ^ 3 or 1 - (2 - 2 * t) ^ 3 / 2
                    beats.feed("notif_drag", { id = id, x = math.floor(reach * eased) })(function()
                        timer(k == 12 and 250 or 30, function() at(k + 1) end)
                    end)
                end
                at(1)
            end)
        end)
    end
end

-- Where a take begins: hidden, with the first glide coming in from below right.
function M.reset()
    pointer:set({ x = 0, y = 0, shown = false })
    spot:set(false)
end

return M
