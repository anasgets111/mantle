fakes = {}
fakes.mpris = { players = {
    { id = "mpv", identity = "mpv", title = "Night drive", artist = "Artist", album_art_path = "/tmp/b.jpg", desktop_entry = "mpv",
      length = 200000000, play_state = "Playing", position = 50000000, position_updated_at = 7, url = "" },
} }
fakes.system = { time = os.time(), monotonic = 100 }
state("media_open", false):set(true)
-- The pill's rect, as its click would pass it.
state("media_anchor", {}):set({ x = 277, y = 3, width = 150, height = 26 })
