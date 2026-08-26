// ponytail: this whole subtree has no production caller yet -- see lua/mod.rs's doc comment.
// Phase 11 gives `Loader` its first real caller.
#[allow(dead_code)]
mod lua;
mod socket;
mod text;
mod wayland;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    socket::spawn_client();
    wayland::run()
}
