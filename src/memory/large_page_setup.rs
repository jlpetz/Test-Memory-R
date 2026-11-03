//! `--setup-large-pages [User:<name> | Group:<name>]` and `--remove-large-pages [User:<name> | Group:<name>]` (TODO 113): the 'Lock
//! pages in memory' right granted to, or removed from, the account TMR runs as, or the user or group named, in this PC's security policy
//! through LSA, by SID. The command is the request: neither asks, and a broad target is warned of and taken.
//!
//! A name needs its type, as a user and a group can share one (this PC's group `witches` and the domain's user `witches`): `User:` or
//! `Group:`, then `DOMAIN\name`, `.\name` (this PC's own) or a bare name, looked up on this PC and in the domain it is joined to, the
//! prefix choosing among what that finds (`choose`). People don't talk in SIDs, so Windows' built-in and automatic groups are taken by
//! their English names on any Windows (`WELL_KNOWN`), and every message shows Windows' own name ("VORDEFINIERT\Administratoren"). A SID
//! (`Group:S-1-5-32-544`) is the last resort, for scripts and for a group Windows can't name.
//!
//! The flow runs over `Machine`, so its every outcome is a golden with a stand-in (`large_page_setup.txt`) and no test touches the policy.
//! The real one (`ThisMachine`) reads the token and the policy, and changes the policy only in `Machine::change`.

use std::collections::HashMap;
use std::io::{self, Write};

use windows::Win32::Security::{
    SID_NAME_USE, SidTypeAlias, SidTypeComputer, SidTypeDeletedAccount, SidTypeDomain, SidTypeGroup, SidTypeLabel, SidTypeLogonSession,
    SidTypeUnknown, SidTypeUser, SidTypeWellKnownGroup,
};

use super::lsa::{Access, Named, Policy, Sid, Token, in_a_domain};
use super::privileges::{
    Candidate, Holder, Reach, administrator, breadth, diagnose_large_pages, held_by, in_token, is_service_account, never_offered,
    uac_elevated, who,
};
use crate::runner::RunStatus;

/// Which command: the grant (`--setup-large-pages`) or the removal (`--remove-large-pages`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Grant,
    Remove,
}

impl Change {
    fn flag(self) -> &'static str {
        match self {
            Change::Grant => "--setup-large-pages",
            Change::Remove => "--remove-large-pages",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Change::Grant => "🔧 TMR Large Page Setup Tool\n=============================\n",
            Change::Remove => "🔧 TMR Large Page Removal Tool\n===============================\n",
        }
    }
}

/// What a target's prefix says it is
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Group,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::User => "User",
            Kind::Group => "Group",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Group => "group",
        }
    }
}

/// A target as typed: its kind, and its name, trimmed, inner spaces kept
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) kind: Kind,
    pub(crate) name: String,
}

/// Why a target can't be read, before anything is looked up
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetError {
    /// No `User:` or `Group:`: the name as typed
    NoKind(String),
    /// A prefix that is neither: the target as typed
    UnknownKind(String),
    /// A prefix and no name
    NoName(Kind),
}

impl TargetError {
    fn text(&self) -> String {
        match self {
            TargetError::NoKind(name) => {
                format!("❌ Say whether {name} is a user or a group: User:{name} or Group:{name} (a user and a group can share a name)")
            }
            TargetError::UnknownKind(target) => format!("❌ {target}: the type is User: or Group:"),
            TargetError::NoName(Kind::User) => r"❌ User: names no one: User:<name>, e.g. User:.\alex".to_string(),
            TargetError::NoName(Kind::Group) => "❌ Group: names no one: Group:<name>, e.g. Group:Administrators".to_string(),
        }
    }
}

/// `User:<name>` or `Group:<name>`, the prefix in any case, split at the first ':' (no account name has one; a UPN, `a@b.com`, and a DNS
/// domain's `corp.example.com\x` still read)
pub(crate) fn parse_target(text: &str) -> Result<Target, TargetError> {
    let Some((prefix, name)) = text.split_once(':') else {
        return Err(TargetError::NoKind(text.trim().to_string()));
    };
    let kind = match prefix {
        _ if prefix.eq_ignore_ascii_case("user") => Kind::User,
        _ if prefix.eq_ignore_ascii_case("group") => Kind::Group,
        _ => return Err(TargetError::UnknownKind(text.to_string())),
    };
    match name.trim() {
        "" => Err(TargetError::NoName(kind)),
        name => Ok(Target { kind, name: name.to_string() }),
    }
}

/// Windows' built-in and automatic groups and its service accounts by their English names, which are taken on any Windows as their fixed
/// SIDs (German Windows names BUILTIN\Administrators "VORDEFINIERT\Administratoren"): each by its full name or the part after the `\`
#[rustfmt::skip]
pub(crate) const WELL_KNOWN: &[(&str, &str)] = &[
    (r"BUILTIN\Administrators", "S-1-5-32-544"),
    (r"BUILTIN\Users", "S-1-5-32-545"),
    (r"BUILTIN\Guests", "S-1-5-32-546"),
    (r"BUILTIN\Power Users", "S-1-5-32-547"),
    (r"BUILTIN\Account Operators", "S-1-5-32-548"),
    (r"BUILTIN\Server Operators", "S-1-5-32-549"),
    (r"BUILTIN\Print Operators", "S-1-5-32-550"),
    (r"BUILTIN\Backup Operators", "S-1-5-32-551"),
    (r"BUILTIN\Replicator", "S-1-5-32-552"),
    (r"BUILTIN\Remote Desktop Users", "S-1-5-32-555"),
    (r"BUILTIN\Network Configuration Operators", "S-1-5-32-556"),
    (r"BUILTIN\Performance Monitor Users", "S-1-5-32-558"),
    (r"BUILTIN\Performance Log Users", "S-1-5-32-559"),
    (r"BUILTIN\Distributed COM Users", "S-1-5-32-562"),
    (r"BUILTIN\IIS_IUSRS", "S-1-5-32-568"),
    (r"BUILTIN\Cryptographic Operators", "S-1-5-32-569"),
    (r"BUILTIN\Event Log Readers", "S-1-5-32-573"),
    (r"BUILTIN\Hyper-V Administrators", "S-1-5-32-578"),
    (r"BUILTIN\Access Control Assistance Operators", "S-1-5-32-579"),
    (r"BUILTIN\Remote Management Users", "S-1-5-32-580"),
    ("Everyone", "S-1-1-0"),
    ("LOCAL", "S-1-2-0"),
    ("CONSOLE LOGON", "S-1-2-1"),
    (r"NT AUTHORITY\DIALUP", "S-1-5-1"),
    (r"NT AUTHORITY\NETWORK", "S-1-5-2"),
    (r"NT AUTHORITY\BATCH", "S-1-5-3"),
    (r"NT AUTHORITY\INTERACTIVE", "S-1-5-4"),
    (r"NT AUTHORITY\SERVICE", "S-1-5-6"),
    (r"NT AUTHORITY\ANONYMOUS LOGON", "S-1-5-7"),
    (r"NT AUTHORITY\PROXY", "S-1-5-8"),
    (r"NT AUTHORITY\ENTERPRISE DOMAIN CONTROLLERS", "S-1-5-9"),
    (r"NT AUTHORITY\Authenticated Users", "S-1-5-11"),
    (r"NT AUTHORITY\RESTRICTED", "S-1-5-12"),
    (r"NT AUTHORITY\TERMINAL SERVER USER", "S-1-5-13"),
    (r"NT AUTHORITY\REMOTE INTERACTIVE LOGON", "S-1-5-14"),
    (r"NT AUTHORITY\This Organization", "S-1-5-15"),
    (r"NT AUTHORITY\SYSTEM", "S-1-5-18"),
    (r"NT AUTHORITY\LOCAL SERVICE", "S-1-5-19"),
    (r"NT AUTHORITY\NETWORK SERVICE", "S-1-5-20"),
    (r"NT AUTHORITY\Local account", "S-1-5-113"),
    (r"NT AUTHORITY\Local account and member of Administrators group", "S-1-5-114"),
    (r"NT AUTHORITY\NTLM Authentication", "S-1-5-64-10"),
    (r"NT AUTHORITY\SChannel Authentication", "S-1-5-64-14"),
    (r"NT AUTHORITY\Digest Authentication", "S-1-5-64-21"),
    (r"NT AUTHORITY\This Organization Certificate", "S-1-5-65-1"),
    ("Authentication authority asserted identity", "S-1-18-1"),
    ("Service asserted identity", "S-1-18-2"),
];

/// The SID `name` gives by its English name (`WELL_KNOWN`), `.\` before it allowed: its full name, or the part after its `\`
pub(crate) fn well_known(name: &str) -> Option<Sid> {
    let name = name.strip_prefix(r".\").unwrap_or(name);
    let named = |full: &&(&str, &str)| {
        full.0.eq_ignore_ascii_case(name) || full.0.rsplit('\\').next().is_some_and(|bare| bare.eq_ignore_ascii_case(name))
    };
    WELL_KNOWN.iter().find(named).and_then(|(_, sid)| sid.parse().ok())
}

/// The names `name` is looked up by, in one call that looks for a name without a domain on this PC alone (`Policy::lookup`): `.\name`
/// as this PC's own (alone, then in its `account_domain`, whose name is this PC's: alone, a name equal to that is the domain); a bare
/// name as that and in the domain it is joined to (`primary`), so a first match can't pick a trusted domain's; `DOMAIN\name` and a UPN
/// as written
pub(crate) fn lookups(name: &str, account_domain: &str, primary: Option<&str>) -> Vec<String> {
    if let Some(local) = name.strip_prefix(r".\") {
        // No account of this PC's has a '\' or a '@' in its name: `.\CORP\x` and `.\a@b.com` would name the domain's
        let mine = !local.is_empty() && !local.contains(['\\', '@']);
        return if mine { vec![local.to_string(), format!(r"{account_domain}\{local}")] } else { Vec::new() };
    }
    if name.contains('\\') || name.contains('@') {
        return vec![name.to_string()];
    }
    let mut names = vec![name.to_string(), format!(r"{account_domain}\{name}")];
    if let Some(domain) = primary.filter(|domain| !domain.eq_ignore_ascii_case(account_domain)) {
        names.push(format!(r"{domain}\{name}"));
    }
    names
}

/// An account or group a lookup found: its SID, its type (`SidTypeUnknown` for a SID Windows can't name) and Windows' name for it
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) sid: Sid,
    pub(crate) kind: SID_NAME_USE,
    pub(crate) name: String,
}

/// Why what was found isn't the target
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Misfit {
    /// It is the other kind: a user, not a group, or a group, not a user
    Is(Kind),
    /// It is no account the right goes to: a domain, a computer, a label...
    Not(&'static str),
}

/// Whether `found` is what a `kind` prefix names: a user for `User:`, a group (Windows' groups, its aliases and its well-known groups) for
/// `Group:`, a service account for either (Windows types them well-known groups: `User:NT SERVICE\MSSQLSERVER` is how its admin would
/// name SQL Server's), and a SID Windows can't name for either. Never a domain, a computer, a label, a logon SID or an app's.
pub(crate) fn fit(kind: Kind, found: &Found) -> Result<(), Misfit> {
    if let Some(what) = never_offered(&found.sid) {
        return Err(Misfit::Not(what));
    }
    if is_service_account(&found.sid) || found.kind == SidTypeUnknown {
        return Ok(());
    }
    let is = if found.kind == SidTypeUser {
        Kind::User
    } else if [SidTypeGroup, SidTypeAlias, SidTypeWellKnownGroup].contains(&found.kind) {
        Kind::Group
    } else {
        let not = [
            (SidTypeDomain, "a domain"),
            (SidTypeComputer, "a computer"),
            (SidTypeLabel, "an integrity label"),
            (SidTypeLogonSession, "a sign-in's own SID, which lasts one sign-in"),
            (SidTypeDeletedAccount, "a deleted account"),
        ];
        return Err(Misfit::Not(not.iter().find(|(kind, _)| *kind == found.kind).map_or("a SID of an unknown kind", |(_, what)| what)));
    };
    if is == kind { Ok(()) } else { Err(Misfit::Is(is)) }
}

/// Why a target named no one to change
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Nothing by that name
    NotFound,
    /// Something by that name, but not of its kind: the first found, one of the other kind before one that is no account
    Misfit(Found, Misfit),
    /// More than one of its kind: their names
    Ambiguous(Vec<String>),
    /// The lookup failed: why
    Lookup(String),
}

/// What `target` names, of what a lookup `found`: the one of its kind (`fit`), each SID counted once
pub(crate) fn choose(target: &Target, found: Vec<Found>) -> Result<Found, Refusal> {
    let mut unique: Vec<Found> = Vec::new();
    for found in found {
        if !unique.iter().any(|seen| seen.sid == found.sid) {
            unique.push(found);
        }
    }
    let (mut fitting, mut misfits) = (Vec::new(), Vec::new());
    for found in unique {
        match fit(target.kind, &found) {
            Ok(()) => fitting.push(found),
            Err(misfit) => misfits.push((found, misfit)),
        }
    }
    match fitting.len() {
        1 => Ok(fitting.remove(0)),
        0 => {
            misfits.sort_by_key(|(_, misfit)| !matches!(misfit, Misfit::Is(_)));
            Err(misfits.into_iter().next().map_or(Refusal::NotFound, |(found, misfit)| Refusal::Misfit(found, misfit)))
        }
        _ => Err(Refusal::Ambiguous(fitting.into_iter().map(|found| found.name).collect())),
    }
}

/// `prefix:name` as it is typed on a command line: quoted when it has a space
fn argument(kind: Kind, name: &str) -> String {
    let argument = format!("{}:{name}", kind.prefix());
    if name.contains(' ') { format!("\"{argument}\"") } else { argument }
}

/// What a refusal says, a line each
fn refusal_text(target: &Target, refusal: &Refusal) -> String {
    let Target { kind, name } = target;
    let qualified = name.contains('\\') || name.contains('@');
    let bare = name.rsplit('\\').next().unwrap_or(name).split('@').next().unwrap_or(name);
    let mut lines = Vec::new();
    match refusal {
        Refusal::NotFound => {
            lines.push(format!("❌ No user or group is named {name}, on this PC or in its domain"));
            if let Some((domain, local)) = name.split_once('\\').filter(|(domain, _)| domain.eq_ignore_ascii_case("local")) {
                lines.push(format!(r"   {domain}\ isn't this PC to Windows (a domain may be named LOCAL): .\{local} names this PC's own"));
            }
            lines.push(format!(
                r"   Give its domain to name another's ({}), or, last, its SID ({})",
                argument(*kind, &format!(r"CORP\{bare}")),
                argument(*kind, "S-1-5-21-...")
            ));
        }
        Refusal::Misfit(found, Misfit::Is(is)) => {
            lines.push(format!(
                "❌ {} is a {}, not a {} ({} names the {})",
                found.name,
                is.noun(),
                kind.noun(),
                argument(*is, name),
                is.noun()
            ));
            if !qualified {
                lines.push(format!(
                    "   A name without its domain is looked up on this PC and in its domain: give the domain to name another, e.g. {}",
                    argument(*kind, &format!(r"CORP\{name}"))
                ));
            }
        }
        Refusal::Misfit(found, Misfit::Not(what)) => {
            lines.push(format!("❌ {} is {what}, not an account or a group that can hold this right", found.name));
            if found.kind == SidTypeDomain && !qualified {
                lines.push(format!(r"   .\{name} names this PC's own {} {name}", kind.noun()));
            }
        }
        Refusal::Ambiguous(names) => {
            let (last, rest) = names.split_last().map_or(("", &[][..]), |(last, rest)| (last.as_str(), rest));
            lines.push(format!("❌ More than one {} is named {name}: {} and {last}", kind.noun(), rest.join(", ")));
            let arguments: Vec<String> = names.iter().map(|found| argument(*kind, found)).collect();
            lines.push(format!("   Name the one you mean: {}", arguments.join(" or ")));
        }
        Refusal::Lookup(e) => lines.push(format!("❌ Couldn't look up {name}: {e}")),
    }
    lines.join("\n")
}

/// Whom the change goes to: its SID, Windows' name for it, the kind it was asked as, and how far it reaches (`breadth`)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Account {
    pub(crate) sid: Sid,
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) reach: Reach,
}

/// Who TMR runs as: its account and groups (the session's candidates, `who`), every SID of its token and Windows' names for them, whether
/// the token has the right, whether it is an administrator under UAC run as administrator (`uac_elevated`), and whether this PC is in a
/// domain
pub(crate) struct Me {
    pub(crate) you: Candidate,
    pub(crate) groups: Vec<Candidate>,
    pub(crate) mine: Vec<Sid>,
    pub(crate) names: HashMap<Sid, Named>,
    pub(crate) in_token: bool,
    pub(crate) uac: bool,
    pub(crate) domain: bool,
}

impl Me {
    fn account(&self) -> Account {
        Account { sid: self.you.sid().clone(), name: self.you.name.clone(), kind: Kind::User, reach: Reach::You }
    }
}

/// What the commands ask of this machine: the real one's (`ThisMachine`) reads its token and its security policy, and changes the policy
/// in `change` alone; a test's stand-in reads and changes nothing
pub(crate) trait Machine {
    /// The large-page check's report (`diagnose_large_pages`), which enables the right in this token when it has it
    fn diagnostic(&mut self) -> String;
    /// Whether TMR runs as administrator (`privileges::administrator`), which changing the policy needs
    fn administrator(&mut self) -> bool;
    /// What `name` may name (`lookups`, `WELL_KNOWN`, and last a SID), each with Windows' name for it
    fn find(&mut self, name: &str) -> Result<Vec<Found>, String>;
    /// Who TMR runs as
    fn me(&mut self) -> Result<Me, String>;
    /// The policy's holders, on a handle opened with the access `change` needs, kept for it
    fn holders(&mut self, change: Change) -> Result<Vec<Sid>, String>;
    /// `change` made to `sid`, on that handle
    fn change(&mut self, change: Change, sid: &Sid) -> Result<(), String>;
    /// Whether large pages work in this process now (`check_large_page_privilege`): the setup's last lines
    fn closing_check(&mut self) -> Result<(), &'static str>;
}

/// `--setup-large-pages` (`Change::Grant`) or `--remove-large-pages` (`Change::Remove`), `args` the command line, the exe's name first,
/// on `machine`. Passed when it was done, or there was nothing to do; NotRun on a refusal, not run as administrator, or a change that
/// failed (exit statuses 0 and 2). A refusal prints the heading and itself only: nothing is looked up before the target is read.
pub(crate) fn run(change: Change, args: &[String], machine: &mut dyn Machine, out: &mut dyn Write) -> io::Result<RunStatus> {
    let program = args.first().map_or("tmr.exe", String::as_str);
    writeln!(out, "{}", change.heading())?;
    let typed = match args.get(2..).unwrap_or_default() {
        [] => None,
        [target] => match parse_target(target) {
            Ok(target) => Some(target),
            Err(e) => {
                writeln!(out, "❌ Usage: {program} {} [User:<name> | Group:<name>]", change.flag())?;
                writeln!(out, "{}", e.text())?;
                return Ok(RunStatus::NotRun);
            }
        },
        _ => {
            writeln!(out, "❌ Usage: {program} {} [User:<name> | Group:<name>]", change.flag())?;
            writeln!(out, "   A name with spaces goes in quotes: \"Group:Remote Desktop Users\"")?;
            return Ok(RunStatus::NotRun);
        }
    };
    let target = match &typed {
        None => None,
        Some(target) => match machine.find(&target.name).map_err(Refusal::Lookup).and_then(|found| choose(target, found)) {
            Ok(found) => Some(Account { reach: breadth(&found.sid), sid: found.sid, name: found.name, kind: target.kind }),
            // The lookup's handle needs TMR run as administrator on some PCs: that is the refusal to give then
            Err(Refusal::Lookup(_)) if !machine.administrator() => return not_administrator(out, program, change),
            Err(refusal) => {
                writeln!(out, "{}", refusal_text(target, &refusal))?;
                return Ok(RunStatus::NotRun);
            }
        },
    };
    match change {
        Change::Grant => grant(program, target, machine, out),
        Change::Remove => remove(program, target, machine, out),
    }
}

fn not_administrator(out: &mut dyn Write, program: &str, change: Change) -> io::Result<RunStatus> {
    writeln!(out, "❌ Not running as Administrator: run {program} {} from a console run as administrator", change.flag())?;
    Ok(RunStatus::NotRun)
}

fn target_line(out: &mut dyn Write, account: &Account) -> io::Result<()> {
    writeln!(out, "🎯 Target: {} (a {})", account.name, account.kind.noun())
}

/// The sign-in it takes, for `me`: the line, for an administrator under UAC that it then reaches every run, and the domain's
fn sign_in(out: &mut dyn Write, me: &Me) -> io::Result<()> {
    writeln!(out, "🔄 SIGN-IN REQUIRED: sign out of Windows and back in (or restart it) for it to take effect")?;
    if me.uac {
        writeln!(out, "   After it, TMR has it in every run, run as administrator or not")?;
    }
    if me.domain {
        writeln!(out, "   This PC is in a domain: a domain Group Policy that sets this right replaces a local grant")?;
    }
    Ok(())
}

/// `--setup-large-pages`: the diagnostic, the target, then, run as administrator, the grant: to the target, or with none to the account
/// TMR runs as unless the policy gives it to the account or one of its groups already. The closing check as before.
fn grant(program: &str, target: Option<Account>, machine: &mut dyn Machine, out: &mut dyn Write) -> io::Result<RunStatus> {
    // With a target, the diagnostic and the closing check are this sign-in's, not the target's: say so (the owner, 2026-11-03)
    let targeted = target.is_some();
    if targeted {
        writeln!(out, "🔎 First, this sign-in's own large pages (the account TMR runs as); the grant to the target follows")?;
    }
    writeln!(out, "{}", machine.diagnostic())?;
    if let Some(account) = &target {
        target_line(out, account)?;
        if let Some(warning) = account.reach.warning() {
            writeln!(out, "⚠️  {} is BROAD (use with caution): {warning}. Granting it, as asked.", account.name)?;
        }
    }
    if !machine.administrator() {
        return not_administrator(out, program, Change::Grant);
    }
    let me = match machine.me() {
        Ok(me) => me,
        Err(e) => {
            writeln!(out, "❌ Couldn't read this process's token: {e}")?;
            return Ok(RunStatus::NotRun);
        }
    };
    let holders = match machine.holders(Change::Grant) {
        Ok(holders) => holders,
        Err(e) => {
            writeln!(out, "❌ Couldn't read the security policy: {e}")?;
            return Ok(RunStatus::NotRun);
        }
    };
    let status = match target {
        None if me.in_token => {
            writeln!(out, "✅ This sign-in has it already: nothing to grant")?;
            RunStatus::Passed
        }
        None => match held_by(&holders, &me.you, &me.groups, &me.mine, &me.names) {
            Some(Holder::You) => {
                writeln!(out, "✅ The security policy gives it to {}: nothing to grant", me.you.name)?;
                sign_in(out, &me)?;
                RunStatus::Passed
            }
            Some(Holder::Group(group)) => {
                writeln!(out, "✅ The security policy gives it to {group}, a group {} is in: nothing to grant", me.you.name)?;
                sign_in(out, &me)?;
                RunStatus::Passed
            }
            None => grant_to(program, &me.account(), &me, &holders, machine, out)?,
        },
        Some(account) => grant_to(program, &account, &me, &holders, machine, out)?,
    };
    match (machine.closing_check(), targeted) {
        (Ok(()), false) => {
            writeln!(out, "\n🎉 SUCCESS: Large pages are now enabled and working!")?;
            writeln!(out, "You can now run TMR for optimal performance.")?;
        }
        (Err(e), false) => {
            writeln!(out, "\n⚠️  Large pages are still not available: {}", e)?;
            writeln!(out, "Sign out and back in (or restart Windows) if it was just granted; until then TMR asks before running on")?;
            writeln!(out, "standard 4 KiB pages.")?;
        }
        (Ok(()), true) => {
            writeln!(out, "\nℹ️  This sign-in (the account TMR runs as) can use large pages; that is about it, not the target")?
        }
        (Err(e), true) => {
            writeln!(out, "\nℹ️  This sign-in (the account TMR runs as) can't use large pages: {e}. That is about it, not the target")?
        }
    }
    Ok(status)
}

/// The grant to `account`, unless the policy gives it to it already
fn grant_to(
    program: &str, account: &Account, me: &Me, holders: &[Sid], machine: &mut dyn Machine, out: &mut dyn Write,
) -> io::Result<RunStatus> {
    let yours = me.mine.contains(&account.sid);
    if holders.contains(&account.sid) {
        writeln!(out, "✅ {} has it already in the security policy: nothing to grant", account.name)?;
        if yours && !me.in_token {
            sign_in(out, me)?;
        }
        return Ok(RunStatus::Passed);
    }
    match machine.change(Change::Grant, &account.sid) {
        Ok(()) => {
            writeln!(
                out,
                "✅ Granted 'Lock pages in memory' (SeLockMemoryPrivilege) to {}, which keeps it until it is removed \
                 ({program} --remove-large-pages {})",
                account.name,
                argument(account.kind, &account.name)
            )?;
            writeln!(out)?;
            if yours {
                sign_in(out, me)?;
            } else {
                writeln!(out, "🔄 It takes effect at the next sign-in of each account it reaches")?;
            }
            Ok(RunStatus::Passed)
        }
        Err(e) => {
            writeln!(out, "❌ Couldn't grant it to {}: {e}", account.name)?;
            writeln!(out)?;
            writeln!(out, "📖 Manual Setup:")?;
            writeln!(out, "1. Run gpedit.msc as Administrator (Windows Pro and up: Home has none)")?;
            writeln!(
                out,
                "2. Navigate to: Computer Configuration → Windows Settings → Security Settings → Local Policies → User Rights Assignment"
            )?;
            writeln!(out, "3. Double-click 'Lock pages in memory'")?;
            writeln!(out, "4. Add {}", account.name)?;
            writeln!(out, "5. Sign out and back in (or restart Windows)")?;
            Ok(RunStatus::NotRun)
        }
    }
}

/// `--remove-large-pages`: run as administrator, the right taken from the target, or with none from the account TMR runs as, when the
/// policy gives it to it; a group of the account's that holds it is named, for its own removal
fn remove(program: &str, target: Option<Account>, machine: &mut dyn Machine, out: &mut dyn Write) -> io::Result<RunStatus> {
    if let Some(account) = &target {
        target_line(out, account)?;
    }
    if !machine.administrator() {
        return not_administrator(out, program, Change::Remove);
    }
    let me = match machine.me() {
        Ok(me) => me,
        Err(e) => {
            writeln!(out, "❌ Couldn't read this process's token: {e}")?;
            return Ok(RunStatus::NotRun);
        }
    };
    let holders = match machine.holders(Change::Remove) {
        Ok(holders) => holders,
        Err(e) => {
            writeln!(out, "❌ Couldn't read the security policy: {e}")?;
            return Ok(RunStatus::NotRun);
        }
    };
    let given = target.is_some();
    let account = target.unwrap_or_else(|| me.account());
    if !holders.contains(&account.sid) {
        writeln!(out, "✅ {} doesn't have it in the security policy: nothing to remove", account.name)?;
        if !given && let Some(Holder::Group(group)) = held_by(&holders, &me.you, &me.groups, &me.mine, &me.names) {
            writeln!(
                out,
                "   It comes from {group}, a group {} is in: {program} --remove-large-pages {} removes it there",
                me.you.name,
                argument(Kind::Group, &group)
            )?;
        }
        return Ok(RunStatus::Passed);
    }
    match machine.change(Change::Remove, &account.sid) {
        Ok(()) => {
            writeln!(out, "✅ Removed 'Lock pages in memory' (SeLockMemoryPrivilege) from {}", account.name)?;
            writeln!(out, "   A sign-in keeps the rights it began with: it goes at the next sign-in")?;
            Ok(RunStatus::Passed)
        }
        Err(e) => {
            writeln!(out, "❌ Couldn't remove it from {}: {e}", account.name)?;
            Ok(RunStatus::NotRun)
        }
    }
}

/// This machine: its token, read once, and its policy, opened to read for the lookups and again for the change with the access it needs
#[derive(Default)]
pub(crate) struct ThisMachine {
    token: Option<Token>,
    read: Option<Policy>,
    change: Option<Policy>,
}

impl ThisMachine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn read_token(&mut self) -> Result<&Token, String> {
        if self.token.is_none() {
            self.token = Some(Token::read()?);
        }
        self.token.as_ref().ok_or_else(|| "no token".to_string())
    }

    fn read_policy(&mut self) -> Result<&Policy, String> {
        if self.read.is_none() {
            self.read = Some(Policy::open(Access::Read)?);
        }
        self.read.as_ref().ok_or_else(|| "no policy".to_string())
    }
}

impl Machine for ThisMachine {
    fn diagnostic(&mut self) -> String {
        diagnose_large_pages()
    }

    fn administrator(&mut self) -> bool {
        self.read_token().is_ok_and(administrator)
    }

    fn find(&mut self, name: &str) -> Result<Vec<Found>, String> {
        let policy = self.read_policy()?;
        let account_domain = policy.account_domain()?;
        let primary = policy.primary_domain().unwrap_or_else(|e| {
            log::debug!("This PC's domain couldn't be read ({e}): {name} is looked up on this PC");
            None
        });
        // Each SID with its type as its lookup gave it, and whether it stands without a name: the English names' don't, as a group this
        // Windows lacks has none (Account Operators, off a domain controller)
        let looked_up = policy.lookup(&lookups(name, &account_domain, primary.as_deref()), true)?;
        let mut sids: Vec<(Sid, SID_NAME_USE, bool)> = looked_up.into_iter().flatten().map(|(sid, kind)| (sid, kind, true)).collect();
        sids.extend(well_known(name).map(|sid| (sid, SidTypeUnknown, false)));
        // The last resort: a SID, for scripts and a group Windows can't name
        if sids.is_empty()
            && let Ok(sid) = name.parse::<Sid>()
        {
            sids.push((sid, SidTypeUnknown, true));
        }
        let mut unique: Vec<(Sid, SID_NAME_USE, bool)> = Vec::new();
        for (sid, kind, unnamed) in sids {
            if !unique.iter().any(|(seen, ..)| *seen == sid) {
                unique.push((sid, kind, unnamed));
            }
        }
        let names = policy.names(&unique.iter().map(|(sid, ..)| sid.clone()).collect::<Vec<_>>())?;
        Ok(unique
            .into_iter()
            .zip(names)
            .filter_map(|((sid, kind, unnamed), named)| match named {
                Some(named) => Some(Found { sid, kind: named.kind, name: named.name }),
                None => unnamed.then(|| Found { name: sid.to_string(), sid, kind }),
            })
            .collect())
    }

    fn me(&mut self) -> Result<Me, String> {
        self.read_token()?;
        if self.read.is_none() {
            self.read = Policy::open(Access::Read).map_err(|e| log::warn!("The policy couldn't be opened to name the groups: {e}")).ok();
        }
        let Some(token) = &self.token else {
            return Err("no token".to_string());
        };
        let (you, groups, names) = who(token, self.read.as_ref(), &[]);
        let mine = std::iter::once(token.user.clone()).chain(token.groups.iter().map(|(sid, _)| sid.clone())).collect();
        Ok(Me { you, groups, mine, names, in_token: in_token(), uac: uac_elevated(token), domain: in_a_domain() })
    }

    fn holders(&mut self, change: Change) -> Result<Vec<Sid>, String> {
        let policy = Policy::open(match change {
            Change::Grant => Access::Grant,
            Change::Remove => Access::Read,
        })?;
        let holders = policy.holders();
        self.change = Some(policy);
        holders
    }

    fn change(&mut self, change: Change, sid: &Sid) -> Result<(), String> {
        let policy = self.change.as_ref().ok_or("the policy wasn't opened for the change")?;
        log::info!("{} 'Lock pages in memory': {sid}", if change == Change::Grant { "Granting" } else { "Removing" });
        match change {
            Change::Grant => policy.add(sid),
            Change::Remove => policy.remove(sid),
        }
    }

    fn closing_check(&mut self) -> Result<(), &'static str> {
        super::privileges::check_large_page_privilege()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(text: &str) -> Sid {
        text.parse().unwrap_or_else(|()| panic!("{text} is a SID"))
    }

    fn found(name: &str, kind: SID_NAME_USE, text: &str) -> Found {
        Found { sid: sid(text), kind, name: name.to_string() }
    }

    /// TODO 113: a target's prefix, in any case, and its name, trimmed, inner spaces kept; a UPN and a DNS domain read; no prefix, an
    /// unknown one and no name are refused, each saying how to write it
    #[test]
    #[rustfmt::skip]
    fn a_target_takes_a_prefix_and_a_name() {
        let target = |kind, name: &str| Ok(Target { kind, name: name.to_string() });
        for (typed, expected) in [
            ("Group:witches", target(Kind::Group, "witches")),
            (r"user:.\witches", target(Kind::User, r".\witches")),
            (r"USER:PC\owner", target(Kind::User, r"PC\owner")),
            (r"Group:NT AUTHORITY\Authenticated Users", target(Kind::Group, r"NT AUTHORITY\Authenticated Users")),
            ("User: a@b.com ", target(Kind::User, "a@b.com")),
            (r"Group:corp.example.com\testers", target(Kind::Group, r"corp.example.com\testers")),
            ("Group:S-1-5-32-544", target(Kind::Group, "S-1-5-32-544")),
            ("witches", Err(TargetError::NoKind("witches".to_string()))),
            ("Team:x", Err(TargetError::UnknownKind("Team:x".to_string()))),
            ("Group:", Err(TargetError::NoName(Kind::Group))),
            ("Group:  ", Err(TargetError::NoName(Kind::Group))),
            ("User:", Err(TargetError::NoName(Kind::User))),
        ] {
            assert_eq!(parse_target(typed), expected, "{typed:?}");
        }
    }

    /// TODO 113: the names a target is looked up by: `.\name` on this PC alone (its account domain's name too, as a name equal to this
    /// PC's is the PC alone); a bare name there and in the domain it is joined to, once when they are one; a domain or a UPN as written
    #[test]
    fn a_name_is_looked_up_where_it_says() {
        let names = |name: &str, primary| lookups(name, "PC", primary);
        assert_eq!(names(r".\witches", Some("CORP")), ["witches", r"PC\witches"]);
        assert_eq!(names(r".\", Some("CORP")), Vec::<String>::new());
        assert_eq!(names(r".\CORP\witches", Some("CORP")), Vec::<String>::new(), "not this PC's: the domain's");
        assert_eq!(names(r".\alex@corp.example.com", Some("CORP")), Vec::<String>::new(), "a UPN is the domain's");
        assert_eq!(names("witches", Some("CORP")), ["witches", r"PC\witches", r"CORP\witches"]);
        assert_eq!(names("witches", None), ["witches", r"PC\witches"], "a workgroup");
        assert_eq!(names("witches", Some("pc")), ["witches", r"PC\witches"], "a domain controller: its account domain is the domain");
        assert_eq!(names(r"CORP\witches", Some("CORP")), [r"CORP\witches"]);
        assert_eq!(names("alex@corp.example.com", Some("CORP")), ["alex@corp.example.com"]);
    }

    /// TODO 113: Windows' built-in and automatic groups by their English names, on any Windows: by the full name or the part after its `\`,
    /// in any case, `.\` allowed; each a SID, each name once
    #[test]
    fn the_english_names_give_the_fixed_sids() {
        assert_eq!(well_known("Administrators"), Some(sid("S-1-5-32-544")));
        assert_eq!(well_known(r"builtin\administrators"), Some(sid("S-1-5-32-544")));
        assert_eq!(well_known(r".\Remote Desktop Users"), Some(sid("S-1-5-32-555")));
        assert_eq!(well_known("Everyone"), Some(sid("S-1-1-0")));
        assert_eq!(well_known(r"NT AUTHORITY\Authenticated Users"), Some(sid("S-1-5-11")));
        assert_eq!(well_known("INTERACTIVE"), Some(sid("S-1-5-4")));
        assert_eq!(well_known("Administratoren"), None, "German Windows' name is Windows' to look up");
        assert_eq!(well_known(r"CORP\Administrators"), None, "a domain's is the domain's");
        let mut seen = HashMap::new();
        for (name, text) in WELL_KNOWN {
            let sid = text.parse::<Sid>().unwrap_or_else(|()| panic!("{name}: {text}"));
            assert!(seen.insert(name.to_lowercase(), sid.clone()).is_none(), "{name} twice");
            let bare = name.rsplit('\\').next().unwrap();
            assert!(seen.insert(format!("bare {}", bare.to_lowercase()), sid).is_none(), "{bare} twice");
        }
    }

    /// TODO 113: what each prefix fits, over every type Windows gives a SID: `User:` a user, `Group:` its groups, aliases and well-known
    /// groups, either a service account and a SID it can't name; neither a domain, a computer, a label, a logon session's SID or a
    /// deleted account
    #[test]
    #[rustfmt::skip]
    fn each_prefix_fits_its_kind() {
        use Kind::{Group, User};
        let other = "S-1-5-21-1-2-3-1001";
        for (kind, sid_text, sid_kind, expected) in [
            (User, other, SidTypeUser, Ok(())),
            (Group, other, SidTypeUser, Err(Misfit::Is(User))),
            (User, other, SidTypeGroup, Err(Misfit::Is(Group))),
            (Group, other, SidTypeGroup, Ok(())),
            (Group, "S-1-5-32-544", SidTypeAlias, Ok(())),
            (User, "S-1-5-32-544", SidTypeAlias, Err(Misfit::Is(Group))),
            (Group, "S-1-5-11", SidTypeWellKnownGroup, Ok(())),
            (User, "S-1-5-11", SidTypeWellKnownGroup, Err(Misfit::Is(Group))),
            (User, "S-1-5-80-1-2-3-4-5", SidTypeWellKnownGroup, Ok(())),
            (Group, "S-1-5-80-1-2-3-4-5", SidTypeWellKnownGroup, Ok(())),
            (User, "S-1-5-18", SidTypeWellKnownGroup, Ok(())),
            (User, other, SidTypeUnknown, Ok(())),
            (Group, other, SidTypeUnknown, Ok(())),
            (User, "S-1-5-21-1-2-3", SidTypeDomain, Err(Misfit::Not("a domain"))),
            (Group, other, SidTypeComputer, Err(Misfit::Not("a computer"))),
            (Group, "S-1-16-12288", SidTypeLabel, Err(Misfit::Not("an integrity label"))),
            (Group, "S-1-5-5-0-1", SidTypeLogonSession, Err(Misfit::Not("a sign-in's own SID, which lasts one sign-in"))),
            (User, other, SidTypeDeletedAccount, Err(Misfit::Not("a deleted account"))),
            (Group, "S-1-15-3-1024-1", SidTypeUnknown, Err(Misfit::Not("an app's SID"))),
        ] {
            assert_eq!(fit(kind, &found("x", sid_kind, sid_text)), expected, "{kind:?} {sid_text} {sid_kind:?}");
        }
    }

    /// A stand-in machine: what it finds and reads, the outcome of its change, and every call made to it, which the golden shows after
    /// each case; its closing check passes when the token has the right, as the real one's does. It changes nothing, and fails the test on
    /// a change before the holders are read on a handle opened for it, or one the policy has made already: a grant to a holder, a removal
    /// from one that isn't.
    struct StandIn {
        administrator: bool,
        found: Result<Vec<Found>, String>,
        holders: Vec<&'static str>,
        in_token: bool,
        uac: bool,
        domain: bool,
        change: Result<(), String>,
        calls: Vec<String>,
    }

    impl Default for StandIn {
        fn default() -> Self {
            StandIn {
                administrator: true,
                found: Ok(Vec::new()),
                holders: Vec::new(),
                in_token: false,
                uac: false,
                domain: false,
                change: Ok(()),
                calls: Vec::new(),
            }
        }
    }

    impl Machine for StandIn {
        fn diagnostic(&mut self) -> String {
            self.calls.push("diagnostic".to_string());
            "<the large-page check's report>\n".to_string()
        }
        fn administrator(&mut self) -> bool {
            self.administrator
        }
        fn find(&mut self, name: &str) -> Result<Vec<Found>, String> {
            self.calls.push(format!("find {name}"));
            self.found.clone()
        }
        fn me(&mut self) -> Result<Me, String> {
            self.calls.push("me".to_string());
            let you = Candidate::new(r"PC\owner", Reach::You, sid("S-1-5-21-1-2-3-1001"));
            let groups = vec![
                Candidate::new(r"BUILTIN\Administrators", Reach::Group, sid("S-1-5-32-544")),
                Candidate::new(r"BUILTIN\Users", Reach::EveryLocalAccount, sid("S-1-5-32-545")),
                Candidate::new("Everyone", Reach::Automatic, sid("S-1-1-0")),
            ];
            let mine = ["S-1-5-21-1-2-3-1001", "S-1-5-32-544", "S-1-5-32-545", "S-1-1-0", "S-1-5-5-0-1"].map(sid).to_vec();
            Ok(Me { you, groups, mine, names: HashMap::new(), in_token: self.in_token, uac: self.uac, domain: self.domain })
        }
        fn holders(&mut self, change: Change) -> Result<Vec<Sid>, String> {
            self.calls.push(format!("holders {change:?}"));
            Ok(self.holders.iter().map(|h| sid(h)).collect())
        }
        fn change(&mut self, change: Change, sid: &Sid) -> Result<(), String> {
            // The policy is changed only after its holders are read on the handle opened for the change, and only where it differs from
            // the change: a grant to a SID it doesn't list, a removal from one it does
            let read = format!("holders {change:?}");
            assert!(self.calls.contains(&read), "{change:?} {sid} before the holders were read for it");
            let held = self.holders.contains(&sid.to_string().as_str());
            assert_eq!(
                held,
                change == Change::Remove,
                "{change:?} {sid}, which the policy {} lists",
                if held { "already" } else { "never" }
            );
            self.calls.push(format!("change {change:?} {sid}"));
            self.change.clone()
        }
        fn closing_check(&mut self) -> Result<(), &'static str> {
            self.calls.push("closing check".to_string());
            if self.in_token { Ok(()) } else { Err("SeLockMemoryPrivilege not assigned to current user") }
        }
    }

    /// TODO 113: both commands' every outcome, from stand-ins, each after a `## case` line with how it ended and then the calls it made:
    /// the grant to the account, to a group, to a broad group (warned, and made), to a service account by User:, to a SID Windows can't
    /// name, to a target while the account holds it (in its token and the policy: the target's grant all the same); held already by the
    /// target, and with no target by the account or a group of it (the sign-in, with the UAC and domain lines); in the token already; not
    /// run as administrator; a grant that failed; the refusals of the target (no type, an unknown type, no name, two targets: nothing
    /// called), a type that doesn't fit (qualified and not), a domain's name, nothing found, `local\`, two found, a lookup that failed, run
    /// as administrator or not; and the removal: from the account, from a group, of nothing (a group of the account's holding it named;
    /// a target that doesn't hold it, the account holding it or not: no removal called), failed, not run as administrator. No test reads
    /// or changes the policy.
    #[test]
    fn the_commands_say_what_they_did() {
        let owner = || found(r"PC\owner", SidTypeUser, "S-1-5-21-1-2-3-1001");
        let admins = || found(r"BUILTIN\Administrators", SidTypeAlias, "S-1-5-32-544");
        let users = || found(r"BUILTIN\Users", SidTypeAlias, "S-1-5-32-545");
        let local_witches = || found(r"PC\witches", SidTypeAlias, "S-1-5-21-1-2-3-1010");
        let corp_witches = || found(r"CORP\witches", SidTypeGroup, "S-1-5-21-7-8-9-1111");
        let denied = Err("LsaAddAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)".to_string());
        let trust = Err("LsaLookupNames2: The trust relationship between this workstation and the primary domain failed. \
                         (1789, NTSTATUS 0xC000018D)"
            .to_string());
        let machine = StandIn::default;
        #[rustfmt::skip]
        let cases: Vec<(&str, Change, &[&str], StandIn)> = vec![
            ("granted to the account", Change::Grant, &[], machine()),
            ("granted to the account, an administrator under UAC on a domain PC", Change::Grant, &[],
                StandIn { uac: true, domain: true, ..machine() }),
            ("granted to a group", Change::Grant, &["Group:Administrators"], StandIn { found: Ok(vec![admins()]), ..machine() }),
            ("granted to a broad group", Change::Grant, &["group:users"], StandIn { found: Ok(vec![users()]), ..machine() }),
            ("granted to a group the account isn't in", Change::Grant, &[r"Group:CORP\witches"],
                StandIn { found: Ok(vec![corp_witches()]), ..machine() }),
            ("granted to a target, the account holding it in its token and the policy", Change::Grant, &[r"Group:CORP\witches"],
                StandIn { found: Ok(vec![corp_witches()]), in_token: true, holders: vec!["S-1-5-21-1-2-3-1001"], ..machine() }),
            ("granted to a service account, by User:", Change::Grant, &[r"User:NT SERVICE\MSSQLSERVER"],
                StandIn { found: Ok(vec![found(r"NT SERVICE\MSSQLSERVER", SidTypeWellKnownGroup, "S-1-5-80-1-2-3-4-5")]), ..machine() }),
            ("granted to a SID Windows can't name", Change::Grant, &["Group:S-1-5-21-9-9-9-1234"],
                StandIn { found: Ok(vec![found("S-1-5-21-9-9-9-1234", SidTypeUnknown, "S-1-5-21-9-9-9-1234")]), ..machine() }),
            ("held already by the target", Change::Grant, &["Group:Administrators"],
                StandIn { found: Ok(vec![admins()]), holders: vec!["S-1-5-32-544"], ..machine() }),
            ("held already by the account, no target", Change::Grant, &[], StandIn { holders: vec!["S-1-5-21-1-2-3-1001"], ..machine() }),
            ("held already through a group, no target, under UAC on a domain PC", Change::Grant, &[],
                StandIn { holders: vec!["S-1-5-80-1-2-3-4-5", "S-1-5-32-544"], uac: true, domain: true, ..machine() }),
            ("in the token already", Change::Grant, &[], StandIn { in_token: true, holders: vec!["S-1-5-32-544"], ..machine() }),
            ("not run as administrator", Change::Grant, &[], StandIn { administrator: false, ..machine() }),
            ("not run as administrator, with a target", Change::Grant, &["User:PC\\owner"],
                StandIn { administrator: false, found: Ok(vec![owner()]), ..machine() }),
            ("the grant failed", Change::Grant, &["Group:Administrators"],
                StandIn { found: Ok(vec![admins()]), change: denied.clone(), ..machine() }),
            ("no type", Change::Grant, &["witches"], machine()),
            ("an unknown type", Change::Grant, &["Team:x"], machine()),
            ("no name", Change::Grant, &["Group: "], machine()),
            ("two targets", Change::Grant, &["Group:Remote", "Desktop Users"], machine()),
            ("a group, not a user", Change::Grant, &["User:witches"], StandIn { found: Ok(vec![local_witches()]), ..machine() }),
            ("a user, not a group, qualified", Change::Grant, &[r"Group:PC\owner"], StandIn { found: Ok(vec![owner()]), ..machine() }),
            ("a domain's name", Change::Grant, &["User:pc"],
                StandIn { found: Ok(vec![found("PC", SidTypeDomain, "S-1-5-21-1-2-3")]), ..machine() }),
            ("a domain's name, and a group", Change::Grant, &["User:witches"],
                StandIn { found: Ok(vec![found("PC", SidTypeDomain, "S-1-5-21-1-2-3"), local_witches()]), ..machine() }),
            ("nothing found", Change::Grant, &["Group:witches"], machine()),
            ("nothing found, local\\", Change::Grant, &[r"Group:local\witches"], machine()),
            ("two found", Change::Grant, &["Group:witches"], StandIn { found: Ok(vec![local_witches(), corp_witches()]), ..machine() }),
            ("one found twice, and a user of the name", Change::Grant, &["Group:witches"],
                StandIn {
                    found: Ok(vec![local_witches(), local_witches(), found(r"CORP\witches", SidTypeUser, "S-1-5-21-7-8-9-1112")]),
                    ..machine()
                }),
            ("the lookup failed", Change::Grant, &[r"Group:CORP\testers"], StandIn { found: trust.clone(), ..machine() }),
            ("the lookup failed, not run as administrator", Change::Grant, &[r"Group:CORP\testers"],
                StandIn { found: trust, administrator: false, ..machine() }),
            ("removed from the account", Change::Remove, &[], StandIn { holders: vec!["S-1-5-21-1-2-3-1001"], ..machine() }),
            ("removed from a broad group", Change::Remove, &["Group:Everyone"],
                StandIn { found: Ok(vec![found("Everyone", SidTypeWellKnownGroup, "S-1-1-0")]), holders: vec!["S-1-1-0"], ..machine() }),
            ("nothing to remove, a group holding it", Change::Remove, &[], StandIn { holders: vec!["S-1-5-32-544"], ..machine() }),
            ("nothing to remove from the target", Change::Remove, &["Group:Administrators"],
                StandIn { found: Ok(vec![admins()]), ..machine() }),
            ("nothing to remove from the target, the account holding it", Change::Remove, &[r"Group:CORP\witches"],
                StandIn { found: Ok(vec![corp_witches()]), in_token: true, holders: vec!["S-1-5-21-1-2-3-1001"], ..machine() }),
            ("the removal failed", Change::Remove, &[], StandIn {
                holders: vec!["S-1-5-21-1-2-3-1001"],
                change: Err("LsaRemoveAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)".to_string()),
                ..machine()
            }),
            ("removal, not run as administrator", Change::Remove, &[], StandIn { administrator: false, ..machine() }),
            ("removal, no type", Change::Remove, &["Everyone"], machine()),
        ];
        let mut text = String::new();
        for (case, change, targets, mut machine) in cases {
            let args: Vec<String> = ["tmr.exe", change.flag()].iter().chain(targets).map(|a| a.to_string()).collect();
            let mut out = Vec::new();
            let status = run(change, &args, &mut machine, &mut out).unwrap();
            text += &format!("## {case}: {status:?}\n{}", String::from_utf8(out).unwrap());
            text += &format!("[calls: {}]\n\n", machine.calls.join(", "));
        }
        crate::test_support::check("large_page_setup", &text);
    }

    /// TODO 113, run by name: this machine's lookups, read-only (a handle opened to read; nothing granted or removed). Each English name
    /// of `WELL_KNOWN` that Windows names here is its SID (on English Windows, all; else Windows' own names, printed); and the targets
    /// resolve as the commands resolve them: the built-in groups by name and by `.\`, Everyone, a SID, and the account TMR runs as by its
    /// full name and by `.\` when it is this PC's.
    #[test]
    #[ignore = "reads this machine's security policy, changes nothing: run it by name"]
    fn the_names_resolve_here() {
        let policy = Policy::open(Access::Read).expect("the policy, to read");
        let sids: Vec<Sid> = WELL_KNOWN.iter().map(|(_, text)| sid(text)).collect();
        for ((english, text), named) in WELL_KNOWN.iter().zip(policy.names(&sids).expect("their names")) {
            match named {
                Some(named) => println!("{text:14} {english:60} Windows: {} ({:?})", named.name, named.kind),
                None => println!("{text:14} {english:60} Windows: no name"),
            }
        }
        println!("account domain {:?}, primary {:?}", policy.account_domain(), policy.primary_domain());
        let mut machine = ThisMachine::new();
        let resolve = |machine: &mut ThisMachine, typed: &str| {
            let target = parse_target(typed).expect(typed);
            let found = machine.find(&target.name).expect(typed);
            println!("{typed}: {found:?}");
            choose(&target, found)
        };
        for (typed, expected) in [
            ("Group:Administrators", "S-1-5-32-544"),
            (r"Group:.\Administrators", "S-1-5-32-544"),
            (r"Group:BUILTIN\Administrators", "S-1-5-32-544"),
            ("Group:Everyone", "S-1-1-0"),
            ("Group:S-1-5-32-545", "S-1-5-32-545"),
            ("User:SYSTEM", "S-1-5-18"),
        ] {
            assert_eq!(resolve(&mut machine, typed).map(|found| found.sid), Ok(sid(expected)), "{typed}");
        }
        // An English name of a group this Windows may lack (Account Operators, off a domain controller) is no SID unnamed
        let account_operators = resolve(&mut machine, "Group:Account Operators");
        assert!(account_operators.as_ref().is_ok_and(|found| found.kind != SidTypeUnknown) || account_operators == Err(Refusal::NotFound));
        let token = Token::read().expect("the token");
        let me = super::super::privileges::account_of(&token);
        assert_eq!(resolve(&mut machine, &format!("User:{me}")).map(|found| found.sid), Ok(token.user.clone()), "{me}");
        let account_domain = policy.account_domain().expect("this PC's account domain");
        if let Some((domain, name)) = me.split_once('\\')
            && domain.eq_ignore_ascii_case(&account_domain)
        {
            assert_eq!(resolve(&mut machine, &format!(r"User:.\{name}")).map(|found| found.sid), Ok(token.user.clone()), ".\\{name}");
        }
        assert!(matches!(resolve(&mut machine, "User:Administrators"), Err(Refusal::Misfit(_, Misfit::Is(Kind::Group)))), "a group");
        assert_eq!(resolve(&mut machine, "Group:no-such-group-tmr-113"), Err(Refusal::NotFound));
    }
}
