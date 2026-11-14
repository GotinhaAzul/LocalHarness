//! Terminal setup and restore: raw mode, the alternate screen, and a panic
//! hook that always gives the user their shell back.

use super::*;

/// Room for a full repaint of a large terminal, so a frame leaves in one
/// write instead of the many small ones a line-buffered stdout makes (which
/// shows as tearing over SSH).
pub(super) const OUTPUT_BUFFER: usize = 256 * 1024;

pub(super) type Tty = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

pub(super) static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(super) fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

pub(super) fn restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            EndSynchronizedUpdate,
            DisableMouseCapture,
            DisableFocusChange,
            LeaveAlternateScreen,
            DisableBracketedPaste
        );
    }
}

pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter(mouse: bool) -> Result<Self> {
        install_panic_hook();
        enable_raw_mode().context("cannot enable raw mode")?;
        let mut out = std::io::stdout();
        // Focus reports let the UI repaint after the terminal was covered
        // or a multiplexer reattached; terminals without them ignore it.
        let entered = if mouse {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange,
                EnableMouseCapture
            )
        } else {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange
            )
        };
        if let Err(error) = entered {
            let _ = disable_raw_mode();
            return Err(error).context("cannot enter the alternate screen");
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}
