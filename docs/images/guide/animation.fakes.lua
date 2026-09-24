-- What each shot on the page animates: the tap, the OSD showing, a dismissed card.
function __after()
    local taps = state("taps", 0)
    taps:set(taps:get() + 1)
    state("osd_shown", false):set(true)
    state("notes", { "Battery low", "Update ready", "Download complete" }):set({ "Battery low", "Download complete" })
end
