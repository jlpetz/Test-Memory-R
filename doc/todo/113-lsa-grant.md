# TODO 113: the large-page grant through LSA, by SID, no secedit

Found 2026-11-02 in TODO 29's review of 97fda82. The secedit grant exported the whole security policy to `%TEMP%\tmr_secpol.inf`, in
the user's own temp folder, edited it and re-applied it elevated with `secedit /configure`: an unelevated program of the same user
could swap the file in between, and the whole template, not one right, was re-applied. The account it named came from
`%USERDOMAIN%\%USERNAME%`, which whoever starts TMR sets. And the session's "restart or grant" asked whether *any* account held the
right, so a SQL Server service account holding it made TMR ask for a restart forever.

The owner's decision (2026-11-03): no external tool. TMR changes the one right, in process, through LSA, by SID, and checks the
holders against the user's own account and groups. The design (`..\lsa-design\design.json`), its security review (`review.json`) and
the build's decisions (`decisions.md`) were written 2026-11-03, outside the repo; this file records what was built from them.

## As built (2026-11-03)

### The calls (`src/memory/lsa.rs`)

- **One FFI file.** `Sid` (a SID in `Vec<u32>`, compared as bytes, `S-1-...` both ways, a pure parser), `Token` (TokenUser, TokenGroups,
  TokenElevationType, read once with `TOKEN_QUERY`), `account_name` (`LookupAccountSidW`, retried while `ERROR_INSUFFICIENT_BUFFER`,
  strings cut at the first NUL), `in_a_domain` (`GetComputerNameExW(ComputerNameDnsDomain)` non-empty), `Policy` and `lsa_error`.
- **`Policy::open(Access)`**, least access each time: `Read` = `POLICY_LOOKUP_NAMES | POLICY_VIEW_LOCAL_INFORMATION` (the question,
  the lookups, the holders, and the removal: `LsaRemoveAccountRights` needs `POLICY_LOOKUP_NAMES`); `Grant` = that and
  `POLICY_CREATE_ACCOUNT` (a first grant creates the account's object). The handle is an `Owned<LSA_HANDLE>`.
- **`Policy::holders`** is the one place `LsaEnumerateAccountsWithUserRight` is called, always with the right: with `None` it enumerates
  every account in the policy, and every group would look like a holder (the restart-forever bug again). `STATUS_NO_MORE_ENTRIES` is
  nobody (a warning status, which `is_ok` calls an error).
- **`add`/`remove`**: `LsaAddAccountRights` and `LsaRemoveAccountRights` (`AllRights` false, the one right), by SID.
- **`names`**: one `LsaLookupSids2` for every SID at once (the token's groups and the holders), so a PC away from its domain controller
  waits out one time-out, not one a group. `STATUS_SOME_NOT_MAPPED` is success with partial names; a SID Windows can't name is `None`.
- **`lookup`**: one `LsaLookupNames2` with `LSA_LOOKUP_ISOLATED_AS_LOCAL`: a name without a domain is looked for on this PC only.
- **`account_domain`/`primary_domain`**: `LsaQueryInformationPolicy`, `PolicyAccountDomainInformation` (this PC's own accounts' domain,
  whose name is the PC's) and `PolicyPrimaryDomainInformation` (the domain it's joined to; no SID in a workgroup).
- **Hygiene**: every LSA buffer is freed on every path by a guard bound by name (`let _buffer = LsaBuffer(..)`), on
  `STATUS_NONE_MAPPED` too; every SID is copied out before its buffer goes; TOKEN_GROUPS' `GroupCount` is checked against the length
  the call returned and the array taken from the raw buffer, each SID read from that buffer's bytes; `LsaNtStatusToWinError`'s 317
  (no Win32 code) prints the NTSTATUS alone, others as `LsaAddAccountRights: Access is denied. (5, NTSTATUS 0xC0000022)`. LIFETIME
  comments on the LSA strings (the right's name, the names looked up) and the PSID array, SAFETY on every block.
- **Cargo**: one new feature, `Win32_Security_Authentication_Identity`. Cargo.lock doesn't change.

### The session's question (`src/memory/privileges.rs`, `src/session.rs`)

Asked when this token lacks the right (not just disabled) and the run's page range allows large pages (`maxpage=` large or huge,
whatever `minpage=` is: decision 5a), one `Frontend::ask_large_pages` with three questions, each with its own answers
(`LargePageAnswer`), the session acting on its own copy of the question so a frontend can't change whom a grant goes to. With
`minpage=regular maxpage=regular` nothing is asked. The run takes 4 KiB pages only when the user says so (`Regular`, which sets both
to `regular` for this run); anything else ends the session before allocating, nothing granted.

| Question | When | Answers | Empty, unreadable, an answer not its own, or a frontend that can't ask |
|---|---|---|---|
| `SignIn { account, via, uac, domain }` | administrator; the policy gives it to the account or a group of its token | `SignOut` (exit 0), `Regular` | `Quit` (exit 2) |
| `Grant { candidates, uac, domain }` | administrator; neither holds it (another account holding it doesn't count) | `Grant(i)`, `Regular`, `Quit` | `Quit` (exit 2) |
| `NeedsAdministrator { account, uac }` | not an administrator | `Quit` (exit 2), `Regular` | `Quit` (exit 2) |

- **Who may grant** (`administrator`): BUILTIN\Administrators *enabled* in the token, not TokenIsElevated (an elevated Backup Operator
  would be offered a grant that fails).
- **`Quit`** ends the session with `Stopped::NeedsLargePages`, nothing allocated; the console's end says so ("Stopped before
  allocating ... minpage=regular maxpage=regular runs on them without asking"), the window's errors list too. A grant that fails
  ends it the same way, after saying why: going on would run slower unasked. The owner's rule (2026-09-14, "pause, warn, then let the
  individual decide", and decision 5a): with UAC, an administrator's double-click is the `NeedsAdministrator` case.
- **The candidates** (`who`, `order`): the account first, the default; then each real group of the token A-Z; then BUILTIN\Users, then
  Domain Users ("None" off a domain); then the automatic groups A-Z, each broad one warned of. Classified by SID (`breadth`), never by
  name: the automatic list S-1-1-0, S-1-2-\*, S-1-5-{1-4, 6-9, 11-15, 113, 114}, S-1-5-64-\*, S-1-5-65-1, S-1-18-\*. Left out: the logon
  SID, integrity labels, app SIDs (S-1-15-\*), deny-only groups (still counted as held), and what isn't a group. A group Windows can't
  name is listed by its `S-1-...` string.
- **A broad pick asks again**, No the default, stating the risk: "any program of any of these accounts can then lock RAM away from
  Windows". No goes back to the choice, granting nothing; the choice's own empty answer then quits. The console flushes type-ahead
  before each read (`console::flush_input`: `FlushConsoleInputBuffer` when stdin is a console); the window opens a second dialog whose
  first button, the one WinUI focuses, and Esc are the No, back to the first dialog, and whose second grants (the security review,
  2026-11-03: as the close button, the No sat after a focused grant that Enter, Space or a double-click took).
- **The held check** (`held_by`): the policy names the account (preferred), a group in the list, or another SID of the token, named
  as Windows names it (its SID only when Windows can't). The log names every holder with its SID ("held by NT SERVICE\MSSQLSERVER").
- **Words**: a new sign-in, not a restart ("A restart of the computer isn't required ... effective the next time the owner of the
  account logs on"); on a domain PC one more line, a domain Group Policy that sets the right replaces a local grant.

### UAC (review 1)

Microsoft Learn, "Access Token Changes" (the Windows Vista UAC developer guide):
<https://learn.microsoft.com/en-us/previous-versions/dotnet/articles/bb530410(v=msdn.10)>. A token with a restricted RID
(Administrators and the other operator groups) is filtered to five privileges, and SeLockMemoryPrivilege isn't one of them;
privileges are filtered by name, so it makes no difference whether the account or Administrators holds the right. So for an
administrator under UAC the right reaches TMR only when it runs as administrator; a standard user granted it has it in every run.

- Shown as a line when the token is the elevated half (`TokenElevationTypeFull`) with Administrators enabled (`uac_elevated`), not on
  Full alone (a non-admin's split can come from a privilege); and to a non-elevated administrator (Limited, Administrators deny-only:
  `uac_filtered`) in `NeedsAdministrator` and in the diagnostic's hint.
- README.md and dist/docs/guide.md say so: their "grant once, sign out and back in, double-click TMR" never gave an administrator's
  double-click large pages.
- The owner's desktop check confirms it; it didn't gate the build: (1) an elevated `tmr --setup-large-pages`, sign out and in, then
  `whoami /priv | findstr /i LockMemory` in a normal and an elevated console; (2) the same with `Group:Administrators`, after
  `tmr --remove-large-pages`; (3) optionally a standard account. Results: not run yet.

### The commands (`src/memory/large_page_setup.rs`)

`--setup-large-pages [User:<name> | Group:<name>]` and `--remove-large-pages [User:<name> | Group:<name>]`, none = the account TMR runs
as. The command is the request: neither asks, and a broad target is warned of and taken.

- **The prefix is required** with a name, as a user and a group can share one; unprefixed, an unknown prefix, no name, or two targets
  are refused before anything is looked up. `<name>` is `DOMAIN\name`, `.\name` (this PC's) or a bare name; `local\` is not `.\` (a
  NetBIOS domain may be named LOCAL), and the not-found message says so.
- **Resolution** (`lookups`, `choose`): a bare name is looked up alone (on this PC), as `<account domain>\name` (a name equal to the
  PC's is the PC's domain alone) and as `<primary domain>\name`, in one call; `.\name` the first two; `DOMAIN\name` and UPNs as typed.
  The prefix filters what that finds (`fit`: `User:` a user, `Group:` a group, alias or well-known group, either a service account,
  S-1-5-18/19/20 and 80/82/83-\*); one is taken, two are refused with both full names; none of its kind says what it found instead.
  Every message shows Windows' own `DOMAIN\name`.
- **People don't talk in SIDs** (the owner, 2026-11-03): Windows' built-in and automatic groups and service accounts are taken by
  their English names on any Windows (`WELL_KNOWN`, 47 entries, `Group:Administrators` is S-1-5-32-544 on German Windows too), and a
  table group this Windows lacks (Account Operators off a domain controller) is not found rather than granted to a SID that names
  nothing. A SID (`Group:S-1-5-32-544`) is the last resort, for scripts and unnamed groups: the help shows names, the not-found message
  mentions the SID last.
- **The flow** runs over a `Machine` trait, so every outcome is the engine's golden `testdata/console/large_page_setup.txt` with a
  stand-in, which also lists every call each case made (the refusals make none). Exit 2 on a refusal, when not an administrator, or a
  failed change; 0 when done or nothing to do. `logfile::wanted` keeps a log for both.

### The window (`crates/tmr-gui`)

Each question is a dialog with explicit buttons (decision 5a): the primary its yes (sign out, the grant, or for `NeedsAdministrator`
"Quit, to run TMR as administrator"), the secondary "Continue on 4 KiB pages this time", the close button "Quit" (none for
`NeedsAdministrator`, whose primary is the quit); Esc, the close button and a window gone with the question open quit. The grant's
dialog lists the candidates as radio buttons, each with its warning in sight, the account preselected; the selection is an `Option`
that mirrors the control (WinUI sends none when its items are replaced), and with none the grant button is disabled. A broad pick
opens the second dialog, whose No reopens the first. The window's `UNANSWERED` flag (a sign-in question gone unanswered ended as a
sign-out, exit 2) went with the change: no answer is now the quit, whose exit status is 2 already.

### Names (the renames are total)

`Frontend::large_page_restart` → `ask_large_pages`; `RestartChoice { Exit, Continue4K }` → `LargePageAnswer { SignOut, Grant(usize),
Regular, Quit }`; `LargePageQuestion::Restart`/`Grant { account }` → `SignIn`/`Grant { candidates }`/`NeedsAdministrator`;
`SessionEnd::RestartChosen` → `SignOutChosen`, with `Stopped::NeedsLargePages` new; session.rs's `LargePages::Restart` → `SignOut`,
with `Quit` new; `Command::SetupLargePages`
→ `SetupLargePages(Vec<String>)`, with `RemoveLargePages(Vec<String>)` new; `grant_large_pages(&str)` → `grant_large_pages(&Candidate)`;
the window's `RestartDialog`, `ask_restart`, `restart_asked`, `restart_dialog`, `answer_restart`, the field `restart`, `Message::Restart`
→ `LargePageDialog`, `ask_large_pages`, `large_pages_asked`, `large_page_dialog`, `answer_large_pages`, `reply`, `Message::LargePages`;
the golden `large_page_restart.txt` → `large_pages.txt`.

## Deleted

From `src/memory/privileges.rs`: `secedit()` (System32's path through `GetSystemDirectoryW`, `CREATE_NO_WINDOW`),
`grant_privilege_via_secedit` (export, UTF-16 decode, edit, re-encode, `/configure`, clean-up), `modify_security_policy`,
`check_if_privilege_in_policy`, `account()` (the account from `%USERDOMAIN%\%USERNAME%`), `setup_large_pages()` and `grant_report()`
(replaced by the commands' flow), the temp files `tmr_secpol.inf`, `tmr_secpol.sdb` and `tmr_quick_check.inf`, both UTF-16 LE decode
sites and the encode loop, `use std::fs` and `use std::process::Command`, `grant_large_pages`' substring rewriting of errors
("configure failed", "UTF-8", "encoding"), the "This is normal on Windows Server 2025 ... gpupdate /force" message, the grant report's
"already/assigned" branch ("POSSIBLE CAUSE ... restart needed"), and `question_for`'s `in_policy: bool`. The diagnostic's "👤 User:"
line, printed twice from the environment, is once, from the token. `git log -S LsaAddAccountRights` found no earlier LSA code in TMR;
TM5 grants the same way (`TM5\bin\awe.asm`, `SetIOPLto3`).

## Checked

- Pure tests: the SID parser both ways, the classification table, the order, the warnings, the held check, the question's gate with
  stand-ins (each closure read only when needed; `maxpage=regular` asks nothing), the administrator and UAC halves from stand-in
  tokens, `settle_large_pages`' every pairing, target parsing, `lookups`, `WELL_KNOWN`, `fit` over every SID type; goldens of the
  console's questions
  (`crates/tmr-cli/testdata/console/large_pages.txt`), the grant event, both commands and their refusals; the window's model.
- Two `#[ignore]`d read-only probes, run on the Sydney dev box (Server 2025, built-in Administrator, no UAC split) 2026-11-03: the
  token and the policy (`the_token_and_the_policy_read_back`: each holder cross-checked with `LsaEnumerateAccountRights`), and the names
  (`the_names_resolve_here`: every `WELL_KNOWN` entry named as in the table but Account and Server Operators, which exist only on a
  domain controller; `Group:Administrators`, `.\Administrators`, `BUILTIN\Administrators`, `Everyone`, a SID, `User:SYSTEM`, the account
  by its full name and by `.\`). Nothing in any test writes the policy.

## Left for later

- **The shakedown and the desktop check**: a grant and a removal on the dev box with a throwaway group (the owner approved), and the
  UAC check above; both with the owner's go-ahead, as both write the policy.
- **Over-the-shoulder elevation and Administrator protection** (review 5): the elevated account may not be the person signed in (the
  admin who typed the password; Administrator protection's hidden system-managed account). The heading shows the full `DOMAIN\name`,
  and `--setup-large-pages User:PC\you` names the desktop account. Offering the session's user too needs wtsapi32, a DLL-planting risk
  until TODO 114 is done.
- **DLL planting** of an elevated tmr.exe from a writable folder (review 6): TODO 114.
- **A sign-in that didn't help** (review, minor): remembering TokenStatistics' AuthenticationId in tmr-cfg.json when SignIn is asked, to
  offer the grant instead the next time (a GPO that replaces the grant, or a deny-only group).
- **winget's InstallationNotes** (`dist/winget/Petz.TMR.locale.en-US.yaml`) still say grant once and sign out and back in; the UAC
  sentence may belong there too.
