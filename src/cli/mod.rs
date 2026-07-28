//! The command line surface, one module per verb.
//!
//! These modules belong to the binary and to nothing else: `lib.rs` does not declare them,
//! so no part of the library can come to depend on how a verb prints. What crosses in the
//! other direction is only the library's own types, which is why the shape of every machine
//! readable answer is declared here rather than derived on a record that is free to grow.

pub mod capture;
pub mod export;
pub mod list;
pub mod repass;

use std::io::{self, Write as _};

/// Writes what the command has to say, and treats a reader that went away as the end of the
/// work rather than as a failure: `archeion list | head` closes the pipe on purpose.
pub fn write_stdout(output: &str) -> io::Result<()> {
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    match stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

/// Says what the archive should hold and does not, on stderr in every mode. A pipeline
/// reading the records on stdout is exactly the caller that must still be told.
pub fn warn(lines: impl IntoIterator<Item = String>) {
    for line in lines {
        eprintln!("warning: {line}");
    }
}

/// What a verb that walked a damaged archive answers with. The walk is still worth printing,
/// so the damage is reported after the records rather than instead of them, and the exit code
/// is what keeps a script from reading a short answer as a complete one.
pub fn damaged_archive(unreadable: usize) -> String {
    format!("archive has {unreadable} unreadable item(s)")
}
