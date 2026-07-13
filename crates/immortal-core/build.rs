use std::path::Path;

mod build_support;

fn main() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in build_support::git_watch_paths(&workspace) {
        println!("cargo::rerun-if-changed={}", path.display());
    }

    if let Err(error) = built::write_built_file() {
        eprintln!("unable to generate build information: {error}");
        std::process::exit(1);
    }
}
