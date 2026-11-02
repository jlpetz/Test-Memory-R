//! The CLI's questions (TODO 29), over any reader and writer so a test can script the answers: the large-page restart, the right granted
//! first when TMR could grant it (`Frontend::large_page_restart`), and a calibration's save (the `confirm` that `tmr::commands::run`
//! takes). The CLI asks on the console.

use std::io::{self, BufRead, Write};

use tmr::frontend::{LargePageQuestion, RestartChoice};

/// Large pages need a restart, and the right they need granted first when `question` offers the grant: what that means (which account
/// gets the right, that it keeps it until it is removed, and that it takes a restart), the choice, A or B, and what the answer does. A
/// exits, after the grant when it is offered; B continues on 4 KiB pages and grants nothing; anything else, an empty answer, the input's
/// end or an input that can't be read do as B does, saying the choice was invalid. The grant's outcome is the session's to show
/// (`Setup::LargePageGrant`).
pub fn large_page_restart(question: &LargePageQuestion, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<RestartChoice> {
    let grant = match question {
        LargePageQuestion::Restart => {
            writeln!(out, "⚠️  LARGE PAGE SETUP REQUIRES RESTART")?;
            writeln!(out, "=====================================")?;
            writeln!(out, "The Large Page privilege has been configured but requires a restart to take effect.")?;
            None
        }
        LargePageQuestion::Grant { account } => {
            writeln!(out, "⚠️  LARGE PAGES NEED THE 'LOCK PAGES IN MEMORY' RIGHT")?;
            writeln!(out, "====================================================")?;
            writeln!(out, "TMR runs as {account}, which doesn't have it (SeLockMemoryPrivilege). As Administrator, TMR can grant it:")?;
            writeln!(out, "{account} then keeps it until it is removed (gpedit.msc: User Rights Assignment), and it takes effect")?;
            writeln!(out, "only after Windows restarts.")?;
            Some(account)
        }
    };
    // What B and an invalid answer add when the grant is offered
    let (this_time, nothing) = if grant.is_some() { (" this time", ", granting nothing") } else { ("", "") };
    writeln!(out)?;
    writeln!(out, "Choose your preferred action:")?;
    match grant {
        None => writeln!(out, "  A) Exit TMR and restart system (RECOMMENDED for optimal performance)")?,
        Some(account) => writeln!(out, "  A) Grant it to {account}, exit TMR and restart Windows (RECOMMENDED for optimal performance)")?,
    }
    writeln!(out, "  B) Continue with standard 4KB pages{this_time}{nothing} (~5-10% slower, so less memory stress)")?;
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
            if let Some(account) = grant {
                writeln!(out, "🔧 Granting it to {account}...")?;
                return Ok(RestartChoice::Exit);
            }
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
            writeln!(out, "⚠️  Continuing with standard 4KB pages (minpage=regular for this run){nothing}.")?;
            writeln!(out, "   Performance impact: ~5-10% slower than large pages")?;
            writeln!(out, "   Every test still runs and every pattern is still checked, but slower")?;
            writeln!(out, "   memory traffic is less stress, so marginal errors that only appear at")?;
            writeln!(out, "   full bandwidth may go undetected")?;
            writeln!(out)?;
            if grant.is_some() {
                writeln!(out, "💡 TIP: TMR asks again at its next run as Administrator (tmr.exe --setup-large-pages grants it at once)")?;
            } else {
                writeln!(out, "💡 TIP: Restart TMR after rebooting for optimal performance")?;
            }
            writeln!(out)?;
            Ok(RestartChoice::Continue4K)
        }
        _ => {
            writeln!(out)?;
            writeln!(out, "❌ Invalid choice. Defaulting to continue with 4KB pages (minpage=regular for this run){nothing}.")?;
            writeln!(out)?;
            Ok(RestartChoice::Continue4K)
        }
    }
}

/// Asks `question` [Y/n]: yes for Enter, y or yes; no for anything else. When no answer can come (the input ended, or can't be read: a run
/// with no console to answer from, over WinRM), the default, yes, as the [Y/n] says: the one question is a calibration's save, whose
/// command was the request to calibrate. `--no-save` is how such a run keeps what is stored.
pub fn confirm(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> bool {
    let _ = write!(out, "\n{question} [Y/n]: ");
    let _ = out.flush();
    let mut answer = String::new();
    match input.read_line(&mut answer) {
        Ok(0) | Err(_) => true,
        Ok(_) => {
            let answer = answer.trim().to_lowercase();
            answer.is_empty() || answer == "y" || answer == "yes"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart's question and each answer's text, after a `## case` line: A exits; B, an empty answer, the input's end and anything
    /// else continue on 4 KiB pages (TODO 29). The grant's question names the account; its A is a yes to the grant (the session's to
    /// make), its B and no answer grant nothing.
    #[test]
    fn the_restart_question_takes_a_or_b() {
        let restart = LargePageQuestion::Restart;
        let grant = LargePageQuestion::Grant { account: r"PC\owner".to_string() };
        let mut text = String::new();
        for (case, question, input, choice) in [
            ("A", &restart, &b"a\r\n"[..], RestartChoice::Exit),
            ("B", &restart, b"B\n", RestartChoice::Continue4K),
            ("an empty answer", &restart, b"\n", RestartChoice::Continue4K),
            ("the input's end", &restart, b"", RestartChoice::Continue4K),
            ("junk", &restart, b"maybe\n", RestartChoice::Continue4K),
            ("an input that isn't text", &restart, b"\xff\xfe\n", RestartChoice::Continue4K),
            ("the grant, A", &grant, b"A\n", RestartChoice::Exit),
            ("the grant, B", &grant, b"b\r\n", RestartChoice::Continue4K),
            ("the grant, the input's end", &grant, b"", RestartChoice::Continue4K),
        ] {
            let mut out = Vec::new();
            let got = large_page_restart(question, &mut &*input, &mut out).unwrap();
            assert_eq!(got, choice, "{case}");
            text += &format!("## {case}\n{}\n", String::from_utf8(out).unwrap());
        }
        crate::golden::check("large_page_restart", &text);
    }

    /// A calibration's save: Enter, y and yes save, and so does no answer at all (the input's end: a run with no console to answer from; or
    /// an input that can't be read), as the [Y/n] default says; n and anything else keep what is stored.
    #[test]
    fn a_calibration_is_saved_unless_the_user_says_no() {
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
        assert!(ask(b""), "the input's end");
        assert!(ask(b"\xff\xfe\n"), "an input that isn't text");
    }
}
