-- Load without user input. Each chain runs only while its knob is on, so the `bar` scenario has
-- no driver wakes of its own.
local QUERIES = { "", "a", "al", "alp", "ec", "echo", "o 1", "tango", "", "mi", "mike 0", "zz" }

return function(mode, cal_hidden, listwin, gallery, theme, fx)
    local flip = state("hs_flip", false)
    local day = state("hs_cal_day", 1)
    local tick = 0
    local busy_armed, flip_armed, fx_armed = false, false, false
    local fx_tick = 0

    -- Re-armed before the work, so a raise in the work (a blown CPU budget) cannot end the chain.
    -- Never reads `listwin.results`: that would run the fuzzy filter inside this callback's budget.
    local function busy()
        if mode:get() ~= "busy" then
            busy_armed = false
            return
        end
        timer(100, busy)
        tick = tick + 1
        listwin.pick((tick * 7) % 60 + 1)
        if tick % 2 == 0 then day:set(tick // 2 % 28 + 1) end
        if tick % 5 == 0 then
            listwin.query:set(QUERIES[tick // 5 % #QUERIES + 1])
            -- A fixed span, not `#gallery.rows:get()`: converting the 331-entry `mantle.files`
            -- snapshot inside this callback measured over the 2.5 ms budget. Past the end is a no-op.
            gallery.scroll:reveal((tick // 5 * 3) % 56 + 1)
        end
        if tick % 40 == 0 then cal_hidden:set(not cal_hidden:get()) end
    end

    -- `mantle call hs.fuzzy_us 200`: microseconds one fuzzy score costs here, to size `hs_rows`
    -- against the 2.5 ms budget a `computed` runs under.
    action("hs.fuzzy_us", function(n)
        n = tonumber(n) or 200
        local started = os.clock()
        for i = 1, n do fuzzy(string.format("tango mike %04d", i), "al") end
        return math.floor((os.clock() - started) / n * 1e6 + 0.5)
    end)

    local function flipper()
        if not flip:get() then
            flip_armed = false
            return
        end
        theme.mode:set(theme.mode:get() == "dark" and "light" or "dark")
        timer(3000, flipper)
    end

    -- Every 200 ms one phase flips, so each card's state changes every 400 ms. The strip scrolls on phase a.
    local function fx_flip()
        if not fx.on:get() then
            fx_armed = false
            return
        end
        timer(200, fx_flip)
        fx_tick = fx_tick + 1
        local phase = fx.phase[fx_tick % 2 + 1]
        phase:set(not phase:get())
        if fx_tick % 2 == 1 then fx.strip:scroll_to(fx_tick % 4 == 1 and 300 or 0) end
    end

    local function arm()
        if mode:get() == "busy" and not busy_armed then
            busy_armed = true
            timer(100, busy)
        end
        if flip:get() and not flip_armed then
            flip_armed = true
            timer(3000, flipper)
        end
        if fx.on:get() and not fx_armed then
            fx_armed = true
            timer(200, fx_flip)
        end
    end
    mode:on_change(arm)
    flip:on_change(arm)
    fx.on:on_change(arm)
    arm()

    local function report()
        local c = theme.counters
        log.info(string.format(
            "heavy counters: bar_child=%d bar_color=%d list_item=%d cal_cell=%d gallery_row=%d",
            c.bar_child, c.bar_color, c.list_item, c.cal_cell, c.gallery_row
        ))
        timer(10000, report)
    end
    timer(10000, report)
end
