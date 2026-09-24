fakes = {}
fakes.system = { time = os.time() }
state("launcher_open", false):set(true)
-- A level change after the first layout, as `mantle set osd_level 0.7` would make: the OSD's pulse.
function __after() state("osd_level", 0.5):set(0.7) end
