-- The OSD is up before the shot, so `__after` can drop its card and play the exit.
state("osd_shown", false):set(true)

-- What each shot on the page animates: the easing and spring races, the staggered cards, the tap,
-- the OSD card leaving, a dismissed notification.
function __after()
    state("go", false):set(true)
    local taps = state("taps", 0)
    taps:set(taps:get() + 1)
    state("osd_shown", false):set(false)
    state("notes", { "Battery low", "Update ready", "Download complete" }):set({ "Battery low", "Download complete" })
end
