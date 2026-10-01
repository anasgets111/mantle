fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    std::fs::write(output.join("check_samples.json"), shared::schema::check_samples())
        .expect("the build output directory is writable");
}
