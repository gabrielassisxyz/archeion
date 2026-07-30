//! Where a run's subscription comes from, and what it is refused for.
//!
//! Two sources and neither of them the command line. An argument lands in shell history and in
//! the process table, where every other process of the same user can read it, so the credential
//! arrives as a file named by `--cookie-file` or in an environment variable. Both are readable
//! by this user's other processes too; what they are not is written down by a shell and shown to
//! everybody on the machine.

use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The environment variable a run may carry its `Cookie` header value in.
///
/// It names this tool and the header, and nothing else. A variable carrying a platform's name
/// would be the first place in this code that knew what a particular publisher is, and the
/// binding comes from the seed's own origin rather than from the variable, so one variable
/// serves every site.
pub const COOKIE_HEADER_VARIABLE: &str = "ARCHEION_COOKIE_HEADER";

/// Why the credential a run was given cannot be used. Every one of these is a refusal before
/// anything is fetched: a run that quietly went ahead anonymously would archive a publication's
/// paid half as teasers and say nothing about why.
///
/// None of them ever prints the value. The path, the permissions and the reason are what a
/// person needs; the credential is what a terminal, a scrollback and a log file must not get.
#[derive(Debug, thiserror::Error)]
pub enum UnusableCookie {
    #[error("{path} could not be read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} can be read by {who}: a file holding a session must be readable by its owner \
         alone, so run chmod 600 on it"
    )]
    ReadableByOthers { path: PathBuf, who: &'static str },
    #[error("{held_in} holds no cookie")]
    Empty { held_in: String },
    #[error(
        "{held_in} holds a character that cannot be sent in a header, so it is not a Cookie \
         header copied from a request"
    )]
    NotAHeaderValue { held_in: String },
}

/// The `Cookie` header value this run will carry, from whichever source holds one.
///
/// It answers with the value and not with a bound credential, because which origin a session
/// belongs to is the seed's business rather than this module's: what lives here is where a
/// credential may come from and what it is refused for.
///
/// The file wins over the environment when both are present, because a path typed on the
/// command line was chosen for this run while a variable may have been exported hours ago.
pub fn cookie_header_value(
    file: Option<&Path>,
    from_environment: Option<OsString>,
) -> Result<Option<String>, UnusableCookie> {
    match (file, from_environment) {
        (Some(path), _) => read_cookie_file(path).map(Some),
        (None, Some(exported)) => {
            let text =
                exported
                    .into_string()
                    .map_err(|_not_text| UnusableCookie::NotAHeaderValue {
                        held_in: COOKIE_HEADER_VARIABLE.to_owned(),
                    })?;
            usable_value(text, COOKIE_HEADER_VARIABLE.to_owned()).map(Some)
        }
        (None, None) => Ok(None),
    }
}

/// The credential a file holds, or the reason it is not one.
///
/// The file is opened once and its mode read off the descriptor rather than off the path a
/// second time, so the permissions that were checked belong to the bytes that were read.
fn read_cookie_file(path: &Path) -> Result<String, UnusableCookie> {
    let unreadable = |source: std::io::Error| UnusableCookie::Unreadable {
        path: path.to_owned(),
        source,
    };
    let mut file = fs::File::open(path).map_err(unreadable)?;
    let mode = file.metadata().map_err(unreadable)?.permissions().mode();
    if let Some(who) = who_else_can_read(mode) {
        return Err(UnusableCookie::ReadableByOthers {
            path: path.to_owned(),
            who,
        });
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(unreadable)?;
    // The path is the source named in a refusal, since that is what a person has to go and fix.
    // It is leaked into the message on purpose: a path is not a credential.
    usable_value(contents, path.display().to_string())
}

/// Who besides the owner can read a file, given its mode.
///
/// The check is written for Unix without a `#[cfg]` around it, and the reason is that the
/// alternative is worse: an arm for a platform nothing here is built for and no test ever runs
/// would be a silent hole rather than a portability win. The release matrix is Linux and macOS.
///
/// Only the read bits are read. Write and execute say something about who can damage the file,
/// which is a different question from who can walk off with the session in it.
fn who_else_can_read(mode: u32) -> Option<&'static str> {
    match (mode & 0o040 != 0, mode & 0o004 != 0) {
        (true, true) => Some("its group and by anybody else"),
        (true, false) => Some("its group"),
        (false, true) => Some("anybody"),
        (false, false) => None,
    }
}

/// The value as it will be sent, or the reason it will not be.
///
/// A file ends in a newline, and a header value may not contain one, so the trim is what makes
/// an ordinary file work rather than a leniency. What is refused after it is a value carrying a
/// character no header can hold: a newline in the middle of one is a second header the operator
/// did not write, which is how a header injection is spelled.
///
/// Every refusal names where the run was told to look, a path for a file and the variable's own
/// name for the environment, because that is the thing a person has to go and edit.
fn usable_value(value: String, held_in: String) -> Result<String, UnusableCookie> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(UnusableCookie::Empty { held_in });
    }
    if !value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Err(UnusableCookie::NotAHeaderValue { held_in });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::TempDir;

    use super::*;

    fn cookie_file(dir: &TempDir, contents: &str, mode: u32) -> PathBuf {
        let path = dir.path().join("substack.cookie");
        let mut file = fs::File::create(&path).expect("a file in a temp dir");
        file.write_all(contents.as_bytes())
            .expect("the write lands");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("permissions are set");
        path
    }

    /// A credential every other process on the machine can read is not one, and the refusal says
    /// which of the two ways it is exposed because that is how far the exposure went: a file its
    /// group can read was seen by the people in that group, and one anybody can read was seen by
    /// every account on the machine. The fix is `chmod 600` either way.
    #[test]
    fn a_cookie_file_readable_by_group_or_by_others_is_refused_saying_which() {
        let dir = TempDir::new().expect("temp dir");

        for (mode, who) in [
            (0o640, "its group"),
            (0o604, "anybody"),
            (0o644, "its group and by anybody else"),
            (0o444, "its group and by anybody else"),
        ] {
            let path = cookie_file(&dir, "substack.sid=secret", mode);
            let refusal = cookie_header_value(Some(&path), None)
                .expect_err("a readable credential was accepted")
                .to_string();

            assert!(
                refusal.contains(who),
                "mode {mode:o} was refused with: {refusal}"
            );
            assert!(!refusal.contains("secret"), "the refusal printed the value");
        }
    }

    #[test]
    fn a_cookie_file_only_its_owner_can_read_is_used() {
        let dir = TempDir::new().expect("temp dir");
        let path = cookie_file(&dir, "substack.sid=secret\n", 0o600);

        assert_eq!(
            cookie_header_value(Some(&path), None).expect("a file only its owner can read"),
            Some("substack.sid=secret".to_owned()),
            "the newline every file ends with is not part of the header"
        );
    }

    /// Neither source is required, and either one is enough. A run given nothing archives what
    /// an anonymous reader is served, which is what every run did before this existed.
    #[test]
    fn the_environment_variable_is_the_alternative_to_the_file_and_neither_is_required() {
        assert_eq!(
            cookie_header_value(None, None).expect("a run may carry no session"),
            None
        );
        assert_eq!(
            cookie_header_value(None, Some(OsString::from("substack.sid=exported")))
                .expect("an exported value is usable"),
            Some("substack.sid=exported".to_owned())
        );
    }

    /// Both present is not an error, and which one wins is documented rather than discovered: a
    /// path typed for this run beats a variable exported at some point earlier.
    #[test]
    fn a_cookie_file_wins_over_a_variable_left_in_the_environment() {
        let dir = TempDir::new().expect("temp dir");
        let path = cookie_file(&dir, "substack.sid=from-the-file", 0o600);

        assert_eq!(
            cookie_header_value(
                Some(&path),
                Some(OsString::from("substack.sid=from-the-environment")),
            )
            .expect("the file is read"),
            Some("substack.sid=from-the-file".to_owned())
        );
    }

    /// A newline inside the value is a second header nobody wrote, which is what a header
    /// injection is. It is refused at the one place that can tell somebody, rather than
    /// dropped further in where the run would simply go out anonymous.
    #[test]
    fn a_value_no_header_could_carry_is_refused() {
        let dir = TempDir::new().expect("temp dir");

        for hostile in [
            "substack.sid=secret\r\nX-Something: else",
            "substack.sid=secret\nX-Something: else",
            "substack.sid=\u{0}secret",
        ] {
            let path = cookie_file(&dir, hostile, 0o600);
            let refusal = cookie_header_value(Some(&path), None)
                .expect_err("a value no header can carry was accepted")
                .to_string();

            assert!(
                refusal.contains("cannot be sent in a header"),
                "it was refused with: {refusal}"
            );
            assert!(
                refusal.contains("substack.cookie"),
                "it was refused without naming the file: {refusal}"
            );
        }
    }

    #[test]
    fn a_file_holding_nothing_is_refused_rather_than_read_as_no_session() {
        let dir = TempDir::new().expect("temp dir");
        let path = cookie_file(&dir, "\n  \n", 0o600);

        let refusal = cookie_header_value(Some(&path), None)
            .expect_err("an empty file was accepted")
            .to_string();

        assert!(refusal.contains("holds no cookie"), "{refusal}");
        assert!(
            refusal.contains("substack.cookie"),
            "it was refused without naming the file: {refusal}"
        );
    }

    /// The variable is named the same way a path is, since a refusal that says only "it holds no
    /// cookie" leaves an operator with both sources to check and no reason to prefer either.
    #[test]
    fn a_variable_holding_nothing_is_refused_by_name() {
        let refusal = cookie_header_value(None, Some(OsString::from("   ")))
            .expect_err("an empty variable was accepted")
            .to_string();

        assert!(refusal.contains(COOKIE_HEADER_VARIABLE), "{refusal}");
    }

    #[test]
    fn a_cookie_file_that_is_not_there_is_refused_by_name() {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("nothing-here.cookie");

        assert!(
            cookie_header_value(Some(&missing), None)
                .expect_err("a missing file was accepted")
                .to_string()
                .contains("nothing-here.cookie")
        );
    }
}
