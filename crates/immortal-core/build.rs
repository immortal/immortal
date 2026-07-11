fn main() {
    if let Err(error) = built::write_built_file() {
        eprintln!("unable to generate build information: {error}");
        std::process::exit(1);
    }
}
