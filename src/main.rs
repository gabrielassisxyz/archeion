// The storage layer exists, but nothing fetches into it yet. This entry point stays
// deliberately empty rather than growing a stub of the future CLI: the command surface
// follows the capture path, and a guessed surface would have to be unlearned.
fn main() {
    println!(
        "{} {}: the archival core is not implemented yet",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}
