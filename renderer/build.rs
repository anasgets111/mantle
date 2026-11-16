fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    std::fs::write(output.join("check_samples.json"), shared::schema::check_samples())
        .expect("the build output directory is writable");
    std::fs::write(output.join("actions.rs"), actions()).expect("the build output directory is writable");
}

/// `actions(capability)`: each capability's action names from its serde action enum, so the
/// methods `mantle.<name>` resolves cannot drift from what the Supervisor decodes.
fn actions() -> String {
    let mut arms = String::new();
    for (name, _, schema) in shared::schema::capability_schemas() {
        let capability = shared::Capability::from_name(name).expect("a roster name");
        let names = schema.as_ref().map_or_else(Vec::new, |schema| shared::schema::action_variants(schema.as_value()));
        let names: Vec<String> = names.into_iter().map(|(action, _)| format!("{action:?}")).collect();
        arms.push_str(&format!("        shared::Capability::{capability:?} => &[{}],\n", names.join(", ")));
    }
    format!(
        "/// The action names, each a method on `mantle.<name>`; empty for a read-only capability. Any\n\
         /// other name is refused at call time; the Supervisor still validates arguments.\n\
         pub const fn actions(capability: shared::Capability) -> &'static [&'static str] {{\n\
         \x20   match capability {{\n{arms}    }}\n}}\n"
    )
}
