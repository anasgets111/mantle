function __after()
    state("volume_osd", { volume = 0, muted = false }):set({ volume = 42, muted = false })
end
