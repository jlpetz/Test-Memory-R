//! The CLI's questions (TODO 29), over any reader and writer so a test can script the answers: the
//! large-page restart (`Frontend::large_page_restart`) and a calibration's save (the `confirm`
//! that `tmr::commands::run` takes). The CLI asks on the console.

use std::io::{self, BufRead, Write};

use tmr::frontend::RestartChoice;

/// Large pages need a restart: what that means, the choice, A or B, and what the answer does. A
/// that exits, B that continues on 4 KiB pages; anything else, an empty answer, the input's end or
/// an input that can't be read continue on them too, saying the choice was invalid.
pub fn large_page_restart(input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<RestartChoice> {
    writeln!(out, "⚠️  LARGE PAGE SETUP REQUIRES RESTART")?;
    writeln!(out, "=====================================")?;
    writeln!(out, "The Large Page privilege has been configured but requires a restart to take effect.")?;
    writeln!(out)?;
    writeln!(out, "Choose your preferred action:")?;
    writeln!(out, "  A) Exit TMR and restart system (RECOMMENDED for optimal performance)")?;
    writeln!(out, "  B) Continue with standard 4KB pages (~5-10% slower, so less memory stress)")?;
    writeln!(out)?;
    write!(out, "Enter your choice (A/B): ")?;
    out.flush()?;

    let mut input_line = String::new();
    // An input that can't be read is no answer
    if input.read_line(&mut input_line).is_err() {
        input_line.clear();
    }
    let choice = input_line.trim().to_uppercase();

    match choice.as_str() {
        "A" => {
            writeln!(out)?;
            writeln!(out, "✅ Restart recommended for optimal performance.")?;
            writeln!(out, "After restart, TMR will automatically use large pages.")?;
            writeln!(out)?;
            writeln!(out, "To restart:")?;
            writeln!(out, "  • Windows: shutdown /r /t 0")?;
            writeln!(out, "  • Or use Start Menu → Power → Restart")?;
            writeln!(out)?;
            writeln!(out, "TMR will exit now.")?;
            Ok(RestartChoice::Exit)
        }
        "B" => {
            writeln!(out)?;
            writeln!(out, "⚠️  Continuing with standard 4KB pages (minpage=regular for this run).")?;
            writeln!(out, "   Performance impact: ~5-10% slower than large pages")?;
            writeln!(out, "   Every test still runs and every pattern is still checked, but slower")?;
            writeln!(out, "   memory traffic is less stress, so marginal errors that only appear at")?;
            writeln!(out, "   full bandwidth may go undetected")?;
            writeln!(out)?;
            writeln!(out, "💡 TIP: Restart TMR after rebooting for optimal performance")?;
            writeln!(out)?;
            Ok(RestartChoice::Continue4K)
        }
        _ => {
            writeln!(out)?;
            writeln!(out, "❌ Invalid choice. Defaulting to continue with 4KB pages (minpage=regular for this run).")?;
            writeln!(out)?;
            Ok(RestartChoice::Continue4K)
        }
    }
}

/// Asks `question` [Y/n]: yes for Enter, y or yes; no for anything else, and when no answer comes
/// (the input ended, or can't be read), so nothing is saved that no one agreed to.
pub fn confirm(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> bool {
    let _ = write!(out, "\n{question} [Y/n]: ");
    let _ = out.flush();
    let mut answer = String::new();
    match input.read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let answer = answer.trim().to_lowercase();
            answer.is_empty() || answer == "y" || answer == "yes"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart's question and each answer's text, after a `## case` line: A exits; B, an empty
    /// answer, the input's end and anything else continue on 4 KiB pages (TODO 29).
    #[test]
    fn the_restart_question_takes_a_or_b() {
        let mut text = String::new();
        for (case, input, choice) in [
            ("A", &b"a\r\n"[..], RestartChoice::Exit),
            ("B", b"B\n", RestartChoice::Continue4K),
            ("an empty answer", b"\n", RestartChoice::Continue4K),
            ("the input's end", b"", RestartChoice::Continue4K),
            ("junk", b"maybe\n", RestartChoice::Continue4K),
            ("an input that isn't text", b"\xff\xfe\n", RestartChoice::Continue4K),
        ] {
            let mut out = Vec::new();
            let got = large_page_restart(&mut &*input, &mut out).unwrap();
            assert_eq!(got, choice, "{case}");
            text += &format!("## {case}\n{}\n", String::from_utf8(out).unwrap());
        }
        crate::golden::check("large_page_restart", &text);
    }

    /// A calibration's save: Enter, y and yes save; n, anything else and no answer at all (the
    /// input's end: a run with no console to answer from; or an input that can't be read) keep
    /// what is stored. cli.rs saved on both of those.
    #[test]
    fn a_calibration_is_saved_only_when_the_user_says_so() {
        let ask = |input: &[u8]| {
            let mut out = Vec::new();
            let yes = confirm(&mut &*input, &mut out, "Save new standard calibration?");
            assert_eq!(String::from_utf8(out).unwrap(), "\nSave new standard calibration? [Y/n]: ");
            yes
        };
        assert!(ask(b"\n"), "Enter");
        assert!(ask(b"\r\n"), "Enter on a console");
        assert!(ask(b"y\n") && ask(b"Yes\n"));
        assert!(!ask(b"n\n"), "n");
        assert!(!ask(b"later\n"));
        assert!(!ask(b""), "the input's end");
        assert!(!ask(b"\xff\xfe\n"), "an input that isn't text");
    }
}
