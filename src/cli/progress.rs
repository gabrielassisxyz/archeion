//! What both `capture` and `repass` share about saying something while they run: the level an
//! operator asks for, and the one line each level ends up drawing on stderr.
//!
//! Nothing here knows what a page or a capture produced; that vocabulary belongs to the verb
//! printing it. What is shared is the mechanics of a line that either redraws in place or
//! never does, because a terminal and a pipe are the two destinations stderr can be and a bar
//! answers to both differently.

/// How much a run says about itself while it goes. Absent (`None` on the flag that carries
/// this) is today's silence; naming a level here is opting into one of the two questions the
/// bead settled on: `Lines` answers what happened to each URL, `Bar` answers how far along the
/// run is. A bare `--progress` lands on `Lines`, since it costs nothing on a pipe and needs no
/// terminal to be useful; naming `bar` explicitly is what asks for the one that does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProgressLevel {
    Lines,
    Bar,
}

/// One line of live progress on stderr.
///
/// On a terminal it redraws in place with a carriage return, the way a bar reads at a glance
/// rather than as a scrollback nobody wants. Off one, every update is its own complete line
/// instead: a pipe or a log file has no cursor to move, and a carriage return written to either
/// is a byte sitting in the file forever rather than the redraw it meant on a screen. Which of
/// the two a destination is is the one decision here that needs no socket to exercise, so it is
/// taken as a plain `bool` rather than asked of `std::io::stderr()` inside this type: a test
/// drives both paths against an in-memory buffer, and the caller is the one place that reads
/// the real answer, once, off `IsTerminal`.
pub struct ProgressLine {
    is_tty: bool,
    /// How wide the line currently on screen is, in characters. Zero means nothing is drawn:
    /// either nothing has been written yet, or the last write was `clear_for_interruption`,
    /// which is what lets a caller ask "is there anything to clear" without keeping a second
    /// flag beside this one.
    drawn_width: usize,
}

impl ProgressLine {
    pub fn new(is_tty: bool) -> Self {
        Self {
            is_tty,
            drawn_width: 0,
        }
    }

    /// Draws one update. A terminal gets a carriage return back to the start of the line, the
    /// new text, and enough trailing spaces to erase whatever the previous update left past the
    /// new text's own end; anything else gets `text` as a line of its own.
    pub fn update(&mut self, out: &mut impl std::io::Write, text: &str) {
        if self.is_tty {
            let pad = self.drawn_width.saturating_sub(text.chars().count());
            let _ = write!(out, "\r{text}{:pad$}", "", pad = pad);
            let _ = out.flush();
            self.drawn_width = text.chars().count();
        } else {
            let _ = writeln!(out, "{text}");
        }
    }

    /// Erases whatever this line last drew, so something else, a warning chief among them, can
    /// be written to the same stream without a redraw shredding it or being shredded by it. A
    /// no-op off a terminal: nothing written there is ever left without its own newline, so
    /// there is nothing sitting on the current line to protect.
    pub fn clear_for_interruption(&mut self, out: &mut impl std::io::Write) {
        if self.is_tty && self.drawn_width > 0 {
            let _ = write!(out, "\r{:width$}\r", "", width = self.drawn_width);
            let _ = out.flush();
            self.drawn_width = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// The decision `ProgressLine` exists to isolate: off a terminal, an update is a plain
    /// line and nothing else, with no carriage return and no trailing padding a person would
    /// never see anyway.
    #[test]
    fn off_a_terminal_an_update_is_a_plain_line() {
        let mut line = ProgressLine::new(false);
        let mut out = Vec::new();

        line.update(&mut out, "3/10 pages");
        line.update(&mut out, "4/10 pages");

        assert_eq!(out, b"3/10 pages\n4/10 pages\n");
    }

    /// On a terminal, a shorter second update still erases the longer first one: the pad is
    /// measured against what was drawn before, not against the new text alone.
    #[test]
    fn on_a_terminal_a_shorter_update_erases_the_longer_line_it_replaces() {
        let mut line = ProgressLine::new(true);
        let mut out = Vec::new();

        line.update(&mut out, "10/10 pages, 3s left");
        line.update(&mut out, "1/1");

        let written = String::from_utf8(out).expect("only ASCII was written");
        assert!(!written.contains('\n'), "a bar update ended its own line");
        let second = written.split('\r').next_back().expect("a redraw was written");
        assert_eq!(second.trim_end(), "1/1");
        assert_eq!(
            second.len(),
            "10/10 pages, 3s left".len(),
            "the shorter update did not erase the longer one it replaced"
        );
    }

    /// The one thing a warning cannot survive: a redraw landing on top of it, or it landing on
    /// top of a redraw with nothing to separate the two. Clearing first makes the warning the
    /// only thing on the line.
    #[test]
    fn clearing_before_a_warning_leaves_no_trace_of_the_bar_on_its_line() {
        let mut line = ProgressLine::new(true);
        let mut out = Vec::new();

        line.update(&mut out, "42/200 pages, 12s left");
        line.clear_for_interruption(&mut out);
        let _ = writeln!(out, "warning: the seed's robots.txt could not be read");

        let written = String::from_utf8(out).expect("only ASCII was written");
        let after_last_redraw = written.rsplit('\r').next().expect("a clear was written");
        assert_eq!(
            after_last_redraw,
            "warning: the seed's robots.txt could not be read\n"
        );
    }

    /// A no-op off a terminal: there is no partial line there to protect, since every update
    /// off one already ends with its own newline.
    #[test]
    fn clearing_off_a_terminal_writes_nothing() {
        let mut line = ProgressLine::new(false);
        let mut out = Vec::new();

        line.update(&mut out, "3/10 pages");
        line.clear_for_interruption(&mut out);

        assert_eq!(out, b"3/10 pages\n");
    }
}
