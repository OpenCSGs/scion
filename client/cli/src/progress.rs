//! git-style progress on standard error:
//! `Receiving objects:  45% (450/1000), 1.20 MiB | 3.00 MiB/s`, ending in `, done.`
//!
//! Shown only when standard error is a terminal (and not with `--json` or `--quiet`), like git.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Redraw at most this often, unless the percentage changes.
const REDRAW: Duration = Duration::from_millis(100);

pub struct Meter {
    inner: Option<Mutex<Inner>>,
}

struct Inner {
    out: Box<dyn Write + Send>,
    /// The stage being drawn, if any.
    stage: Option<Stage>,
}

struct Stage {
    title: &'static str,
    started: Instant,
    drawn: Instant,
    percent: Option<u64>,
    /// The latest numbers: done, total, bytes.
    latest: (u64, Option<u64>, Option<u64>),
    /// Length of the line last drawn, to blank out what a shorter line leaves behind.
    width: usize,
}

impl Meter {
    /// A meter on standard error, drawn only when `enabled` and standard error is a terminal.
    pub fn stderr(enabled: bool) -> Self {
        use std::io::IsTerminal;
        if enabled && std::io::stderr().is_terminal() {
            Self::to(Box::new(std::io::stderr()))
        } else {
            Self { inner: None }
        }
    }

    fn to(out: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Some(Mutex::new(Inner { out, stage: None })),
        }
    }

    /// Reports `done` of `total` (None when unknown) for `title`, and the bytes moved so far.
    /// A new title finishes the previous stage.
    pub fn update(&self, title: &'static str, done: u64, total: Option<u64>, bytes: Option<u64>) {
        let Some(inner) = &self.inner else { return };
        let Ok(mut inner) = inner.lock() else { return };
        let now = Instant::now();
        if inner.stage.as_ref().is_some_and(|s| s.title != title) {
            inner.finish();
        }
        let stage = inner.stage.get_or_insert_with(|| Stage {
            title,
            started: now,
            drawn: now - REDRAW,
            percent: None,
            latest: (0, None, None),
            width: 0,
        });
        stage.latest = (done, total, bytes);
        let percent = total.map(|t| percent(done, t));
        if percent == stage.percent && now.duration_since(stage.drawn) < REDRAW {
            return;
        }
        stage.percent = percent;
        stage.drawn = now;
        inner.draw(false);
    }

    /// Ends the current stage with `, done.`
    pub fn done(&self) {
        if let Some(Ok(mut inner)) = self.inner.as_ref().map(Mutex::lock) {
            inner.finish();
        }
    }
}

impl Inner {
    fn draw(&mut self, finished: bool) {
        let Some(s) = &mut self.stage else { return };
        let (done, total, bytes) = s.latest;
        let mut text = line(s.title, done, total, bytes, s.started.elapsed());
        if finished {
            text.push_str(", done.");
        }
        let pad = s.width.saturating_sub(text.len());
        s.width = text.len();
        let eol = if finished { "\n" } else { "" };
        let _ = write!(self.out, "\r{text}{:pad$}{eol}", "").and_then(|()| self.out.flush());
    }

    fn finish(&mut self) {
        if self.stage.is_some() {
            self.draw(true);
            self.stage = None;
        }
    }
}

fn percent(done: u64, total: u64) -> u64 {
    (done.min(total) * 100).checked_div(total).unwrap_or(100)
}

/// One progress line, as git draws it.
fn line(title: &str, done: u64, total: Option<u64>, bytes: Option<u64>, elapsed: Duration) -> String {
    let mut s = match (total, bytes) {
        (Some(t), Some(_)) => format!("{title}: {:>3}% ({done}/{t}), ", percent(done, t)),
        (Some(t), None) => format!("{title}: {:>3}% ({done}/{t})", percent(done, t)),
        // Only a size: `Receiving archive: 1.20 MiB`
        (None, Some(_)) => format!("{title}: "),
        (None, None) => format!("{title}: {done}"),
    };
    if let Some(b) = bytes {
        s.push_str(&human(b));
        let secs = elapsed.as_secs_f64();
        if secs >= 0.5 {
            s.push_str(&format!(" | {}/s", human((b as f64 / secs) as u64)));
        }
    }
    s
}

/// Sizes the way git prints them: bytes, then KiB / MiB / GiB with two decimals.
pub fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, size) in UNITS {
        if bytes >= size {
            return format!("{:.2} {unit}", bytes as f64 / size as f64);
        }
    }
    format!("{bytes} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("lock").clone()).expect("utf8")
        }
    }

    #[test]
    fn lines_look_like_git() {
        let ms = Duration::from_millis;
        assert_eq!(line("Listing files", 12, None, None, ms(0)), "Listing files: 12");
        assert_eq!(
            line("Receiving objects", 45, Some(100), Some(2048), ms(100)),
            "Receiving objects:  45% (45/100), 2.00 KiB"
        );
        assert_eq!(
            line("Receiving objects", 1, Some(1), Some(3 << 20), ms(2000)),
            "Receiving objects: 100% (1/1), 3.00 MiB | 1.50 MiB/s"
        );
        assert_eq!(
            line("Importing commits", 0, Some(0), None, ms(0)),
            "Importing commits: 100% (0/0)"
        );
        assert_eq!(
            line("Receiving archive", 0, None, Some(1 << 20), ms(1000)),
            "Receiving archive: 1.00 MiB | 1.00 MiB/s"
        );
        assert_eq!(human(10), "10 bytes");
        assert_eq!(human(1536), "1.50 KiB");
        assert_eq!(human(5 << 30), "5.00 GiB");
    }

    #[test]
    fn meter_draws_stages_and_finishes_them() {
        let buf = Buf::default();
        let m = Meter::to(Box::new(buf.clone()));
        m.update("Listing files", 1, None, None);
        m.update("Listing files", 2, None, None);
        m.update("Receiving objects", 1, Some(2), Some(10));
        m.update("Receiving objects", 1, Some(2), Some(10));
        m.update("Receiving objects", 2, Some(2), Some(20));
        m.done();
        m.done();
        let text = buf.text();
        assert!(text.starts_with("\rListing files: 1"), "{text:?}");
        // The unchanged second update within the redraw interval is skipped
        assert_eq!(text.matches("(1/2)").count(), 1, "{text:?}");
        assert!(text.contains("\rListing files: 2, done.\n"), "{text:?}");
        assert!(
            text.ends_with("\rReceiving objects: 100% (2/2), 20 bytes, done.\n"),
            "{text:?}"
        );
    }

    #[test]
    fn disabled_meter_is_silent() {
        let m = Meter::stderr(false);
        m.update("Listing files", 1, None, None);
        m.done();
        assert!(m.inner.is_none());
    }
}
