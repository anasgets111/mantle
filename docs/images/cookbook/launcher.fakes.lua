fakes = {}
fakes.applications = { by_app_id = {}, entries = {
    { id = "org.gnome.Nautilus", name = "Files", icon = "folder", keywords = { "folder" } },
    { id = "foot", name = "Foot", generic_name = "Terminal", icon = "utilities-terminal", keywords = {} },
    { id = "firefox", name = "Firefox", generic_name = "Web Browser", icon = "web-browser", keywords = { "www" } },
} }
state("launcher_open", false):set(true)
