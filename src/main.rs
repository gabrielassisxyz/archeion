// Capturing a seed into an archive works as a library, but the flags that would drive it
// are still being decided by the execution policy above it. This entry point stays
// deliberately empty rather than growing a stub of the future CLI: a guessed surface
// would have to be unlearned. `examples/capture_seed.rs` is the path to run meanwhile.
fn main() {
    println!(
        "{} {}: the archival core is not implemented yet",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}
