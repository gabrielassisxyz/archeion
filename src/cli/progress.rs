//! What both `capture` and `repass` share about saying something while they run: the level an
//! operator asks for, the one line each level ends up drawing on stderr, and the thread that
//! keeps that line moving while the run itself is waiting on a host.
//!
//! Nothing here knows what a page or a capture produced; that vocabulary belongs to the verb
//! printing it. What is shared is the mechanics of a line that either redraws in place or
//! never does, because a terminal and a pipe are the two destinations stderr can be and a bar
//! answers to both differently.

use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// How often the ticking thread wakes to see whether the bar has gone quiet. It is not how
/// often the bar is redrawn: that is [`TTY_REDRAW_AFTER`] and [`PIPE_LINE_AFTER`] below. A
/// short poll is what keeps a finished run from waiting out a whole tick before it can exit.
const TICK_POLL: Duration = Duration::from_millis(100);

/// How long a terminal's bar may sit unchanged before the ticking thread redraws it. The
/// countdown and the time since the last page are the two figures that move on their own, and
/// a host that stopped answering is exactly the case where nothing else will move them.
const TTY_REDRAW_AFTER: Duration = Duration::from_secs(1);

/// The same, off a terminal, where every redraw is a line in a log file forever. Far rarer on
/// purpose: what this has to answer is "is the run alive", and a line every few seconds
/// answers it without turning a stalled run into a megabyte of identical lines.
const PIPE_LINE_AFTER: Duration = Duration::from_secs(5);

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

/// Whatever a verb's bar is counting, rendered into the one line the bar draws.
///
/// It is a trait rather than a string the caller keeps up to date because a bar has to be
/// redrawable when nothing has happened: the figures that matter most while a run is stuck,
/// the deadline left and how long since anything arrived, are read off a clock rather than
/// pushed in by an event that is never coming.
pub trait BarFacts: Send + 'static {
    /// The line to draw right now. Called under the bar's own lock, from the run's own thread
    /// and from the ticking thread alike.
    fn line(&self) -> String;
}

/// A bar on stderr that keeps moving while the run behind it is waiting.
///
/// The reason this exists at all is the case the bead was opened for: a host that stopped
/// answering leaves the run with nothing to report, so a bar drawn only when a page arrives
/// freezes at exactly the moment an operator most needs to know whether anything is happening.
/// A thread that wakes on its own and redraws from the clock is the answer, and it is allowed
/// to be one only because of what it does not do: it reads shared counters and writes stderr,
/// and it never waits, paces or gates anything the run asks a host for. The run makes the same
/// requests in the same order whether or not this is here.
pub struct TickingBar<F: BarFacts> {
    shared: Arc<Mutex<DrawnBar<F>>>,
    stop: Arc<AtomicBool>,
    /// `None` only after `Drop` has taken it to join the thread.
    ticker: Option<JoinHandle<()>>,
}

/// The bar and everything it is drawn from, behind the one lock the run's thread and the
/// ticking thread take turns on. `last_printed` is what makes the tick polite off a terminal:
/// it moves on every write to stderr this bar makes, warnings included, so the tick only ever
/// prints into a window in which nothing else did.
struct DrawnBar<F: BarFacts> {
    line: ProgressLine,
    facts: F,
    last_printed: Instant,
    redraw_after: Duration,
}

impl<F: BarFacts> DrawnBar<F> {
    fn draw(&mut self) {
        let text = self.facts.line();
        self.line.update(&mut io::stderr(), &text);
        self.last_printed = Instant::now();
    }
}

impl<F: BarFacts> TickingBar<F> {
    pub fn new(is_tty: bool, facts: F) -> Self {
        let redraw_after = if is_tty {
            TTY_REDRAW_AFTER
        } else {
            PIPE_LINE_AFTER
        };
        let shared = Arc::new(Mutex::new(DrawnBar {
            line: ProgressLine::new(is_tty),
            facts,
            last_printed: Instant::now(),
            redraw_after,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = thread::spawn({
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(TICK_POLL);
                    let Ok(mut bar) = shared.lock() else {
                        return;
                    };
                    if bar.last_printed.elapsed() >= bar.redraw_after {
                        bar.draw();
                    }
                }
            }
        });
        Self {
            shared,
            stop,
            ticker: Some(ticker),
        }
    }

    /// Changes what the bar counts and redraws it, which is what a page or an item arriving
    /// does. The two happen under one lock so the ticking thread cannot draw the half-updated
    /// state in between.
    pub fn advance(&mut self, change: impl FnOnce(&mut F)) {
        let Ok(mut bar) = self.shared.lock() else {
            return;
        };
        change(&mut bar.facts);
        bar.draw();
    }

    /// Writes something that is not the bar to the same stream, with the bar taken down first
    /// and not put back until the next update. A warning sharing a line with a redraw is the
    /// corruption this exists to rule out rather than merely to make unlikely, which is why it
    /// holds the lock across the whole write: the ticking thread cannot redraw into the gap.
    pub fn interrupt(&mut self, lines: impl IntoIterator<Item = String>) {
        let Ok(mut bar) = self.shared.lock() else {
            return;
        };
        let mut stderr = io::stderr();
        bar.line.clear_for_interruption(&mut stderr);
        for line in lines {
            let _ = writeln!(stderr, "warning: {line}");
        }
        bar.last_printed = Instant::now();
    }

    /// Takes the bar down and leaves it down until something draws it again. Called before
    /// anything is written to stdout, and again by `Drop` so that every path out of a run,
    /// the ones that return an error included, leaves the terminal with its own line free.
    pub fn clear(&mut self) {
        let Ok(mut bar) = self.shared.lock() else {
            return;
        };
        bar.line.clear_for_interruption(&mut io::stderr());
    }
}

impl<F: BarFacts> Drop for TickingBar<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        // After the join, so nothing can redraw over the clear: a bar left on the line is what
        // the next thing written to this terminal lands on top of, and for a run ending in an
        // error that next thing is the error.
        self.clear();
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
        let second = written
            .split('\r')
            .next_back()
            .expect("a redraw was written");
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

    struct CountedFacts(usize);

    impl BarFacts for CountedFacts {
        fn line(&self) -> String {
            format!("{} so far", self.0)
        }
    }

    /// A bar off a terminal keeps its tick rare on purpose, because every tick there is a line
    /// in a log file that nothing ever erases. Five seconds is far longer than any test may
    /// wait for, so what is asserted is the number the choice turns on rather than a wait.
    #[test]
    fn a_bar_on_a_pipe_ticks_far_more_rarely_than_one_on_a_terminal() {
        assert!(PIPE_LINE_AFTER >= TTY_REDRAW_AFTER * 4);
        assert!(TICK_POLL < TTY_REDRAW_AFTER);
    }

    /// The bar the run itself drives, with the ticking thread running beside it the whole
    /// time: an update still lands exactly once per call, so the thread is not stealing
    /// redraws from the run or adding any of its own inside a window this short.
    #[test]
    fn a_ticking_bar_still_draws_once_per_advance_while_its_thread_runs() {
        let mut bar = TickingBar::new(false, CountedFacts(0));
        bar.advance(|facts| facts.0 = 1);
        bar.advance(|facts| facts.0 = 2);
        let seen = bar
            .shared
            .lock()
            .expect("the bar's lock is not poisoned")
            .facts
            .0;
        assert_eq!(seen, 2);
    }
}
