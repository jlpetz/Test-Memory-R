//! The CLI's questions (TODO 29), over any reader and writer so a test can script the answers: the large-page question (a new sign-in,
//! whom to grant the right to, or 4 KiB pages or quit: `Frontend::ask_large_pages`, TODO 113), and a calibration's save (the `confirm`
//! that `tmr::commands::run` takes). The CLI asks on the console.

use std::io::{self, BufRead, Write};

use tmr::frontend::{BROAD_CAUTION, BROAD_MARK, BROAD_RISK, Candidate, LargePageAnswer, LargePageQuestion};

/// One answer: a line of `input`, trimmed and in capitals, after `flush` has dropped what was typed ahead (`console::flush_input`); empty
/// for no answer, the input's end, or an input that can't be read
fn answer(input: &mut dyn BufRead, flush: &dyn Fn()) -> String {
    flush();
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        line.clear();
    }
    line.trim().to_uppercase()
}

/// A question's heading, underlined to its width (⚠️ and its spaces count as three, as the console has always drawn them)
fn heading(out: &mut dyn Write, text: &str) -> io::Result<()> {
    writeln!(out, "⚠️  {text}")?;
    writeln!(out, "{}", "=".repeat(text.chars().count() + 3))
}

/// What 4 KiB pages mean, after B
const FOUR_KIB_PAGES: [&str; 4] = [
    "   Performance impact: ~5-10% slower than large pages",
    "   Every test still runs and every pattern is still checked, but slower",
    "   memory traffic is less stress, so marginal errors that only appear at",
    "   full bandwidth may go undetected",
];

/// B: this run on 4 KiB pages, what that means, and `tip`; `nothing` says nothing was granted when a grant was offered
fn continuing(out: &mut dyn Write, nothing: &str, tip: &str) -> io::Result<LargePageAnswer> {
    writeln!(out)?;
    writeln!(out, "⚠️  Continuing with standard 4KB pages (minpage=regular maxpage=regular for this run){nothing}.")?;
    for line in FOUR_KIB_PAGES {
        writeln!(out, "{line}")?;
    }
    writeln!(out)?;
    writeln!(out, "💡 TIP: {tip}")?;
    writeln!(out)?;
    Ok(LargePageAnswer::Regular)
}

/// No answer, or one that isn't a choice: TMR quits (TODO 113, decision 5a), granting nothing when a grant was offered (`granting`)
fn invalid(out: &mut dyn Write, granting: bool) -> io::Result<LargePageAnswer> {
    writeln!(out)?;
    let nothing = if granting { ", granting nothing," } else { "" };
    writeln!(out, "❌ Invalid choice. TMR will exit{nothing} rather than run slower unasked.")?;
    writeln!(out, "   minpage=regular maxpage=regular runs on standard 4KB pages without asking.")?;
    writeln!(out)?;
    Ok(LargePageAnswer::Quit)
}

/// How to sign out, or restart, and that TMR exits
fn sign_out_steps(out: &mut dyn Write) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "To sign out: Start Menu → your account → Sign out")?;
    writeln!(out, "To restart:")?;
    writeln!(out, "  • Windows: shutdown /r /t 0")?;
    writeln!(out, "  • Or use Start Menu → Power → Restart")?;
    writeln!(out)?;
    writeln!(out, "TMR will exit now.")
}

/// The UAC and domain lines under a heading's paragraph, when they apply
fn uac_and_domain(out: &mut dyn Write, uac: bool, domain: bool) -> io::Result<()> {
    if uac {
        writeln!(out, "After the next sign-in TMR has it in every run, run as administrator or not.")?;
    }
    if domain {
        writeln!(out, "This PC is in a domain: a domain Group Policy that sets this right replaces a local grant.")?;
    }
    Ok(())
}

/// The large-page question on the console (`Frontend::ask_large_pages`): what it means, the choice, and what the answer does, `flush`
/// dropping what was typed ahead before each read. The sign-in's A exits to sign out; the grant's A grants it to the account TMR runs as,
/// a number to that group, a broad one only after a second question whose default, No, goes back to the choice, and its Q quits; the
/// question that needs an administrator's A quits, to start TMR as one. B takes 4 KiB pages this time. Anything else (an empty answer,
/// the input's end, an input that can't be read) quits, saying the choice was invalid, so nothing is granted and nothing runs on 4 KiB
/// pages unasked (TODO 113, decision 5a). A grant's outcome is the session's to show (`Setup::LargePageGrant`).
pub fn large_pages(
    question: &LargePageQuestion, input: &mut dyn BufRead, out: &mut dyn Write, flush: &dyn Fn(),
) -> io::Result<LargePageAnswer> {
    match question {
        LargePageQuestion::SignIn { account, via, uac, domain } => sign_in(account, via.as_deref(), *uac, *domain, input, out, flush),
        LargePageQuestion::Grant { candidates, uac, domain } => grant(candidates, *uac, *domain, input, out, flush),
        LargePageQuestion::NeedsAdministrator { account, uac } => needs_administrator(account, *uac, input, out, flush),
    }
}

/// The policy gives the right to `account`, or `via` a group of it, and this sign-in began before it did
fn sign_in(
    account: &str, via: Option<&str>, uac: bool, domain: bool, input: &mut dyn BufRead, out: &mut dyn Write, flush: &dyn Fn(),
) -> io::Result<LargePageAnswer> {
    heading(out, "LARGE PAGES NEED A NEW SIGN-IN")?;
    let holder = match via {
        Some(group) => format!("{group}, a group {account} is in"),
        None => format!("{account}, the account TMR runs as"),
    };
    writeln!(out, "The security policy gives 'Lock pages in memory' (SeLockMemoryPrivilege) to {holder},")?;
    writeln!(out, "but this sign-in began before it did: a program gets the rights its account had at sign-in, so signing")?;
    writeln!(out, "out and back in brings it (Windows needn't restart).")?;
    uac_and_domain(out, uac, domain)?;
    writeln!(out)?;
    writeln!(out, "Choose your preferred action:")?;
    writeln!(out, "  A) Exit TMR, then sign out and back in, or restart Windows (RECOMMENDED for optimal performance)")?;
    writeln!(out, "  B) Continue with standard 4KB pages this time (~5-10% slower, so less memory stress)")?;
    writeln!(out)?;
    write!(out, "Enter your choice (A/B): ")?;
    out.flush()?;
    match answer(input, flush).as_str() {
        "A" => {
            writeln!(out)?;
            writeln!(out, "✅ Sign out and back in (or restart Windows), then run TMR again: it then uses large pages.")?;
            sign_out_steps(out)?;
            Ok(LargePageAnswer::SignOut)
        }
        "B" => continuing(out, "", "Sign out and back in, then run TMR again, for optimal performance"),
        _ => invalid(out, false),
    }
}

/// Neither the account nor a group of it has the right, and TMR can grant it: to whom, `candidates[0]` the account
fn grant(
    candidates: &[Candidate], uac: bool, domain: bool, input: &mut dyn BufRead, out: &mut dyn Write, flush: &dyn Fn(),
) -> io::Result<LargePageAnswer> {
    let Some((you, groups)) = candidates.split_first() else {
        return Ok(LargePageAnswer::Quit);
    };
    heading(out, "LARGE PAGES NEED THE 'LOCK PAGES IN MEMORY' RIGHT")?;
    writeln!(out, "TMR runs as {}, and neither it nor a group it is in has this right (SeLockMemoryPrivilege).", you.name)?;
    writeln!(out, "As Administrator, TMR can grant it: whoever gets it keeps it until it is removed (tmr.exe")?;
    writeln!(out, "--remove-large-pages), and it takes effect at their next sign-in.")?;
    uac_and_domain(out, uac, domain)?;
    writeln!(out)?;
    writeln!(out, "Choose your preferred action:")?;
    writeln!(out, "  A) Grant it to {}, exit TMR, then sign out and back in (RECOMMENDED for optimal performance)", you.name)?;
    writeln!(out, "  B) Continue with standard 4KB pages this time, granting nothing (~5-10% slower, so less memory stress)")?;
    writeln!(out, "  Q) Quit TMR, granting nothing")?;
    if !groups.is_empty() {
        writeln!(out, "Or grant it to a group {} is in, then exit TMR and sign out and back in:", you.name)?;
        for (number, group) in (1..).zip(groups) {
            match group.reach.is_broad() {
                true => writeln!(out, "  {number}) {}  {BROAD_MARK}", group.name)?,
                false => writeln!(out, "  {number}) {}", group.name)?,
            }
        }
        // What the mark means, once for every row it marks (the owner, 2026-11-04)
        if groups.iter().any(|group| group.reach.is_broad()) {
            writeln!(out, "  {BROAD_MARK}  {BROAD_CAUTION}: most or all accounts are in it, and {BROAD_RISK}.")?;
        }
    }
    let granting = |out: &mut dyn Write, i: usize| -> io::Result<LargePageAnswer> {
        writeln!(out)?;
        writeln!(out, "🔧 Granting it to {}...", candidates[i].name)?;
        Ok(LargePageAnswer::Grant(i))
    };
    // Asked again after a broad pick's No, which goes back to the choice: the input's end then quits here
    loop {
        writeln!(out)?;
        match groups.len() {
            0 => write!(out, "Enter your choice (A, B or Q): ")?,
            1 => write!(out, "Enter your choice (A, B, Q or 1): ")?,
            n => write!(out, "Enter your choice (A, B, Q or 1-{n}): ")?,
        }
        out.flush()?;
        let picked = match answer(input, flush).as_str() {
            "A" => return granting(out, 0),
            "B" => {
                let tip = "TMR asks again at its next run as Administrator (tmr.exe --setup-large-pages grants it at once)";
                return continuing(out, ", granting nothing", tip);
            }
            "Q" => {
                writeln!(out)?;
                writeln!(out, "✅ Nothing granted. TMR will exit now.")?;
                writeln!(out)?;
                return Ok(LargePageAnswer::Quit);
            }
            number => match number.parse::<usize>() {
                Ok(i) if (1..=groups.len()).contains(&i) => i,
                _ => return invalid(out, true),
            },
        };
        // A broad pick asks again, No the default
        let Some(warning) = candidates[picked].reach.warning() else {
            return granting(out, picked);
        };
        let name = &candidates[picked].name;
        writeln!(out)?;
        writeln!(out, "{BROAD_MARK}  {name} is BROAD: {warning}.")?;
        write!(out, "Grant it to {name} anyway? [y/N]: ")?;
        out.flush()?;
        if matches!(answer(input, flush).as_str(), "Y" | "YES") {
            return granting(out, picked);
        }
        writeln!(out)?;
        writeln!(out, "Nothing granted to {name}: choose again.")?;
    }
}

/// TMR can't grant the right: 4 KiB pages this time, or quit, to start it as administrator
fn needs_administrator(
    account: &str, uac: bool, input: &mut dyn BufRead, out: &mut dyn Write, flush: &dyn Fn(),
) -> io::Result<LargePageAnswer> {
    heading(out, "LARGE PAGES NEED TMR RUN AS ADMINISTRATOR")?;
    writeln!(out, "TMR runs as {account} without the right large pages need, 'Lock pages in memory' (SeLockMemoryPrivilege).")?;
    writeln!(out, "Only TMR run as administrator can set them up: Run as administrator (right-click TMR), or")?;
    writeln!(out, "tmr.exe --setup-large-pages from a console run as administrator.")?;
    if uac {
        writeln!(
            out,
            "You sign in as an administrator: run TMR as administrator once to grant it; after the next sign-in it has it in every run."
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Choose your preferred action:")?;
    writeln!(out, "  A) Exit TMR, to run it as administrator (RECOMMENDED for optimal performance)")?;
    writeln!(out, "  B) Continue with standard 4KB pages this time (~5-10% slower, so less memory stress)")?;
    writeln!(out)?;
    write!(out, "Enter your choice (A/B): ")?;
    out.flush()?;
    match answer(input, flush).as_str() {
        "A" => {
            writeln!(out)?;
            writeln!(out, "✅ Run TMR as administrator to set up large pages. TMR will exit now.")?;
            writeln!(out)?;
            Ok(LargePageAnswer::Quit)
        }
        "B" => continuing(out, "", "Run TMR as administrator once to set up large pages, for optimal performance"),
        _ => invalid(out, false),
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
    use std::cell::Cell;

    use tmr::frontend::{Reach, Sid};

    use super::*;

    fn candidate(name: &str, reach: Reach, sid: &str) -> Candidate {
        Candidate::new(name, reach, sid.parse::<Sid>().unwrap())
    }

    /// TODO 113: each question and each answer's text, after a `## case` line. The sign-in's A signs out, through the account or a
    /// group; the grant's A grants to the account and a number to its group, a broad one only after a second yes, whose No goes back to
    /// the choice; the grant's Q and the administrator's A quit. B takes 4 KiB pages; an empty answer, the input's end and anything else
    /// quit (decision 5a), granting nothing. Type-ahead is flushed before every read.
    #[test]
    fn the_large_page_question_takes_its_answers() {
        let you = candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001");
        let groups = vec![
            you.clone(),
            candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"),
            candidate(r"BUILTIN\Users", Reach::EveryLocalAccount, "S-1-5-32-545"),
            candidate("Everyone", Reach::Automatic, "S-1-1-0"),
        ];
        let sign_in = |via: Option<&str>, uac, domain| LargePageQuestion::SignIn {
            account: r"PC\owner".to_string(),
            via: via.map(str::to_string),
            uac,
            domain,
        };
        let grant = |candidates: Vec<Candidate>, uac, domain| LargePageQuestion::Grant { candidates, uac, domain };
        let needs = |uac| LargePageQuestion::NeedsAdministrator { account: r"PC\owner".to_string(), uac };
        use LargePageAnswer::{Grant, Quit, Regular, SignOut};
        let mut text = String::new();
        #[rustfmt::skip]
        let cases = [
            ("the sign-in, A", sign_in(None, false, false), &b"a\r\n"[..], SignOut, 1),
            ("the sign-in, B", sign_in(None, false, false), b"B\n", Regular, 1),
            ("the sign-in, an empty answer", sign_in(None, false, false), b"\n", Quit, 1),
            ("the sign-in, the input's end", sign_in(None, false, false), b"", Quit, 1),
            ("the sign-in, junk", sign_in(None, false, false), b"maybe\n", Quit, 1),
            ("the sign-in, an input that isn't text", sign_in(None, false, false), b"\xff\xfe\n", Quit, 1),
            ("the sign-in through a group, under UAC on a domain PC, A", sign_in(Some(r"BUILTIN\Administrators"), true, true), b"A\n",
                SignOut, 1),
            ("the grant, A", grant(groups.clone(), false, false), b"A\n", Grant(0), 1),
            ("the grant, 1", grant(groups.clone(), false, false), b"1\n", Grant(1), 1),
            ("the grant, a broad pick, yes", grant(groups.clone(), false, false), b"2\ny\n", Grant(2), 2),
            ("the grant, a broad pick, no, then A", grant(groups.clone(), false, false), b"2\nn\nA\n", Grant(0), 3),
            ("the grant, a broad pick, no answer, then none", grant(groups.clone(), false, false), b"3\n\n\n", Quit, 3),
            ("the grant, a broad pick, the input's end", grant(groups.clone(), false, false), b"3\n", Quit, 3),
            ("the grant, B", grant(groups.clone(), false, false), b"b\r\n", Regular, 1),
            ("the grant, Q", grant(groups.clone(), false, false), b"q\n", Quit, 1),
            ("the grant, an empty answer", grant(groups.clone(), false, false), b"\n", Quit, 1),
            ("the grant, the input's end", grant(groups.clone(), false, false), b"", Quit, 1),
            ("the grant, 0", grant(groups.clone(), false, false), b"0\n", Quit, 1),
            ("the grant, out of range", grant(groups.clone(), false, false), b"4\n", Quit, 1),
            ("the grant, under UAC on a domain PC", grant(groups[..2].to_vec(), true, true), b"1\n", Grant(1), 1),
            ("the grant, no groups", grant(vec![you.clone()], false, false), b"1\n", Quit, 1),
            ("the administrator's, A", needs(false), b"A\n", Quit, 1),
            ("the administrator's under UAC, B", needs(true), b"b\n", Regular, 1),
            ("the administrator's, an empty answer", needs(false), b"\n", Quit, 1),
            ("the administrator's, the input's end", needs(false), b"", Quit, 1),
        ];
        for (case, question, input, expected, reads) in cases {
            let flushed = Cell::new(0);
            let mut out = Vec::new();
            let got = large_pages(&question, &mut &*input, &mut out, &|| flushed.set(flushed.get() + 1)).unwrap();
            assert_eq!(got, expected, "{case}");
            assert_eq!(flushed.get(), reads, "{case}: type-ahead flushed before each read");
            text += &format!("## {case}\n{}\n", String::from_utf8(out).unwrap());
        }
        crate::golden::check("large_pages", &text);
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
