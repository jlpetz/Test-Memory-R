// Every `unsafe` block in this file crosses a Win32 FFI boundary, where the borrow checker is switched off precisely where the invariants
// get subtle (TODO #66). The lint below makes a missing `// SAFETY:` a warning here in the tests too; `src/lib.rs` makes it one in all
// shipped code. It was per file until 2026-10-23, lest the SIMD kernels need boilerplate, but their `unsafe fn`s need no blocks
// (`test_fn_safety.md` covers them): crate-wide added 30 sites, none a kernel's.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID, WIN32_ERROR};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SID_NAME_USE,
    SidTypeComputer, SidTypeDeletedAccount, SidTypeDomain, SidTypeInvalid, SidTypeLabel, SidTypeLogonSession, SidTypeUser,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrivileges,
};
use windows::Win32::System::Diagnostics::Debug::{FORMAT_MESSAGE_FROM_SYSTEM, FormatMessageW};
use windows::Win32::System::Memory::{MEM_COMMIT, MEM_LARGE_PAGES, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
use windows::Win32::System::SystemServices::{SE_GROUP_ENABLED, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID, SE_GROUP_USE_FOR_DENY_ONLY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PWSTR;

use super::lsa::{Access, Elevation, Named, Policy, Sid, Token, account_name, in_a_domain};

#[derive(Debug, PartialEq)]
enum PrivilegeState {
    NotAssigned,
    AssignedButDisabled,
    AssignedAndEnabled,
}

pub(super) fn format_win32_error(err: WIN32_ERROR) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: `FormatMessageW` writes at most `buffer.len()` UTF-16 units into the stack buffer we hand it, and the length we pass is that
    // array's own length. Without `FORMAT_MESSAGE_ALLOCATE_BUFFER` it never reallocates, so the pointer stays ours.
    let len = unsafe { FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM, None, err.0, 0, PWSTR(buffer.as_mut_ptr()), buffer.len() as u32, None) };
    if len == 0 { format!("Unknown error {}", err.0) } else { String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string() }
}

pub fn check_large_page_privilege() -> Result<(), &'static str> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing. `token` starts as a default `HANDLE` and is only used on
    // the paths where `OpenProcessToken` succeeded; every early return closes it exactly once. `luid` is fully initialised before
    // `LookupPrivilegeValueW` writes it, and the `VirtualAlloc` probe below is paired with `VirtualFree` on the success path.
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();

        if OpenProcessToken(process, TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return Err("Failed to open process token");
        }

        let privilege_name = windows::core::w!("SeLockMemoryPrivilege");
        let mut luid = LUID::default();

        if LookupPrivilegeValueW(None, privilege_name, &mut luid).is_err() {
            let _ = CloseHandle(token);
            return Err("Failed to lookup SeLockMemoryPrivilege");
        }

        // First, check if we have the privilege at all
        match check_privilege_in_token(token, luid) {
            PrivilegeState::NotAssigned => {
                let _ = CloseHandle(token);
                return Err("SeLockMemoryPrivilege not assigned to current user");
            }
            PrivilegeState::AssignedButDisabled => {
                log::info!("SeLockMemoryPrivilege assigned but disabled, attempting to enable...");

                // Try to enable it
                if let Err(e) = enable_privilege_in_token(token, luid) {
                    let _ = CloseHandle(token);
                    return Err(e);
                }

                // Verify it's now enabled
                match check_privilege_in_token(token, luid) {
                    PrivilegeState::AssignedAndEnabled => {
                        log::info!("Successfully enabled SeLockMemoryPrivilege");
                    }
                    _ => {
                        let _ = CloseHandle(token);
                        return Err("Failed to enable SeLockMemoryPrivilege (insufficient rights)");
                    }
                }
            }
            PrivilegeState::AssignedAndEnabled => {
                log::info!("SeLockMemoryPrivilege already enabled");
            }
        }

        let _ = CloseHandle(token);

        // Test with actual allocation (2MB - one large page)
        log::info!("Testing large page allocation with 2MB test allocation...");
        let test_size = 2 * 1024 * 1024;
        let ptr = VirtualAlloc(Some(null_mut()), test_size, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, PAGE_READWRITE);

        if ptr.is_null() {
            let err = GetLastError();
            let error_msg = format_win32_error(err);
            log::error!("Large page test allocation failed: {} ({})", error_msg, err.0);

            // Provide specific guidance based on error code
            match err.0 {
                1314 => Err("Large page privilege test failed (ERROR_PRIVILEGE_NOT_HELD)"),
                1450 => Err("Insufficient system resources for large pages (ERROR_NO_SYSTEM_RESOURCES)"),
                8 => Err("Not enough memory available for large pages (ERROR_NOT_ENOUGH_MEMORY)"),
                _ => Err("Large page test allocation failed"),
            }
        } else {
            // Clean up test allocation
            let _ = VirtualFree(ptr, 0, MEM_RELEASE);
            log::info!("Large page test allocation successful");
            Ok(())
        }
    }
}

/// # Safety
///
/// `token` must be an open process-token handle with `TOKEN_QUERY` access, and must stay open for the duration of the call — this function
/// never closes it; the caller owns that. Reading a closed or non-token handle is what makes this `unsafe` rather than the pointer work
/// below, which is bounded by the two-call `GetTokenInformation` size protocol.
unsafe fn check_privilege_in_token(token: HANDLE, privilege_luid: LUID) -> PrivilegeState {
    use windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES;

    // Get the size needed for token privileges
    let mut return_length = 0u32;
    let _ = GetTokenInformation(token, TokenPrivileges, None, 0, &mut return_length);

    if return_length == 0 {
        return PrivilegeState::NotAssigned;
    }

    // Allocate buffer and get actual privileges.
    //
    // ALIGNMENT: backed by `u64`, not `u8`. `TOKEN_PRIVILEGES` requires 4-byte alignment (DWORD `PrivilegeCount` + `LUID_AND_ATTRIBUTES`
    // array), but `Vec<u8>` only guarantees 1 byte — so reinterpreting a `Vec<u8>` as `*const TOKEN_PRIVILEGES` below is UB by the letter
    // of the language, and only "works" because the global allocator happens to hand back 8/16-byte-aligned blocks. `Vec<u64>` guarantees 8
    // >= 4 by construction, so the cast is sound rather than incidentally lucky. Keep the element type at least as aligned as any struct
    // read out of this buffer.
    let elements = (return_length as usize).div_ceil(std::mem::size_of::<u64>());
    let mut buffer = vec![0u64; elements];
    if GetTokenInformation(token, TokenPrivileges, Some(buffer.as_mut_ptr() as *mut c_void), return_length, &mut return_length).is_err() {
        return PrivilegeState::NotAssigned;
    }

    let token_privileges = &*(buffer.as_ptr() as *const TOKEN_PRIVILEGES);

    // Search for our privilege
    for i in 0..token_privileges.PrivilegeCount {
        // Manually calculate the privilege pointer for index i
        let privilege_ptr = if i == 0 {
            // First privilege is directly accessible
            &token_privileges.Privileges[0]
        } else {
            // Additional privileges are stored contiguously after the structure
            let base_ptr = buffer.as_ptr() as *const TOKEN_PRIVILEGES;
            let privileges_array_ptr = std::ptr::addr_of!((*base_ptr).Privileges) as *const LUID_AND_ATTRIBUTES;
            &*privileges_array_ptr.add(i as usize)
        };

        if privilege_ptr.Luid.LowPart == privilege_luid.LowPart && privilege_ptr.Luid.HighPart == privilege_luid.HighPart {
            // Fix type comparison issue
            let enabled_flag = TOKEN_PRIVILEGES_ATTRIBUTES(SE_PRIVILEGE_ENABLED.0);
            let zero_flag = TOKEN_PRIVILEGES_ATTRIBUTES(0);

            if (privilege_ptr.Attributes & enabled_flag) != zero_flag {
                return PrivilegeState::AssignedAndEnabled;
            } else {
                return PrivilegeState::AssignedButDisabled;
            }
        }
    }

    PrivilegeState::NotAssigned
}

/// # Safety
///
/// `token` must be an open process-token handle with `TOKEN_ADJUST_PRIVILEGES` access (and `TOKEN_QUERY` if the caller then re-reads the
/// state), open for the duration of the call. This function does not close it.
unsafe fn enable_privilege_in_token(token: HANDLE, privilege_luid: LUID) -> Result<(), &'static str> {
    use windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES;

    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: privilege_luid, Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(SE_PRIVILEGE_ENABLED.0) }],
    };

    let result = AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);

    if result.is_err() {
        return Err("AdjustTokenPrivileges failed");
    }

    // Even if AdjustTokenPrivileges succeeds, check GetLastError
    let last_error = GetLastError();
    match last_error.0 {
        0 => Ok(()), // ERROR_SUCCESS
        1300 => Err("Privilege not held by client (ERROR_NOT_ALL_ASSIGNED)"),
        1314 => Err("Required privilege not held (ERROR_PRIVILEGE_NOT_HELD)"),
        _ => {
            log::warn!("AdjustTokenPrivileges returned unexpected error: {}", last_error.0);
            Err("Failed to enable privilege")
        }
    }
}

fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: `elevation` is a fully initialised local passed directly as the out-param, with the length `size_of::<TOKEN_ELEVATION>()`
    // matching its type — the direct-argument form, so no pointer outlives the call (TODO #66). The token is closed on both paths.
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();

        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut return_length = 0u32;

        let result = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        );

        let _ = CloseHandle(token);

        result.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Alternative privilege enabling that works better on Windows Server
pub fn enable_large_page_privilege_enhanced() -> Result<(), String> {
    // SAFETY: same token discipline as `check_large_page_privilege` — `token` is used only where `OpenProcessToken` succeeded, and closed
    // exactly once on every return path. The `check_privilege_in_token` / `enable_privilege_in_token` calls below meet their documented
    // contract because the token was opened with `TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES`.
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();

        // Open token with maximum permissions
        let token_access = TOKEN_ADJUST_PRIVILEGES
            | TOKEN_QUERY
            | windows::Win32::Security::TOKEN_DUPLICATE
            | windows::Win32::Security::TOKEN_ASSIGN_PRIMARY;

        if OpenProcessToken(process, token_access, &mut token).is_err() {
            return Err("Failed to open process token with full access".to_string());
        }

        let privilege_name = windows::core::w!("SeLockMemoryPrivilege");
        let mut luid = LUID::default();

        if LookupPrivilegeValueW(None, privilege_name, &mut luid).is_err() {
            let _ = CloseHandle(token);
            return Err("Failed to lookup SeLockMemoryPrivilege".to_string());
        }

        // Try to enable with SE_PRIVILEGE_ENABLED_BY_DEFAULT as well
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES(SE_PRIVILEGE_ENABLED.0)
                    | windows::Win32::Security::SE_PRIVILEGE_ENABLED_BY_DEFAULT,
            }],
        };

        let result = AdjustTokenPrivileges(token, false, Some(&privileges), std::mem::size_of::<TOKEN_PRIVILEGES>() as u32, None, None);

        let last_error = GetLastError();
        let _ = CloseHandle(token);

        if result.is_err() {
            return Err(format!("AdjustTokenPrivileges failed with error {}", last_error.0));
        }

        match last_error.0 {
            0 => {
                log::info!("Successfully enabled SeLockMemoryPrivilege");
                Ok(())
            }
            1300 => Err("Some privileges were not assigned (ERROR_NOT_ALL_ASSIGNED)".to_string()),
            1314 => Err("Required privilege not held (ERROR_PRIVILEGE_NOT_HELD)".to_string()),
            _ => Err(format!("Unexpected error after AdjustTokenPrivileges: {}", last_error.0)),
        }
    }
}

// ---- Whom the right reaches (TODO 113) ------------------------------------------------------------------------------------------------

/// How far a grant of the right reaches, which orders the candidates and warns of the broad ones: the account TMR runs as, a group it is
/// in, every account that can sign in to this PC (BUILTIN\Users), every account of a domain (Domain Users), or one of Windows' automatic
/// groups, which every account that signs in a certain way is in (Everyone, Authenticated Users, INTERACTIVE). By SID, never by name:
/// names are localized ("VORDEFINIERT\Administratoren", "Jeder").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    You,
    Group,
    EveryLocalAccount,
    EveryDomainAccount,
    Automatic,
}

impl Reach {
    /// What a broad one reaches, the end of its warning ("⚠️  BROAD, likely not what you want: ..."): `None` for the account and a group
    pub fn warning(self) -> Option<&'static str> {
        match self {
            Reach::You | Reach::Group => None,
            Reach::EveryLocalAccount => Some("every account that can sign in to this PC is in it"),
            Reach::EveryDomainAccount => Some("every account of its domain is in it"),
            Reach::Automatic => Some("Windows puts every account that signs in a certain way in it"),
        }
    }

    /// Warned of, and a pick of it asked again (`BROAD_RISK`)
    pub fn is_broad(self) -> bool {
        self.warning().is_some()
    }
}

/// What a broad grant risks, which the second question after a broad pick states: Microsoft's 'Lock pages in memory' page names it a
/// denial of service, "could leave little or no RAM for other processes"
pub const BROAD_RISK: &str = "any program of any of these accounts can then lock RAM away from Windows";

/// Windows' automatic groups, by SID (the TODO 113 review's list): Everyone (S-1-1-0); S-1-2-*, LOCAL and CONSOLE LOGON; NT AUTHORITY's
/// DIALUP, NETWORK, BATCH, INTERACTIVE, SERVICE, ANONYMOUS LOGON, PROXY, ENTERPRISE DOMAIN CONTROLLERS, Authenticated Users, RESTRICTED,
/// TERMINAL SERVER USER, REMOTE INTERACTIVE LOGON, This Organization, Local account and its member-of-Administrators part (S-1-5-1 to 15,
/// 113, 114); NTLM, SChannel and Digest Authentication (S-1-5-64-*); This Organization Certificate (S-1-5-65-1); and the asserted
/// identities (S-1-18-*)
pub(crate) fn is_automatic(sid: &Sid) -> bool {
    matches!(
        (sid.authority(), sid.sub_authorities()),
        (1, [0])
            | (2, [_, ..])
            | (18, [_, ..])
            | (5, [1 | 2 | 3 | 4 | 6 | 7 | 8 | 9 | 11 | 12 | 13 | 14 | 15 | 113 | 114])
            | (5, [64, _, ..])
            | (5, [65, 1])
    )
}

/// Service accounts, which fit both `User:` and `Group:` (Windows types them well-known groups) and aren't broad: SYSTEM, LOCAL SERVICE
/// and NETWORK SERVICE (S-1-5-18, 19, 20), and NT SERVICE\*, IIS APPPOOL\* and NT VIRTUAL MACHINE\* (S-1-5-80-*, 82-*, 83-*)
pub(crate) fn is_service_account(sid: &Sid) -> bool {
    matches!((sid.authority(), sid.sub_authorities()), (5, [18..=20]) | (5, [80 | 82 | 83, _, ..]))
}

/// BUILTIN\Administrators, which is enabled in a token only when it runs as administrator, and which a grant needs
fn is_administrators(sid: &Sid) -> bool {
    sid.authority() == 5 && sid.sub_authorities() == [32, 544]
}

/// What a SID is that the right is never offered to, `None` for the rest: a sign-in's own (the logon SID, S-1-5-5-*), an integrity label
/// (S-1-16-*), or an app's (S-1-15-*: its package and its capabilities)
pub(crate) fn never_offered(sid: &Sid) -> Option<&'static str> {
    match (sid.authority(), sid.sub_authorities()) {
        (5, [5, ..]) => Some("a sign-in's own SID, which lasts one sign-in"),
        (16, _) => Some("an integrity label"),
        (15, _) => Some("an app's SID"),
        _ => None,
    }
}

/// How far a grant to `sid` reaches, by its SID alone: the automatic groups, BUILTIN\Users (S-1-5-32-545), a domain's Domain Users
/// (S-1-5-21-a-b-c-513, "None" on a PC in no domain), else a group's (an account's too: its reach is the account's own)
pub(crate) fn breadth(sid: &Sid) -> Reach {
    match (sid.authority(), sid.sub_authorities()) {
        _ if is_automatic(sid) => Reach::Automatic,
        (5, [32, 545]) => Reach::EveryLocalAccount,
        (5, [21, _, _, _, 513]) => Reach::EveryDomainAccount,
        _ => Reach::Group,
    }
}

/// Whether a group of this token, `sid` with its `attributes` (`SE_GROUP_*`) and its type as Windows named it (`kind`, `None` when it
/// can't), is offered the right, and how far that reaches (`breadth`). Left out: the logon SID, the integrity labels, a group this token
/// can't use (deny-only, which still counts as holding it: `held_by`), app SIDs, and what isn't a group (a label, a logon session, a
/// domain, a computer, a deleted account, a user among the groups). A group Windows can't name is kept: it can still be granted by SID.
pub(crate) fn reach(sid: &Sid, kind: Option<SID_NAME_USE>, attributes: u32) -> Option<Reach> {
    let logon_id = SE_GROUP_LOGON_ID as u32;
    if attributes & logon_id == logon_id || attributes & (SE_GROUP_INTEGRITY | SE_GROUP_USE_FOR_DENY_ONLY) as u32 != 0 {
        return None;
    }
    let not_a_group =
        [SidTypeLabel, SidTypeLogonSession, SidTypeDomain, SidTypeComputer, SidTypeDeletedAccount, SidTypeInvalid, SidTypeUser];
    if never_offered(sid).is_some() || kind.is_some_and(|kind| not_a_group.contains(&kind)) {
        return None;
    }
    Some(breadth(sid))
}

/// Whom a session offers the right to (`LargePageQuestion::Grant`): an account or a group, Windows' name for it (`DOMAIN\name`, its
/// `S-1-...` string when Windows can't name it) and how far a grant reaches. Its SID is the engine's: the session grants from its own
/// copy of the question, so a frontend can't change whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub reach: Reach,
    sid: Sid,
}

impl Candidate {
    pub fn new(name: impl Into<String>, reach: Reach, sid: Sid) -> Self {
        Candidate { name: name.into(), reach, sid }
    }

    pub(crate) fn sid(&self) -> &Sid {
        &self.sid
    }
}

/// The candidates in the order they are offered: `you` first, the default; then the groups, a real group before every local account
/// (Users), before every account of a domain, before the automatic groups, each kind A-Z. Sorted rather than in the token's order, so the
/// list (and its goldens) stay put. A group that is the account itself, or one listed twice, is left out.
pub(crate) fn order(you: Candidate, groups: Vec<Candidate>) -> Vec<Candidate> {
    let mut groups: Vec<Candidate> = groups.into_iter().filter(|group| group.sid != you.sid).collect();
    groups.sort_by_cached_key(|group| (group.reach, group.name.to_lowercase()));
    groups.dedup_by(|a, b| a.sid == b.sid);
    std::iter::once(you).chain(groups).collect()
}

/// Which of this token's SIDs the policy gives the right to (`held_by`)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The account itself
    You,
    /// A group it is in, by its name in the list, or when the list left it out by Windows' name for it (its `S-1-...` string if none)
    Group(String),
}

/// Whether the right is the token's to have, from the policy's `holders`: given to `you` (preferred), or to one of `groups` (in the
/// order offered, first), or to another SID of `mine` (the token's every SID, deny-only ones too: a sign-in's privileges come from its
/// memberships), named from `names`. Holders that aren't the token's don't count: a SQL Server service account holding it made TMR ask
/// for a restart forever.
pub(crate) fn held_by(holders: &[Sid], you: &Candidate, groups: &[Candidate], mine: &[Sid], names: &HashMap<Sid, Named>) -> Option<Holder> {
    if holders.contains(&you.sid) {
        return Some(Holder::You);
    }
    if let Some(group) = groups.iter().find(|group| holders.contains(&group.sid)) {
        return Some(Holder::Group(group.name.clone()));
    }
    let named = |sid: &Sid| names.get(sid).map_or_else(|| sid.to_string(), |named| named.name.clone());
    mine.iter().find(|sid| **sid != you.sid && holders.contains(sid)).map(|sid| Holder::Group(named(sid)))
}

/// Whether this token runs as administrator, which granting the right needs: BUILTIN\Administrators enabled in it. TokenIsElevated isn't
/// enough: an elevated Backup Operator would be offered a grant that fails.
pub(crate) fn administrator(token: &Token) -> bool {
    token.groups.iter().any(|(sid, attributes)| {
        is_administrators(sid) && attributes & SE_GROUP_ENABLED as u32 != 0 && attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 == 0
    })
}

/// An administrator under UAC, run as administrator: the elevated half of a split token, BUILTIN\Administrators enabled. The right then
/// reaches only programs run as administrator (Microsoft's "Access Token Changes": a filtered token keeps five privileges, not this
/// one), whichever account or group it was granted to.
pub(crate) fn uac_elevated(token: &Token) -> bool {
    token.elevation == Elevation::Full && administrator(token)
}

/// An administrator under UAC, not run as administrator: the filtered half of a split token, BUILTIN\Administrators deny-only
pub(crate) fn uac_filtered(token: &Token) -> bool {
    let deny_only = SE_GROUP_USE_FOR_DENY_ONLY as u32;
    token.elevation == Elevation::Limited
        && token.groups.iter().any(|(sid, attributes)| is_administrators(sid) && attributes & deny_only != 0)
}

/// The account this token runs as, as every message names it: Windows' `DOMAIN\name`, or its `S-1-...` string when it has none
pub(crate) fn account_of(token: &Token) -> String {
    account_name(&token.user).map_or_else(|_| token.user.to_string(), |named| named.name)
}

/// What a session asks about large pages before its run (`Frontend::ask_large_pages`), when this token lacks the right they need,
/// SeLockMemoryPrivilege ('Lock pages in memory'), and the run's page range allows them (maxpage= large or huge). A right reaches a
/// program at its account's next sign-in: no restart is needed. The owner's rule (2026-11-03): a run takes 4 KiB pages only when the
/// user says so (`LargePageAnswer::Regular`) or gave `minpage=regular maxpage=regular` (then nothing is asked); no answer quits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LargePageQuestion {
    /// The policy gives it to `account`, the account TMR runs as, or to `via`, a group it is in, but this sign-in began before it did. A
    /// new sign-in brings it, or this run takes 4 KiB pages.
    SignIn { account: String, via: Option<String>, uac: bool, domain: bool },
    /// Neither the account nor a group it is in holds it, and TMR, run as administrator, can grant it: to whom, `candidates[0]` the
    /// account itself (the default), then its groups, the broad ones last (`Reach`). Granted, it takes a new sign-in; or this run takes
    /// 4 KiB pages, or TMR quits, granting nothing.
    Grant { candidates: Vec<Candidate>, uac: bool, domain: bool },
    /// TMR can't grant it, not run as administrator (or elevated without the Administrators group): this run takes 4 KiB pages, or TMR
    /// quits, to be started as administrator. `uac`: an administrator under UAC not run as one, whom the right reaches only in programs
    /// run as administrator.
    NeedsAdministrator { account: String, uac: bool },
}

/// What the large-page question for an administrator reads (`question_for`): the account and its groups as candidates (`reach`), every
/// SID of the token, Windows' names for them and for the holders, the policy's holders, whether it is an administrator under UAC
/// (`uac_elevated`), and whether this PC is in a domain
pub(crate) struct Standing {
    pub(crate) you: Candidate,
    pub(crate) groups: Vec<Candidate>,
    pub(crate) mine: Vec<Sid>,
    pub(crate) names: HashMap<Sid, Named>,
    pub(crate) holders: Result<Vec<Sid>, String>,
    pub(crate) uac: bool,
    pub(crate) domain: bool,
}

/// The large-page question this process has for its user (`session::run`), if any: none when the run's page range allows nothing above
/// 4 KiB (`wants_large`: maxpage= large or huge), and none when its token has the right (enabled or not). Reads the token, and the
/// security policy when TMR runs as administrator; changes nothing.
pub fn large_page_question(wants_large: bool) -> Option<LargePageQuestion> {
    if !wants_large || check_privilege_state() != PrivilegeState::NotAssigned {
        return None;
    }
    let token = match Token::read() {
        Ok(token) => token,
        Err(e) => {
            log::warn!("This process's token couldn't be read ({e}): no large-page question");
            return None;
        }
    };
    question_for(false, wants_large, administrator(&token), || (account_of(&token), uac_filtered(&token)), || standing(&token))
}

/// `large_page_question`'s choice, from whether this token has the right (`in_token`, enabled or not), whether the run's page range
/// allows large pages (`wants_large`) and whether TMR runs as `administrator`: none with the right, or with 4 KiB pages the most the run
/// takes; TMR not run as administrator, 4 KiB pages or quit (the account and `uac` from `unelevated`); run as administrator, the sign-in
/// when the policy gives the right to the account or one of its groups, else the grant (`standing`, read only then). A policy that can't
/// be read offers the grant, which then fails as the read did, saying why.
pub(crate) fn question_for(
    in_token: bool, wants_large: bool, administrator: bool, unelevated: impl FnOnce() -> (String, bool),
    standing: impl FnOnce() -> Standing,
) -> Option<LargePageQuestion> {
    if in_token || !wants_large {
        return None;
    }
    if !administrator {
        let (account, uac) = unelevated();
        return Some(LargePageQuestion::NeedsAdministrator { account, uac });
    }
    let Standing { you, groups, mine, names, holders, uac, domain } = standing();
    let holders = holders.unwrap_or_else(|e| {
        log::warn!("The security policy couldn't be read ({e}): the grant is offered");
        Vec::new()
    });
    let candidates = order(you, groups);
    let account = candidates[0].name.clone();
    Some(match held_by(&holders, &candidates[0], &candidates[1..], &mine, &names) {
        Some(Holder::You) => LargePageQuestion::SignIn { account, via: None, uac, domain },
        Some(Holder::Group(group)) => LargePageQuestion::SignIn { account, via: Some(group), uac, domain },
        None => LargePageQuestion::Grant { candidates, uac, domain },
    })
}

/// The names of `sids` (`Policy::names`, one call), by SID: none when the policy couldn't name them
fn names_of(policy: &Policy, sids: &[Sid]) -> HashMap<Sid, Named> {
    let names = policy.names(sids).unwrap_or_else(|e| {
        log::warn!("Windows couldn't name this process's groups ({e}): they are listed by SID");
        vec![None; sids.len()]
    });
    sids.iter().cloned().zip(names).filter_map(|(sid, named)| Some((sid, named?))).collect()
}

/// The account, its groups as candidates, and their names, with `holders`' too (`names_of`, `policy` the one to ask, none when it couldn't
/// be opened, and then only the account is named, `LookupAccountSidW`)
pub(crate) fn who(token: &Token, policy: Option<&Policy>, holders: &[Sid]) -> (Candidate, Vec<Candidate>, HashMap<Sid, Named>) {
    let mut asked: Vec<Sid> = std::iter::once(token.user.clone()).chain(token.groups.iter().map(|(sid, _)| sid.clone())).collect();
    let others: Vec<Sid> = holders.iter().filter(|holder| !asked.contains(holder)).cloned().collect();
    asked.extend(others);
    let names = policy.map(|policy| names_of(policy, &asked)).unwrap_or_default();
    let you = names.get(&token.user).map_or_else(|| account_of(token), |named| named.name.clone());
    let groups = token
        .groups
        .iter()
        .filter_map(|(sid, attributes)| {
            let named = names.get(sid);
            let reach = reach(sid, named.map(|named| named.kind), *attributes)?;
            Some(Candidate::new(named.map_or_else(|| sid.to_string(), |named| named.name.clone()), reach, sid.clone()))
        })
        .collect();
    (Candidate::new(you, Reach::You, token.user.clone()), groups, names)
}

/// `holders` as the log names them: each by its name and SID, so a holder that isn't this user shows ("NT SERVICE\MSSQLSERVER")
pub(crate) fn describe_holders(holders: &[Sid], names: &HashMap<Sid, Named>) -> String {
    if holders.is_empty() {
        return "no account or group".to_string();
    }
    let named = |sid: &Sid| names.get(sid).map_or_else(|| sid.to_string(), |named| format!("{} ({sid})", named.name));
    holders.iter().map(named).collect::<Vec<_>>().join(", ")
}

/// What an administrator's question reads: the policy's holders and the names of every SID, on one handle opened to read
fn standing(token: &Token) -> Standing {
    let mine: Vec<Sid> = std::iter::once(token.user.clone()).chain(token.groups.iter().map(|(sid, _)| sid.clone())).collect();
    let policy = Policy::open(Access::Read);
    let holders = policy.as_ref().map_err(String::clone).and_then(Policy::holders);
    let (you, groups, names) = who(token, policy.as_ref().ok(), holders.as_deref().unwrap_or_default());
    if let Ok(holders) = &holders {
        log::info!("The security policy gives 'Lock pages in memory' to {}", describe_holders(holders, &names));
    }
    Standing { you, groups, mine, names, holders, uac: uac_elevated(token), domain: in_a_domain() }
}

/// Grants `candidate` the right large pages need, in the machine's security policy (`LsaAddAccountRights`, by its SID). Called only once
/// the user has picked it in the question that offered it (`session::run`); `--setup-large-pages` grants through its own flow
/// (`large_page_setup`), the command being the request. The account or group keeps the right until it is removed, and it takes effect at
/// the next sign-in.
pub fn grant_large_pages(candidate: &Candidate) -> Result<(), String> {
    log::info!("Granting 'Lock pages in memory' to {} ({})", candidate.name, candidate.sid);
    Policy::open(Access::Grant)?.add(&candidate.sid)
}

/// The large-page check's lines: the right's state in this token, enabling it there when it is disabled, and a test allocation; without
/// it, who can grant it and how. It never grants the right (`grant_large_pages` does, asked). The allocator calls it too, before a
/// large-page request (`WindowsBackend::allocate_with_virtualalloc2`).
pub fn enable_large_pages() -> Result<String, String> {
    let mut messages = Vec::new();

    match check_privilege_state() {
        PrivilegeState::NotAssigned => {
            messages.push("❌ SeLockMemoryPrivilege: NOT in current session token".to_string());

            let token = Token::read();
            match &token {
                // A session asks first (`large_page_question`), and `--setup-large-pages` grants it at once
                Ok(token) if administrator(token) => messages.push("✅ Running as Administrator: TMR can grant it".to_string()),
                _ => {
                    if is_elevated() {
                        messages.push("❌ Running elevated, but not as an administrator: only an administrator can grant it".to_string());
                    } else {
                        messages.push("❌ Not running as Administrator".to_string());
                    }
                    messages.push("".to_string());
                    messages.push("🔧 SOLUTIONS (try in order):".to_string());
                    messages.push(
                        "1. Run TMR as administrator: it uses large pages once granted, and asks to grant them first \
                         (tmr.exe --setup-large-pages grants them at once)"
                            .to_string(),
                    );
                    if token.as_ref().is_ok_and(uac_filtered) {
                        messages.push(
                            "   You sign in as an administrator under UAC: TMR gets this right only when run as administrator".to_string(),
                        );
                    }
                    messages.push("2. If it was just granted: sign out of Windows and back in, then run TMR again".to_string());
                    messages.push(
                        "3. Manual setup via gpedit.msc (User Rights Assignment, 'Lock pages in memory'), then sign out and back in"
                            .to_string(),
                    );
                }
            }
        }
        PrivilegeState::AssignedButDisabled => {
            messages.push("⚠️  SeLockMemoryPrivilege: ASSIGNED but DISABLED".to_string());

            match enable_large_page_privilege_enhanced() {
                Ok(()) => {
                    messages.push("✅ Successfully enabled SeLockMemoryPrivilege".to_string());

                    match test_large_page_allocation() {
                        Ok(()) => {
                            messages.push("✅ Large page test allocation successful".to_string());
                            return Ok(messages.join("\n"));
                        }
                        Err(e) => {
                            messages.push(format!("❌ Large page test failed: {}", e));
                        }
                    }
                }
                Err(e) => {
                    messages.push(format!("❌ Failed to enable privilege: {}", e));
                }
            }
        }
        PrivilegeState::AssignedAndEnabled => {
            messages.push("✅ SeLockMemoryPrivilege: ASSIGNED and ENABLED".to_string());

            match test_large_page_allocation() {
                Ok(()) => {
                    messages.push("✅ Large page test allocation successful".to_string());
                    return Ok(messages.join("\n"));
                }
                Err(e) => {
                    messages.push(format!("❌ Large page test failed: {}", e));
                }
            }
        }
    }

    Ok(messages.join("\n"))
}

/// Check the current state of the SeLockMemoryPrivilege
fn check_privilege_state() -> PrivilegeState {
    // SAFETY: `token` is opened with `TOKEN_QUERY` — the access
    // `check_privilege_in_token` documents — stays open across that call, and is closed once on each return path.
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();

        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            return PrivilegeState::NotAssigned;
        }

        let privilege_name = windows::core::w!("SeLockMemoryPrivilege");
        let mut luid = LUID::default();

        if LookupPrivilegeValueW(None, privilege_name, &mut luid).is_err() {
            let _ = CloseHandle(token);
            return PrivilegeState::NotAssigned;
        }

        let result = check_privilege_in_token(token, luid);
        let _ = CloseHandle(token);

        result
    }
}

/// Whether this token has the right, enabled or not: `--setup-large-pages`' "this sign-in has it already"
pub(crate) fn in_token() -> bool {
    check_privilege_state() != PrivilegeState::NotAssigned
}

/// Test a small large page allocation
fn test_large_page_allocation() -> Result<(), String> {
    // SAFETY: a self-contained 2 MB probe. The returned pointer is never dereferenced — only null-checked and, when non-null, released with
    // the `MEM_RELEASE` form `VirtualFree` requires (size 0, base address as returned).
    unsafe {
        let test_size = 2 * 1024 * 1024; // 2MB
        let ptr = VirtualAlloc(Some(null_mut()), test_size, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES, PAGE_READWRITE);

        if ptr.is_null() {
            let err = GetLastError();
            let error_msg = format_win32_error(err);
            Err(format!("Large page allocation failed: {} ({})", error_msg, err.0))
        } else {
            // Clean up test allocation
            let _ = VirtualFree(ptr, 0, MEM_RELEASE);
            Ok(())
        }
    }
}

/// The large-page check's report (`Setup::LargePageDiagnostic`): elevated or not, the account, and `enable_large_pages`' lines. It never
/// grants the right: a session asks first (`large_page_question`), and `--setup-large-pages` grants it after this.
pub fn diagnose_large_pages() -> String {
    let mut report = String::new();

    report.push_str("🔧 TMR Large Page Auto-Setup starting\n\n");

    // Show current system info
    report.push_str("System Information:\n");
    if is_elevated() {
        report.push_str("✅ Running with Administrator privileges\n");
    } else {
        report.push_str("❌ Not running as Administrator\n");
    }

    // The account, from the token: not %USERDOMAIN%\%USERNAME%, which whoever started TMR sets
    match Token::read() {
        Ok(token) => report.push_str(&format!("👤 User: {}\n", account_of(&token))),
        Err(e) => report.push_str(&format!("👤 User: unknown ({e})\n")),
    }

    report.push('\n');

    // Enable it in this token, and try it
    match enable_large_pages() {
        Ok(setup_result) => {
            report.push_str("Auto-Setup Results:\n");
            report.push_str(&setup_result);
            report.push('\n');
        }
        Err(setup_error) => {
            report.push_str("Auto-Setup Failed:\n");
            report.push_str(&setup_error);
            report.push('\n');
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use windows::Win32::Security::{SidTypeAlias, SidTypeGroup, SidTypeWellKnownGroup};

    use super::*;

    fn sid(text: &str) -> Sid {
        text.parse().unwrap_or_else(|()| panic!("{text} is a SID"))
    }

    fn candidate(name: &str, reach: Reach, text: &str) -> Candidate {
        Candidate::new(name, reach, sid(text))
    }

    const ENABLED: u32 = (SE_GROUP_ENABLED | windows::Win32::System::SystemServices::SE_GROUP_MANDATORY) as u32;

    /// TODO 113: which of a token's groups are offered the right, and how far each reaches, by SID: the attributes leave out the logon SID,
    /// the labels and deny-only groups; the types what isn't a group; Users, Domain Users and the automatic groups are broad, service
    /// accounts aren't; a group Windows can't name is kept
    #[test]
    #[rustfmt::skip]
    fn a_tokens_groups_are_offered_by_sid() {
        let logon = SE_GROUP_LOGON_ID as u32 | ENABLED;
        let integrity = (SE_GROUP_INTEGRITY | windows::Win32::System::SystemServices::SE_GROUP_INTEGRITY_ENABLED) as u32;
        let deny_only = SE_GROUP_USE_FOR_DENY_ONLY as u32;
        for (text, kind, attributes, expected) in [
            ("S-1-5-32-544", Some(SidTypeAlias), ENABLED, Some(Reach::Group)),
            ("S-1-5-32-545", Some(SidTypeAlias), ENABLED, Some(Reach::EveryLocalAccount)),
            ("S-1-5-21-1-2-3-513", Some(SidTypeGroup), ENABLED, Some(Reach::EveryDomainAccount)),
            ("S-1-5-21-1-2-3-512", Some(SidTypeGroup), ENABLED, Some(Reach::Group)),
            ("S-1-5-21-1-2-3-1105", Some(SidTypeAlias), ENABLED, Some(Reach::Group)),
            ("S-1-1-0", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-11", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-4", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-2-1", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-114", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-64-10", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-65-1", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-18-1", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Automatic)),
            ("S-1-5-80-1-2-3-4-5", Some(SidTypeWellKnownGroup), ENABLED, Some(Reach::Group)),
            ("S-1-5-21-9-9-9-1234", None, ENABLED, Some(Reach::Group)),
            ("S-1-5-5-0-123456", Some(SidTypeLogonSession), logon, None),
            ("S-1-5-5-0-123456", None, ENABLED, None),
            ("S-1-16-12288", Some(SidTypeLabel), integrity, None),
            ("S-1-16-12288", Some(SidTypeLabel), 0, None),
            ("S-1-5-32-544", Some(SidTypeAlias), deny_only, None),
            ("S-1-15-3-1024-1", None, ENABLED, None),
            ("S-1-5-21-1-2-3", Some(SidTypeDomain), ENABLED, None),
            ("S-1-5-21-1-2-3-1001", Some(SidTypeComputer), ENABLED, None),
            ("S-1-5-21-1-2-3-1002", Some(SidTypeDeletedAccount), ENABLED, None),
            ("S-1-5-21-1-2-3-1003", Some(SidTypeUser), ENABLED, None),
            ("S-1-5-21-1-2-3-1004", Some(SidTypeInvalid), ENABLED, None),
        ] {
            assert_eq!(reach(&sid(text), kind, attributes), expected, "{text} {kind:?} {attributes:#x}");
        }
        for service in ["S-1-5-18", "S-1-5-19", "S-1-5-20", "S-1-5-80-1-2-3-4-5", "S-1-5-82-1-2-3-4-5", "S-1-5-83-1-2-3-4-5"] {
            assert!(is_service_account(&sid(service)) && !breadth(&sid(service)).is_broad(), "{service}");
        }
        for not in ["S-1-5-32-544", "S-1-5-21-1-2-3-500", "S-1-5-80", "S-1-5-17"] {
            assert!(!is_service_account(&sid(not)), "{not}");
        }
        assert!(!is_automatic(&sid("S-1-2")) && !is_automatic(&sid("S-1-5-64")) && !is_automatic(&sid("S-1-5-65-2")));
    }

    /// TODO 113: the warnings, one phrase each, and only on the broad reaches (the review's wording for Users: every account that can
    /// sign in is in it, Microsoft, Entra and domain accounts too)
    #[test]
    fn only_the_broad_reaches_are_warned_of() {
        assert_eq!(Reach::You.warning(), None);
        assert_eq!(Reach::Group.warning(), None);
        assert_eq!(Reach::EveryLocalAccount.warning(), Some("every account that can sign in to this PC is in it"));
        assert_eq!(Reach::EveryDomainAccount.warning(), Some("every account of its domain is in it"));
        assert_eq!(Reach::Automatic.warning(), Some("Windows puts every account that signs in a certain way in it"));
        assert!(Reach::Automatic.is_broad() && !Reach::Group.is_broad());
    }

    /// TODO 113: the account first, then the real groups A-Z whatever their case, then Users, then Domain Users ("None"), then the
    /// automatic groups A-Z; a group that is the account itself, and a group listed twice, once
    #[test]
    fn the_candidates_come_in_the_order_offered() {
        let you = candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001");
        let groups = vec![
            candidate("Everyone", Reach::Automatic, "S-1-1-0"),
            candidate(r"PC\None", Reach::EveryDomainAccount, "S-1-5-21-1-2-3-513"),
            candidate(r"BUILTIN\Users", Reach::EveryLocalAccount, "S-1-5-32-545"),
            candidate(r"PC\testers", Reach::Group, "S-1-5-21-1-2-3-1010"),
            candidate(r"NT AUTHORITY\Authenticated Users", Reach::Automatic, "S-1-5-11"),
            candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"),
            candidate(r"PC\owner", Reach::Group, "S-1-5-21-1-2-3-1001"),
            candidate(r"CORP\Remote Desktop Users", Reach::Group, "S-1-5-32-555"),
            candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"),
        ];
        let names: Vec<String> = order(you, groups).into_iter().map(|c| c.name).collect();
        assert_eq!(names, [
            r"PC\owner", r"BUILTIN\Administrators", r"CORP\Remote Desktop Users", r"PC\testers", r"BUILTIN\Users", r"PC\None", r"Everyone",
            r"NT AUTHORITY\Authenticated Users",
        ]);
    }

    /// TODO 113: the right is the token's to have when the policy gives it to the account (preferred) or to a group of the token, a group
    /// left out of the list too (by Windows' name for it, its SID when it has none); another account holding it doesn't count, the SQL
    /// Server case that asked for a restart forever
    #[test]
    fn the_right_is_held_only_through_the_tokens_own_sids() {
        let you = candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001");
        let groups =
            [candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544"), candidate("Everyone", Reach::Automatic, "S-1-1-0")];
        let mine = ["S-1-5-21-1-2-3-1001", "S-1-5-32-544", "S-1-1-0", "S-1-5-32-551", "S-1-5-21-1-2-3-1107"].map(sid);
        let names = HashMap::from([(sid("S-1-5-32-551"), Named { name: r"BUILTIN\Backup Operators".to_string(), kind: SidTypeAlias })]);
        let held = |holders: &[&str]| held_by(&holders.iter().map(|h| sid(h)).collect::<Vec<_>>(), &you, &groups, &mine, &names);
        assert_eq!(held(&[]), None, "nobody");
        assert_eq!(held(&["S-1-5-21-1-2-3-1001"]), Some(Holder::You));
        assert_eq!(held(&["S-1-5-32-544"]), Some(Holder::Group(r"BUILTIN\Administrators".to_string())));
        assert_eq!(held(&["S-1-5-32-544", "S-1-5-21-1-2-3-1001"]), Some(Holder::You), "both: the account");
        assert_eq!(held(&["S-1-5-80-1-2-3-4-5"]), None, r"NT SERVICE\MSSQLSERVER, not this token's");
        assert_eq!(held(&["S-1-5-32-551"]), Some(Holder::Group(r"BUILTIN\Backup Operators".to_string())), "left out of the list: named");
        assert_eq!(held(&["S-1-5-21-1-2-3-1107"]), Some(Holder::Group("S-1-5-21-1-2-3-1107".to_string())), "and unnamed: its SID");
        assert_eq!(held(&["S-1-1-0", "S-1-5-32-544"]), Some(Holder::Group(r"BUILTIN\Administrators".to_string())), "the first offered");
    }

    /// TODO 113: the question, from stand-ins (decision 5a): none with the right in the token, or with maxpage=regular (4 KiB pages the
    /// most the run takes), nothing read; not run as administrator, 4 KiB pages or quit whatever minpage is, the policy unread; run as
    /// administrator, the sign-in when the account or a group holds it, else the grant, the account first, also when the policy can't be
    /// read; `uac` and `domain` carried. No test reads or changes the policy.
    #[test]
    fn the_question_follows_the_token_and_the_policy() {
        let you = || candidate(r"PC\owner", Reach::You, "S-1-5-21-1-2-3-1001");
        let admins = || candidate(r"BUILTIN\Administrators", Reach::Group, "S-1-5-32-544");
        let ask = |in_token, wants_large, administrator, holders: Result<Vec<&str>, String>| {
            let (unelevated_read, standing_read) = (Cell::new(false), Cell::new(false));
            let question = question_for(
                in_token,
                wants_large,
                administrator,
                || {
                    unelevated_read.set(true);
                    (r"PC\owner".to_string(), true)
                },
                || {
                    standing_read.set(true);
                    Standing {
                        you: you(),
                        groups: vec![candidate("Everyone", Reach::Automatic, "S-1-1-0"), admins()],
                        mine: ["S-1-5-21-1-2-3-1001", "S-1-5-32-544", "S-1-1-0"].map(sid).to_vec(),
                        names: HashMap::new(),
                        holders: holders.map(|holders| holders.iter().map(|h| sid(h)).collect()),
                        uac: true,
                        domain: false,
                    }
                },
            );
            (question, unelevated_read.get(), standing_read.get())
        };
        let grant = || {
            Some(LargePageQuestion::Grant {
                candidates: vec![you(), admins(), candidate("Everyone", Reach::Automatic, "S-1-1-0")],
                uac: true,
                domain: false,
            })
        };
        let sign_in = |via: Option<&str>| {
            Some(LargePageQuestion::SignIn { account: r"PC\owner".to_string(), via: via.map(str::to_string), uac: true, domain: false })
        };
        let needs = Some(LargePageQuestion::NeedsAdministrator { account: r"PC\owner".to_string(), uac: true });
        assert_eq!(ask(true, true, true, Ok(vec![])), (None, false, false), "the right in the token");
        assert_eq!(ask(true, true, false, Ok(vec![])), (None, false, false), "the right in the token, not elevated");
        assert_eq!(ask(false, false, false, Ok(vec![])), (None, false, false), "maxpage=regular, not an administrator: no question");
        assert_eq!(ask(false, false, true, Ok(vec![])), (None, false, false), "maxpage=regular, an administrator: no question");
        assert_eq!(
            ask(false, true, false, Ok(vec![])),
            (needs, true, false),
            "not an administrator: 4 KiB pages or quit, the policy unread"
        );
        assert_eq!(ask(false, true, true, Ok(vec!["S-1-5-21-1-2-3-1001"])), (sign_in(None), false, true), "held by the account");
        assert_eq!(ask(false, true, true, Ok(vec!["S-1-5-32-544"])), (sign_in(Some(r"BUILTIN\Administrators")), false, true), "by a group");
        assert_eq!(ask(false, true, true, Ok(vec!["S-1-5-80-1-2-3-4-5"])), (grant(), false, true), "by another account only");
        assert_eq!(ask(false, true, true, Ok(vec![])), (grant(), false, true), "by nobody");
        assert_eq!(ask(false, true, true, Err("LsaOpenPolicy: Access is denied.".to_string())), (grant(), false, true), "unreadable");
    }

    /// TODO 113: who may grant it, and which UAC half this token is, from stand-in tokens: Administrators enabled is an administrator;
    /// deny-only (UAC's filtered token) or absent isn't; the elevated half of a split token with it enabled is `uac_elevated`, the
    /// filtered half with it deny-only `uac_filtered`
    #[test]
    fn an_administrator_is_the_administrators_group_enabled() {
        let token = |elevation, attributes: Option<u32>| Token {
            user: sid("S-1-5-21-1-2-3-1001"),
            groups: [Some(("S-1-5-32-545", ENABLED)), attributes.map(|a| ("S-1-5-32-544", a))]
                .into_iter()
                .flatten()
                .map(|(text, attributes)| (sid(text), attributes))
                .collect(),
            elevation,
        };
        let deny_only = Some(SE_GROUP_USE_FOR_DENY_ONLY as u32);
        let full = token(Elevation::Full, Some(ENABLED));
        assert!(administrator(&full) && uac_elevated(&full) && !uac_filtered(&full), "elevated, under UAC");
        let filtered = token(Elevation::Limited, deny_only);
        assert!(!administrator(&filtered) && !uac_elevated(&filtered) && uac_filtered(&filtered), "an administrator, not elevated");
        let uac_off = token(Elevation::Default, Some(ENABLED));
        assert!(administrator(&uac_off) && !uac_elevated(&uac_off) && !uac_filtered(&uac_off), "UAC off: every run elevated");
        let standard = token(Elevation::Default, None);
        assert!(!administrator(&standard) && !uac_elevated(&standard) && !uac_filtered(&standard), "a standard user");
        let backup_operator = token(Elevation::Full, None);
        assert!(!administrator(&backup_operator), "elevated without Administrators: a grant would fail");
    }

    /// TODO 113, run by name, elevated for the policy part: this machine's token and policy, read and changed nowhere (no grant, no
    /// removal, no handle with GRANT access). The candidates start with the account, and none is a logon SID or a label; each holder
    /// the policy lists has the right when asked by its own SID (`LsaEnumerateAccountRights`), so the enumeration was asked for the
    /// right, not every account.
    #[test]
    #[ignore = "reads this machine's token and security policy, changes nothing: run it by name, elevated for the policy part"]
    fn the_token_and_the_policy_read_back() {
        let token = Token::read().expect("this process's token");
        println!("account {} ({}), {:?}, administrator {}", account_of(&token), token.user, token.elevation, administrator(&token));
        let policy = administrator(&token).then(|| Policy::open(Access::Read).expect("the policy, to read"));
        let holders = policy.as_ref().map(|policy| policy.holders().expect("its holders")).unwrap_or_default();
        let (you, groups, names) = who(&token, policy.as_ref(), &holders);
        let candidates = order(you, groups);
        assert_eq!(candidates[0].reach, Reach::You);
        for candidate in &candidates {
            assert!(never_offered(candidate.sid()).is_none(), "{} ({}) offered", candidate.name, candidate.sid());
            println!("  {:?} {} ({})", candidate.reach, candidate.name, candidate.sid());
        }
        let Some(policy) = policy else {
            println!("not run as administrator: the policy left unread");
            return;
        };
        let unique: std::collections::HashSet<&Sid> = holders.iter().collect();
        assert_eq!(unique.len(), holders.len(), "no holder twice");
        println!("held by {}", describe_holders(&holders, &names));
        for holder in &holders {
            let rights = policy.rights(holder).expect("its rights");
            assert!(rights.iter().any(|right| right == "SeLockMemoryPrivilege"), "{holder}: {rights:?}");
        }
        println!("in a domain: {}", in_a_domain());
    }
}
