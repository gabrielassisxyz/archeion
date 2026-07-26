//! Crawls a seed into an archive with the real engine.
//!
//! It exists because the adapter is the one part of the capture path no test can cover: a
//! test that reaches the web is a crawl, not a test. Point this at a server you are
//! running locally and the whole path is exercised for real, from the engine's page
//! events to the records on disk.
//!
//! ```sh
//! cargo run --example capture_seed -- http://127.0.0.1:8000/index.html /tmp/archive
//! ```

use archeion::capture::capture_seed;
use archeion::crawl::{Seed, SpiderEngine};
use archeion::storage::Archive;

fn main() {
    let mut args = std::env::args().skip(1);
    let seed_url = args
        .next()
        .expect("usage: capture_seed <seed url> <archive dir>");
    let archive_dir = args
        .next()
        .expect("usage: capture_seed <seed url> <archive dir>");

    let archive = Archive::open(archive_dir).expect("the archive opens");
    let mut seed = Seed::new(seed_url);
    seed.max_pages = 10;
    seed.concurrency = 4;

    let run = capture_seed(&SpiderEngine, &archive, &seed).expect("the run completes");
    println!("{run:#?}");
}
