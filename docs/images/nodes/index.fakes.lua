-- The switching-views shot: swap the tab after the first layout, so the frames show the crossfade.
__after = function() state("tab", "wifi"):set("bluetooth") end
