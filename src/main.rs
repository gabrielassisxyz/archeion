// The archival core does not exist yet. This entry point is deliberately empty rather
// than a stub of the future CLI: the command surface follows the item/capture/asset
// model, which is the next thing to be designed, and a guessed surface would have to be
// unlearned.
fn main() {
    println!(
        "{} {}: the archival core is not implemented yet",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}
