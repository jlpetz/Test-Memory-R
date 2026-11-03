//! The Windows calls behind the large-page right (TODO 113): the process token's account and groups, and the security policy through
//! LSA, in process and by SID. They replaced `secedit`, whose export of the whole policy went through a file in the user's %TEMP% that an
//! unelevated program could swap before the elevated `/configure` re-applied it, and whose account name came from the environment.
//!
//! The policy is opened with the least access each call needs (`Access`): reading it and naming SIDs, or that and granting. Every LSA
//! buffer is freed on every path (`LsaBuffer`), and every SID is copied out of the buffer it came in before that buffer goes: no PSID
//! leaves this file but `Sid::as_psid`'s, which borrows a `Sid` for one call.

// As `privileges.rs`: a missing `// SAFETY:` is a warning here in the tests too
#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::fmt;
use std::ptr::null_mut;
use std::str::FromStr;

#[cfg(test)]
use windows::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND;
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MR_MID_NOT_FOUND, HANDLE, NTSTATUS, STATUS_NO_MORE_ENTRIES, STATUS_NONE_MAPPED,
    STATUS_SOME_NOT_MAPPED, STATUS_SUCCESS, WIN32_ERROR,
};
#[cfg(test)]
use windows::Win32::Security::Authentication::Identity::LsaEnumerateAccountRights;
use windows::Win32::Security::Authentication::Identity::{
    LSA_ENUMERATION_INFORMATION, LSA_HANDLE, LSA_LOOKUP_ISOLATED_AS_LOCAL, LSA_OBJECT_ATTRIBUTES, LSA_REFERENCED_DOMAIN_LIST,
    LSA_TRANSLATED_NAME, LSA_TRANSLATED_SID2, LSA_TRUST_INFORMATION, LSA_UNICODE_STRING, LsaAddAccountRights,
    LsaEnumerateAccountsWithUserRight, LsaFreeMemory, LsaLookupNames2, LsaLookupSids2, LsaNtStatusToWinError, LsaOpenPolicy,
    LsaQueryInformationPolicy, LsaRemoveAccountRights, POLICY_ACCOUNT_DOMAIN_INFO, POLICY_CREATE_ACCOUNT, POLICY_INFORMATION_CLASS,
    POLICY_LOOKUP_NAMES, POLICY_PRIMARY_DOMAIN_INFO, POLICY_VIEW_LOCAL_INFORMATION, PolicyAccountDomainInformation,
    PolicyPrimaryDomainInformation,
};
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, LookupAccountSidW, PSID, SID_AND_ATTRIBUTES, SID_NAME_USE, SidTypeDomain,
    SidTypeInvalid, SidTypeUnknown, TOKEN_ELEVATION_TYPE, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
    TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited, TokenGroups, TokenUser,
};
use windows::Win32::System::SystemInformation::{ComputerNameDnsDomain, GetComputerNameExW};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{Owned, PWSTR};

use super::privileges::format_win32_error;

/// A security identifier (TODO 113), as Windows lays one out (revision 1, the sub-authority count, the 6-byte authority, then the
/// sub-authorities), kept in `u32`s so the sub-authorities are aligned wherever it is read. Compared as bytes, never by name: names are
/// localized. `Display` is its `S-1-...` string, which `FromStr` reads back.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sid(Vec<u32>);

impl Sid {
    /// `SID_MAX_SUB_AUTHORITIES`
    const MAX_SUB_AUTHORITIES: usize = 15;
    /// The authority is 6 bytes
    const MAX_AUTHORITY: u64 = (1 << 48) - 1;

    /// The SID `bytes` hold, exactly: `None` for a short buffer, a revision other than 1, or a sub-authority count that doesn't match the
    /// length
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Sid> {
        let (&[revision, count], _) = bytes.split_first_chunk::<2>()?;
        let count = usize::from(count);
        if revision != 1 || count > Self::MAX_SUB_AUTHORITIES || bytes.len() != 8 + 4 * count {
            return None;
        }
        Some(Sid(bytes.as_chunks::<4>().0.iter().map(|word| u32::from_ne_bytes(*word)).collect()))
    }

    /// The SID of `authority` and `sub_authorities`: `None` for an authority above 6 bytes or more than 15 sub-authorities
    pub(crate) fn from_parts(authority: u64, sub_authorities: &[u32]) -> Option<Sid> {
        if authority > Self::MAX_AUTHORITY || sub_authorities.len() > Self::MAX_SUB_AUTHORITIES {
            return None;
        }
        let mut bytes = vec![1, sub_authorities.len() as u8];
        bytes.extend_from_slice(&authority.to_be_bytes()[2..]);
        for sub in sub_authorities {
            bytes.extend_from_slice(&sub.to_ne_bytes());
        }
        Sid::from_bytes(&bytes)
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|word| word.to_ne_bytes()).collect()
    }

    /// The identifier authority: 5 for NT AUTHORITY, 1 for Everyone's
    pub(crate) fn authority(&self) -> u64 {
        let bytes = self.bytes();
        bytes[2..8].iter().fold(0, |authority, &byte| authority << 8 | u64::from(byte))
    }

    pub(crate) fn sub_authorities(&self) -> &[u32] {
        &self.0[2..]
    }

    /// The SID for one call, which reads it ([in] only) while `self` is borrowed: never stored for a later call
    pub(crate) fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut c_void)
    }

    /// A copy of the SID at `psid`, `None` when it is null or not a valid SID.
    ///
    /// # Safety
    ///
    /// `psid` must be null or point at memory that stays readable for the call and holds a SID's header at least: `IsValidSid` checks
    /// the rest before `GetLengthSid` sizes the copy.
    unsafe fn copy_from(psid: PSID) -> Option<Sid> {
        if psid.is_invalid() {
            return None;
        }
        // SAFETY: the caller's contract: a readable SID header; IsValidSid reads no more than the SID it checks
        if !unsafe { IsValidSid(psid) }.as_bool() {
            return None;
        }
        // SAFETY: a valid SID, which GetLengthSid sizes from its own count, and which is readable for that length (the caller's contract)
        let bytes = unsafe { std::slice::from_raw_parts(psid.0 as *const u8, GetLengthSid(psid) as usize) };
        Sid::from_bytes(bytes)
    }
}

impl fmt::Display for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // As ConvertSidToStringSidW: an authority of 2^32 or more in hex
        match self.authority() {
            authority if authority >= 1 << 32 => write!(f, "S-1-0x{authority:012X}")?,
            authority => write!(f, "S-1-{authority}")?,
        }
        self.sub_authorities().iter().try_for_each(|sub| write!(f, "-{sub}"))
    }
}

impl fmt::Debug for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// An `S-1-...` string, as `Display` writes one (the authority in decimal, or in hex after `0x`), and nothing else
impl FromStr for Sid {
    type Err = ();

    fn from_str(text: &str) -> Result<Sid, ()> {
        let digits = |part: &str, radix: u32| part.chars().all(|c| c.is_digit(radix)) && !part.is_empty();
        let mut parts = text.split('-');
        if !parts.next().is_some_and(|s| s.eq_ignore_ascii_case("S")) || parts.next() != Some("1") {
            return Err(());
        }
        let authority = parts.next().ok_or(())?;
        let authority = match authority.strip_prefix("0x").or_else(|| authority.strip_prefix("0X")) {
            Some(hex) if digits(hex, 16) => u64::from_str_radix(hex, 16).map_err(|_| ())?,
            None if digits(authority, 10) => authority.parse().map_err(|_| ())?,
            _ => return Err(()),
        };
        let subs = parts.map(|part| if digits(part, 10) { part.parse::<u32>().map_err(|_| ()) } else { Err(()) });
        Sid::from_parts(authority, &subs.collect::<Result<Vec<u32>, ()>>()?).ok_or(())
    }
}

/// How this process's token was elevated (`TokenElevationType`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Elevation {
    /// No split token: UAC is off, or the account has no administrative rights to filter (or the type couldn't be read)
    Default,
    /// The elevated half of a split token: run as administrator
    Full,
    /// The filtered half: an administrator under UAC, not elevated
    Limited,
}

/// What this process's token says of whom it runs as: its account, its groups with their attributes (`SE_GROUP_*`), and its elevation.
/// Read once, with `TOKEN_QUERY`; every SID copied out of the token's buffers.
pub(crate) struct Token {
    pub(crate) user: Sid,
    pub(crate) groups: Vec<(Sid, u32)>,
    pub(crate) elevation: Elevation,
}

impl Token {
    /// This process's token. Its user must be readable; its groups that can't be are none, logged (the account is still the default).
    pub(crate) fn read() -> Result<Token, String> {
        let mut handle = HANDLE::default();
        // SAFETY: GetCurrentProcess' pseudo-handle needs no closing; `handle` is a local out-param passed directly
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) }.map_err(|e| format!("OpenProcessToken: {e}"))?;
        // SAFETY: the handle was just opened, and nothing else owns it: `Owned` closes it once
        let token = unsafe { Owned::new(handle) };
        let user = Self::user(&token_information(*token, TokenUser)?).ok_or("the token's user is not a SID")?;
        let groups = token_information(*token, TokenGroups).and_then(|info| Self::groups(&info)).unwrap_or_else(|e| {
            log::warn!("This process's groups couldn't be read ({e}): only its account is offered the large-page right");
            Vec::new()
        });
        let mut elevation = TOKEN_ELEVATION_TYPE::default();
        let mut len = 0u32;
        // SAFETY: `elevation` is a local passed directly as the out-param, with its own size
        let read = unsafe {
            GetTokenInformation(
                *token,
                TokenElevationType,
                Some(&mut elevation as *mut TOKEN_ELEVATION_TYPE as *mut c_void),
                size_of::<TOKEN_ELEVATION_TYPE>() as u32,
                &mut len,
            )
        };
        let elevation = if read.is_err() {
            Elevation::Default
        } else if elevation == TokenElevationTypeFull {
            Elevation::Full
        } else if elevation == TokenElevationTypeLimited {
            Elevation::Limited
        } else {
            Elevation::Default
        };
        Ok(Token { user, groups, elevation })
    }

    fn user(info: &TokenInfo) -> Option<Sid> {
        if info.len < size_of::<TOKEN_USER>() {
            return None;
        }
        // SAFETY: the buffer holds a TOKEN_USER (checked against the length the call returned), and a `u64` buffer is aligned for it
        let user = unsafe { std::ptr::read(info.buffer.as_ptr() as *const TOKEN_USER) };
        info.sid_at(user.User.Sid)
    }

    /// TOKEN_GROUPS' entries: `GroupCount` checked against the length the call returned, and the array taken from the raw buffer, never
    /// through a `&TOKEN_GROUPS`, whose one-element array would bound it
    fn groups(info: &TokenInfo) -> Result<Vec<(Sid, u32)>, String> {
        let bytes = info.bytes();
        let count = bytes.first_chunk::<4>().map(|count| u32::from_ne_bytes(*count) as usize).ok_or("TokenGroups: no count")?;
        let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
        let end = count.checked_mul(size_of::<SID_AND_ATTRIBUTES>()).and_then(|size| size.checked_add(offset));
        if end.is_none_or(|end| end > info.len) {
            return Err(format!("TokenGroups: {count} groups don't fit its {} bytes", info.len));
        }
        // SAFETY: `count` entries fit the bytes the call returned (checked above) from `offset`, which is SID_AND_ATTRIBUTES' place in
        // TOKEN_GROUPS, so aligned for it in the `u64` buffer; the slice is read while `info` lives
        let entries =
            unsafe { std::slice::from_raw_parts(info.buffer.as_ptr().cast::<u8>().add(offset).cast::<SID_AND_ATTRIBUTES>(), count) };
        Ok(entries.iter().filter_map(|entry| info.sid_at(entry.Sid).map(|sid| (sid, entry.Attributes))).collect())
    }
}

/// A token's information, as `GetTokenInformation` returned it: `len` bytes of `buffer`, which is `u64`s so the structures read from it
/// are aligned (the file's ALIGNMENT note, `privileges.rs`)
struct TokenInfo {
    buffer: Vec<u64>,
    len: usize,
}

impl TokenInfo {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `len` bytes of the buffer, which is initialised and at least that long (`token_information`), viewed as bytes while
        // `self` is borrowed
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), self.len) }
    }

    /// The SID at `psid`, which a token's structures point at within their own buffer: read from that buffer's bytes, and none if it isn't
    /// in them whole
    fn sid_at(&self, psid: PSID) -> Option<Sid> {
        let bytes = self.bytes();
        let start = (psid.0 as usize).checked_sub(bytes.as_ptr() as usize)?;
        let sid = bytes.get(start..)?;
        Sid::from_bytes(sid.get(..8 + 4 * usize::from(*sid.get(1)?))?)
    }
}

/// `token`'s `class` information: the size asked first, then read into a buffer of it, again if it grew in between
fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<TokenInfo, String> {
    let mut len = 0u32;
    // SAFETY: the size query: no buffer, and `len` a local out-param
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut len) };
    for _ in 0..3 {
        let mut buffer = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        let size = (buffer.len() * size_of::<u64>()) as u32;
        // SAFETY: the buffer is writable for `size` bytes, the length passed, and `len` is a local out-param
        match unsafe { GetTokenInformation(token, class, Some(buffer.as_mut_ptr().cast()), size, &mut len) } {
            Ok(()) => return Ok(TokenInfo { len: (len as usize).min(size as usize), buffer }),
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => continue,
            Err(e) => return Err(format!("GetTokenInformation: {e}")),
        }
    }
    Err("GetTokenInformation: its size kept changing".to_string())
}

/// What Windows calls a SID: `DOMAIN\name` (`name` alone when the domain is empty: Everyone; the domain's name for a domain), and its type
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) name: String,
    pub(crate) kind: SID_NAME_USE,
}

impl Named {
    /// From a lookup's parts: `None` when Windows can't name the SID (unknown, or a domain it can't reach)
    fn of(kind: SID_NAME_USE, domain: String, name: String) -> Option<Named> {
        if kind == SidTypeUnknown || kind == SidTypeInvalid {
            return None;
        }
        let name = if kind == SidTypeDomain && !domain.is_empty() {
            domain
        } else if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        };
        Some(Named { name, kind })
    }
}

/// UTF-16 up to its first NUL: a lookup's lengths are documented for its failure path only, so the text is cut there
fn until_nul(units: &[u16]) -> String {
    String::from_utf16_lossy(units.split(|&unit| unit == 0).next().unwrap_or_default())
}

/// What Windows calls `sid` (`LookupAccountSidW`), asked again while its buffers are too small: the name of the account TMR runs as, which
/// needs no policy handle, so no elevation
pub(crate) fn account_name(sid: &Sid) -> Result<Named, String> {
    let (mut name, mut domain) = (vec![0u16; 64], vec![0u16; 64]);
    for _ in 0..4 {
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: both buffers are writable for the lengths passed, in UTF-16 units; the SID is borrowed for the call; the rest are locals
        let looked_up = unsafe {
            LookupAccountSidW(
                None,
                sid.as_psid(),
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        };
        match looked_up {
            Ok(()) => return Named::of(kind, until_nul(&domain), until_nul(&name)).ok_or_else(|| format!("{sid} has no name")),
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {
                name.resize((name_len as usize).max(name.len() + 1), 0);
                domain.resize((domain_len as usize).max(domain.len() + 1), 0);
            }
            Err(e) => return Err(format!("LookupAccountSidW: {}", e.message())),
        }
    }
    Err("LookupAccountSidW: its name kept growing".to_string())
}

/// Whether this PC has a DNS domain (`GetComputerNameExW`, `ComputerNameDnsDomain`): a domain PC, where a domain Group Policy that sets
/// the right replaces a local grant
pub(crate) fn in_a_domain() -> bool {
    let mut len = 0u32;
    // SAFETY: the size query: no buffer, and `len` a local out-param
    let _ = unsafe { GetComputerNameExW(ComputerNameDnsDomain, None, &mut len) };
    // The size counts the NUL: 1 is an empty name
    if len <= 1 {
        return false;
    }
    let mut buffer = vec![0u16; len as usize];
    // SAFETY: the buffer is writable for `len` UTF-16 units, the size passed
    let read = unsafe { GetComputerNameExW(ComputerNameDnsDomain, Some(PWSTR(buffer.as_mut_ptr())), &mut len) };
    read.is_ok() && !until_nul(&buffer).is_empty()
}

/// An LSA call's failure, as `format_win32_error` words its Win32 code, with the NTSTATUS: `LsaAddAccountRights: Access is denied. (5,
/// NTSTATUS 0xC0000022)`. A status with no Win32 code (317, `ERROR_MR_MID_NOT_FOUND`, whose text is "cannot find message text") gives the
/// NTSTATUS alone.
pub(crate) fn lsa_error(call: &str, status: NTSTATUS) -> String {
    // SAFETY: a value argument
    let code = unsafe { LsaNtStatusToWinError(status) };
    let ntstatus = format!("NTSTATUS 0x{:08X}", status.0 as u32);
    if code == ERROR_MR_MID_NOT_FOUND.0 {
        format!("{call}: {ntstatus}")
    } else {
        format!("{call}: {} ({code}, {ntstatus})", format_win32_error(WIN32_ERROR(code)))
    }
}

/// An LSA allocation, freed (`LsaFreeMemory`) when it drops, null or not. Bound by name (`let _buffer = LsaBuffer(..)`): `let _ =` would
/// free it at once.
struct LsaBuffer(*mut c_void);

impl Drop for LsaBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an LSA call allocated it for this guard alone, and nothing else frees it
            let _ = unsafe { LsaFreeMemory(Some(self.0)) };
        }
    }
}

/// `units` as the LSA string its calls take, which points into `units`: the caller keeps them alive as long as the string is read
fn lsa_string(units: &mut [u16]) -> Result<LSA_UNICODE_STRING, String> {
    let bytes = u16::try_from(2 * units.len()).map_err(|_| format!("a name of {} characters is too long for Windows", units.len()))?;
    Ok(LSA_UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: PWSTR(units.as_mut_ptr()) })
}

/// An LSA string's text, cut at a NUL.
///
/// # Safety
///
/// `text.Buffer` must be null or readable for `text.Length` bytes while this reads it.
unsafe fn lsa_text(text: &LSA_UNICODE_STRING) -> String {
    if text.Buffer.is_null() || text.Length == 0 {
        return String::new();
    }
    // SAFETY: the caller's contract
    until_nul(unsafe { std::slice::from_raw_parts(text.Buffer.0, usize::from(text.Length) / 2) })
}

/// A lookup's referenced domains, by index: their names.
///
/// # Safety
///
/// `list` must be null or a list an LSA lookup returned, alive while this reads it.
unsafe fn referenced_domains(list: *const LSA_REFERENCED_DOMAIN_LIST) -> Vec<String> {
    if list.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller's contract: the list LSA returned, with `Entries` trust records at `Domains`
    unsafe {
        let list = &*list;
        if list.Domains.is_null() {
            return Vec::new();
        }
        let domains: &[LSA_TRUST_INFORMATION] = std::slice::from_raw_parts(list.Domains, list.Entries as usize);
        domains.iter().map(|domain| lsa_text(&domain.Name)).collect()
    }
}

/// The access a policy handle is opened with: the least each use needs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Name SIDs and accounts, read who holds the right, and remove it (`LsaRemoveAccountRights` needs `POLICY_LOOKUP_NAMES`)
    Read,
    /// That, and grant it: `LsaAddAccountRights` creates the account's object in the policy, which a first grant needs
    Grant,
}

impl Access {
    fn mask(self) -> u32 {
        let read = (POLICY_LOOKUP_NAMES | POLICY_VIEW_LOCAL_INFORMATION) as u32;
        match self {
            Access::Read => read,
            Access::Grant => read | POLICY_CREATE_ACCOUNT as u32,
        }
    }
}

/// Calls `f` with the right large pages need, as the LSA string its calls take
fn with_right<R>(f: impl FnOnce(&LSA_UNICODE_STRING) -> R) -> R {
    let mut name: Vec<u16> = "SeLockMemoryPrivilege".encode_utf16().collect();
    // LIFETIME: `right.Buffer` points into `name`, which outlives `f`, and so every LSA call made in it. 21 characters fit a string.
    let right =
        LSA_UNICODE_STRING { Length: (2 * name.len()) as u16, MaximumLength: (2 * name.len()) as u16, Buffer: PWSTR(name.as_mut_ptr()) };
    f(&right)
}

/// This PC's security policy, open (`LsaOpenPolicy`), closed when it drops (`LsaClose`, through `Owned`)
pub(crate) struct Policy(Owned<LSA_HANDLE>);

impl Policy {
    /// The policy with `access`. Needs TMR run as administrator for most of it: Microsoft's "You must run the process 'As Administrator'
    /// so that the call doesn't fail with ERROR_ACCESS_DENIED".
    pub(crate) fn open(access: Access) -> Result<Policy, String> {
        // Its members aren't used: "initialize them to NULL or zero"
        let attributes = LSA_OBJECT_ATTRIBUTES::default();
        let mut handle = LSA_HANDLE::default();
        // SAFETY: SystemName None is this PC; `attributes` and `handle` are locals passed directly
        let status = unsafe { LsaOpenPolicy(None, &attributes, access.mask(), &mut handle) };
        if status != STATUS_SUCCESS {
            return Err(lsa_error("LsaOpenPolicy", status));
        }
        // SAFETY: the handle was just opened, and nothing else owns it: `Owned` closes it once
        Ok(Policy(unsafe { Owned::new(handle) }))
    }

    /// The accounts and groups the policy gives the right to, each by its own SID: a group comes back as the group, not its members
    /// ("hold the specified privilege directly through the user account, not as part of membership to a group"). The one place the
    /// enumeration is called, always with the right: without it (`None`) it enumerates every account in the policy, and every group
    /// would look like a holder.
    pub(crate) fn holders(&self) -> Result<Vec<Sid>, String> {
        with_right(|right| {
            let mut buffer: *mut c_void = null_mut();
            let mut count = 0u32;
            // SAFETY: `right` is the one right, alive for the call (`with_right`); `buffer` and `count` are locals, the buffer freed below
            let status = unsafe { LsaEnumerateAccountsWithUserRight(*self.0, Some(std::ptr::from_ref(right)), &mut buffer, &mut count) };
            let _buffer = LsaBuffer(buffer);
            // Nobody holds it: a warning status, which `is_ok` would call an error
            if status == STATUS_NO_MORE_ENTRIES {
                return Ok(Vec::new());
            }
            if status != STATUS_SUCCESS {
                return Err(lsa_error("LsaEnumerateAccountsWithUserRight", status));
            }
            if buffer.is_null() {
                return Ok(Vec::new());
            }
            // SAFETY: on success the buffer holds `count` entries, alive until `_buffer` frees it
            let entries = unsafe { std::slice::from_raw_parts(buffer as *const LSA_ENUMERATION_INFORMATION, count as usize) };
            // SAFETY: each entry's SID is in the same LSA allocation, alive until `_buffer` frees it
            Ok(entries.iter().filter_map(|entry| unsafe { Sid::copy_from(entry.Sid) }).collect())
        })
    }

    /// Gives `sid` the right: its account's object is created in the policy if it has none, and a right it holds already is ignored
    pub(crate) fn add(&self, sid: &Sid) -> Result<(), String> {
        with_right(|right| {
            // SAFETY: the SID is borrowed for the call ([in] only), and `right` with its buffer outlives it
            let status = unsafe { LsaAddAccountRights(*self.0, sid.as_psid(), std::slice::from_ref(right)) };
            if status == STATUS_SUCCESS { Ok(()) } else { Err(lsa_error("LsaAddAccountRights", status)) }
        })
    }

    /// Takes the right from `sid`, and only it (`AllRights` false)
    pub(crate) fn remove(&self, sid: &Sid) -> Result<(), String> {
        with_right(|right| {
            // SAFETY: as `add`'s
            let status = unsafe { LsaRemoveAccountRights(*self.0, sid.as_psid(), false, Some(std::slice::from_ref(right))) };
            if status == STATUS_SUCCESS { Ok(()) } else { Err(lsa_error("LsaRemoveAccountRights", status)) }
        })
    }

    /// Windows' names for `sids`, in one call (`LsaLookupSids2`), so a PC away from its domain controller waits out one time-out, not one
    /// a group: each `None` that Windows can't name. Some named and some not is a success (`STATUS_SOME_NOT_MAPPED`).
    pub(crate) fn names(&self, sids: &[Sid]) -> Result<Vec<Option<Named>>, String> {
        // The documented limit of one call
        if sids.is_empty() || sids.len() > 20480 {
            return Ok(vec![None; sids.len()]);
        }
        // LIFETIME: each PSID points into a `Sid` of `sids`, borrowed for the call, which reads the array and is done with it on return
        let psids: Vec<PSID> = sids.iter().map(Sid::as_psid).collect();
        let (mut domains, mut names): (*mut LSA_REFERENCED_DOMAIN_LIST, *mut LSA_TRANSLATED_NAME) = (null_mut(), null_mut());
        // SAFETY: `psids` holds `count` SIDs for the call; the out-params are locals, freed below on every path
        let status = unsafe { LsaLookupSids2(*self.0, 0, psids.len() as u32, psids.as_ptr(), &mut domains, &mut names) };
        let (_domains, _names) = (LsaBuffer(domains.cast()), LsaBuffer(names.cast()));
        if status == STATUS_NONE_MAPPED || (status.0 >= 0 && names.is_null()) {
            return Ok(vec![None; sids.len()]);
        }
        if status != STATUS_SUCCESS && status != STATUS_SOME_NOT_MAPPED {
            return Err(lsa_error("LsaLookupSids2", status));
        }
        // SAFETY: on success `domains` is the list the names index, alive until `_domains` frees it
        let domains = unsafe { referenced_domains(domains) };
        // SAFETY: on success `names` holds one translation a SID, alive until `_names` frees it
        let translated = unsafe { std::slice::from_raw_parts(names, sids.len()) };
        Ok(translated
            .iter()
            .map(|t| {
                let domain = usize::try_from(t.DomainIndex).ok().and_then(|i| domains.get(i)).cloned().unwrap_or_default();
                // SAFETY: the name is in the same LSA allocation, alive until `_names` frees it
                Named::of(t.Use, domain, unsafe { lsa_text(&t.Name) })
            })
            .collect())
    }

    /// The accounts `names` name, in one call (`LsaLookupNames2`): each its SID and type, `None` when Windows knows none by it. With
    /// `isolated_as_local`, a name without a domain is looked for on this PC only (`LSA_LOOKUP_ISOLATED_AS_LOCAL`), not in the domain or
    /// the domains it trusts, which a first match would otherwise pick from.
    pub(crate) fn lookup(&self, names: &[String], isolated_as_local: bool) -> Result<Vec<Option<(Sid, SID_NAME_USE)>>, String> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut units: Vec<Vec<u16>> = names.iter().map(|name| name.encode_utf16().collect()).collect();
        // LIFETIME: each string's Buffer points into `units`, which outlives the call
        let strings = units.iter_mut().map(|name| lsa_string(name)).collect::<Result<Vec<_>, _>>()?;
        let flags = if isolated_as_local { LSA_LOOKUP_ISOLATED_AS_LOCAL } else { 0 };
        let (mut domains, mut sids): (*mut LSA_REFERENCED_DOMAIN_LIST, *mut LSA_TRANSLATED_SID2) = (null_mut(), null_mut());
        // SAFETY: `strings` holds `count` names, alive for the call (above); the out-params are locals, freed below on every path
        let status = unsafe { LsaLookupNames2(*self.0, flags, strings.len() as u32, strings.as_ptr(), &mut domains, &mut sids) };
        let (_domains, _sids) = (LsaBuffer(domains.cast()), LsaBuffer(sids.cast()));
        if status == STATUS_NONE_MAPPED || (status.0 >= 0 && sids.is_null()) {
            return Ok(vec![None; names.len()]);
        }
        if status != STATUS_SUCCESS && status != STATUS_SOME_NOT_MAPPED {
            return Err(lsa_error("LsaLookupNames2", status));
        }
        // SAFETY: on success `sids` holds one translation a name, alive until `_sids` frees it
        let translated = unsafe { std::slice::from_raw_parts(sids, names.len()) };
        Ok(translated
            .iter()
            .map(|t| {
                if t.Use == SidTypeUnknown || t.Use == SidTypeInvalid {
                    return None;
                }
                // SAFETY: a translated name's SID is in the same LSA allocation, alive until `_sids` frees it, or null
                unsafe { Sid::copy_from(t.Sid) }.map(|sid| (sid, t.Use))
            })
            .collect())
    }

    /// The domain this PC's own accounts are in (`PolicyAccountDomainInformation`): this PC's name, `.\`'s, but on a domain controller
    /// the domain's
    pub(crate) fn account_domain(&self) -> Result<String, String> {
        self.query(PolicyAccountDomainInformation, |info| {
            // SAFETY: what the query returns for this class, alive while `query` holds its buffer
            unsafe { lsa_text(&(*(info as *const POLICY_ACCOUNT_DOMAIN_INFO)).DomainName) }
        })
    }

    /// The domain this PC is joined to (`PolicyPrimaryDomainInformation`), `None` in a workgroup, whose name it has but no SID
    pub(crate) fn primary_domain(&self) -> Result<Option<String>, String> {
        self.query(PolicyPrimaryDomainInformation, |info| {
            // SAFETY: what the query returns for this class, its name in the same buffer, alive while `query` holds it
            let (sid, name) = unsafe {
                let info = &*(info as *const POLICY_PRIMARY_DOMAIN_INFO);
                (info.Sid, lsa_text(&info.Name))
            };
            (!sid.is_invalid()).then_some(name)
        })
    }

    /// `LsaQueryInformationPolicy` of `class`, read by `read` while its buffer lives, then freed
    fn query<R>(&self, class: POLICY_INFORMATION_CLASS, read: impl FnOnce(*const c_void) -> R) -> Result<R, String> {
        let mut buffer: *mut c_void = null_mut();
        // SAFETY: `buffer` is a local out-param, freed below on every path
        let status = unsafe { LsaQueryInformationPolicy(*self.0, class, &mut buffer) };
        let _buffer = LsaBuffer(buffer);
        if status != STATUS_SUCCESS {
            return Err(lsa_error("LsaQueryInformationPolicy", status));
        }
        if buffer.is_null() {
            return Err("LsaQueryInformationPolicy: no information".to_string());
        }
        Ok(read(buffer))
    }

    /// The rights the policy gives `sid` itself (`LsaEnumerateAccountRights`): the probe's check of the holders, read-only
    #[cfg(test)]
    pub(crate) fn rights(&self, sid: &Sid) -> Result<Vec<String>, String> {
        let mut rights: *mut LSA_UNICODE_STRING = null_mut();
        let mut count = 0u32;
        // SAFETY: the SID is borrowed for the call; the out-params are locals, the buffer freed below on every path
        let status = unsafe { LsaEnumerateAccountRights(*self.0, sid.as_psid(), &mut rights, &mut count) };
        let _rights = LsaBuffer(rights.cast());
        if status == STATUS_OBJECT_NAME_NOT_FOUND {
            return Ok(Vec::new());
        }
        if status != STATUS_SUCCESS {
            return Err(lsa_error("LsaEnumerateAccountRights", status));
        }
        if rights.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: on success `rights` holds `count` strings, each in the same LSA allocation, alive until `_rights` frees it
        let rights = unsafe { std::slice::from_raw_parts(rights, count as usize) };
        // SAFETY: as above
        Ok(rights.iter().map(|right| unsafe { lsa_text(right) }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 113: a SID from its bytes, as Windows lays it out, and its `S-1-...` string, which reads back as the same SID; an authority of
    /// 2^32 or more in hex. Refused: a short buffer, a count that doesn't match the length, revision 2, and strings that aren't a SID.
    #[test]
    #[rustfmt::skip]
    fn a_sid_reads_from_its_bytes_and_its_string() {
        let bytes = |revision: u8, authority: u8, subs: &[u32]| {
            let mut bytes = vec![revision, subs.len() as u8, 0, 0, 0, 0, 0, authority];
            subs.iter().for_each(|sub| bytes.extend_from_slice(&sub.to_le_bytes()));
            bytes
        };
        for (sid, authority, subs) in [
            ("S-1-1-0", 1, &[0][..]),
            ("S-1-5-32-544", 5, &[32, 544]),
            ("S-1-5-21-1-2-3-513", 5, &[21, 1, 2, 3, 513]),
            ("S-1-16-12288", 16, &[12288]),
            ("S-1-5", 5, &[]),
        ] {
            let read = Sid::from_bytes(&bytes(1, authority, subs)).expect(sid);
            assert_eq!(read.to_string(), sid);
            assert_eq!((read.authority(), read.sub_authorities()), (u64::from(authority), subs), "{sid}");
            assert_eq!(sid.parse::<Sid>(), Ok(read.clone()), "{sid} reads back");
            assert_eq!(read.bytes(), bytes(1, authority, subs), "{sid}: its bytes");
        }
        let wide = Sid::from_parts(1 << 40, &[7]).unwrap();
        assert_eq!(wide.to_string(), "S-1-0x010000000000-7");
        assert_eq!("S-1-0x010000000000-7".parse::<Sid>(), Ok(wide));
        assert_eq!("s-1-5-18".parse::<Sid>().map(|sid| sid.to_string()), Ok("S-1-5-18".to_string()), "a small s");

        let mut short = bytes(1, 5, &[32, 544]);
        short.pop();
        assert_eq!(Sid::from_bytes(&short), None, "a short buffer");
        assert_eq!(Sid::from_bytes(&[1, 2]), None, "no authority");
        let mut miscounted = bytes(1, 5, &[32, 544]);
        miscounted[1] = 3;
        assert_eq!(Sid::from_bytes(&miscounted), None, "a count that doesn't match the length");
        assert_eq!(Sid::from_bytes(&bytes(2, 5, &[32])), None, "revision 2");
        for not in ["", "S-1", "S-2-5-32", "S-1-5-x", "S-1-5--1", "S-1-5-+5", "S-1-5-4294967296", "Administrators", "S-1-0x-5",
                    "S-1-5-1-2-3-4-5-6-7-8-9-10-11-12-13-14-15-16"] {
            assert_eq!(not.parse::<Sid>(), Err(()), "{not:?}");
        }
    }

    /// TODO 113: text is cut at its first NUL, as the lookups' lengths aren't documented on success
    #[test]
    fn a_lookups_text_stops_at_its_nul() {
        use windows::Win32::Security::{SidTypeAlias, SidTypeWellKnownGroup};
        let units: Vec<u16> = "PC\0junk".encode_utf16().collect();
        assert_eq!(until_nul(&units), "PC");
        assert_eq!(until_nul(&[]), "");
        assert_eq!(Named::of(SidTypeUnknown, String::new(), "x".to_string()), None, "no name");
        let named = |domain: &str, name: &str, kind| Named::of(kind, domain.to_string(), name.to_string()).map(|named| named.name);
        assert_eq!(named("BUILTIN", "Administrators", SidTypeAlias).as_deref(), Some(r"BUILTIN\Administrators"));
        assert_eq!(named("", "Everyone", SidTypeWellKnownGroup).as_deref(), Some("Everyone"), "no domain");
        assert_eq!(named("PC", "", SidTypeDomain).as_deref(), Some("PC"), "a domain: its name");
    }
}
