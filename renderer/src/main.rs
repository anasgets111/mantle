mod socket;
mod text;
mod wayland;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    socket::spawn_client();
    wayland::run()
}
