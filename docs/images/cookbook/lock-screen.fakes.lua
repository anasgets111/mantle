fakes = {}
fakes.lock = { active = true, attempts = 2, authenticating = false, error = "authentication failed", unlocking = false }
fakes.clock = { time = os.time(), monotonic = 100 }
