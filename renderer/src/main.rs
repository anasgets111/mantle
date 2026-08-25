mod wayland;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    wayland::run()
}
