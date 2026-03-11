use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// Create and start a spinner on stderr that shows a loading animation.
///
/// The spinner only renders if stderr is a TTY; otherwise it stays hidden
/// so piped/redirected output is not polluted.
///
/// Call `finish()` or `finish_and_clear()` on the returned `ProgressBar`
/// to stop the animation.
pub fn start_spinner(message: &str) -> ProgressBar {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        // Not a TTY — return a hidden (no-draw) spinner so callers
        // don't need to branch on TTY themselves.
        let pb = ProgressBar::hidden();
        return pb;
    }

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["   ", ".  ", ".. ", "...", " ..", "  .", "   "]),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}
