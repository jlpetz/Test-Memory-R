// Every `unsafe` block in this file crosses a Win32 FFI boundary, where the borrow checker is switched off precisely where the invariants
// get subtle (TODO #66). The lint below makes a missing `// SAFETY:` a warning here in the tests too; `src/lib.rs` makes it one in all
// shipped code. It was per file until 2026-10-23, lest the SIMD kernels need boilerplate, but their `unsafe fn`s need no blocks
// (`test_fn_safety.md` covers them): crate-wide added 30 sites, none a kernel's.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::fs;
use std::process::Command;
use std::ptr::null_mut;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID, WIN32_ERROR};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrivileges,
};
use windows::Win32::System::Diagnostics::Debug::{FORMAT_MESSAGE_FROM_SYSTEM, FormatMessageW};
use windows::Win32::System::Memory::{MEM_COMMIT, MEM_LARGE_PAGES, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PWSTR;

#[derive(Debug, PartialEq)]
enum PrivilegeState {
    NotAssigned,
    AssignedButDisabled,
    AssignedAndEnabled,
}

fn format_win32_error(err: WIN32_ERROR) -> String {
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

/// What a session asks about large pages before its run (`Frontend::large_page_restart`), when this process is elevated and its token
/// lacks the right they need, SeLockMemoryPrivilege ('Lock pages in memory'). Either way they take effect only once Windows restarts: a
/// token takes the rights its account has at its sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LargePageQuestion {
    /// The security policy assigns the right already, so only the restart is missing. To some account: the check doesn't say which
    /// (`check_if_privilege_in_policy`).
    Restart,
    /// The policy doesn't assign it, and TMR could grant it to `account` (`DOMAIN\user`, the account TMR runs as), which then keeps it
    /// until it is removed
    Grant { account: String },
}

/// The account TMR runs as, `DOMAIN\user`: the one a grant names
fn account() -> String {
    let username = std::env::var("USERNAME").unwrap_or_else(|_| "Administrator".to_string());
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "WORKGROUP".to_string()));
    format!("{}\\{}", domain, username)
}

/// The large-page question this process has for its user (`session::run`), if any: none when its token has the right, or when it isn't
/// elevated, so it couldn't grant it (the diagnostic says what to do then). Reads the security policy (a `secedit /export`), and changes
/// nothing.
pub fn large_page_question() -> Option<LargePageQuestion> {
    let in_token = check_privilege_state() != PrivilegeState::NotAssigned;
    // An export that fails reads as no assignment: the question offers the grant, which would then fail as the export did, and say why
    question_for(in_token, is_elevated(), || check_if_privilege_in_policy().unwrap_or_default(), account)
}

/// `large_page_question`'s choice, from whether this token has the right (`in_token`, enabled or not) and this process is `elevated`:
/// `in_policy` (the `secedit /export`) is read only when those two leave a question to ask, and `account` named only for the grant
fn question_for(
    in_token: bool, elevated: bool, in_policy: impl FnOnce() -> bool, account: impl FnOnce() -> String,
) -> Option<LargePageQuestion> {
    if in_token || !elevated {
        return None;
    }
    Some(if in_policy() { LargePageQuestion::Restart } else { LargePageQuestion::Grant { account: account() } })
}

/// Grants `account` the right large pages need, editing the machine's security policy (`grant_privilege_via_secedit`). Called only once
/// the user has said yes to the question that named `account` (`session::run`), or by `--setup-large-pages` (`setup_large_pages`), the
/// command being the request. The account keeps the right until it is removed, and it takes effect once Windows restarts.
pub fn grant_large_pages(account: &str) -> Result<(), String> {
    if !is_elevated() {
        return Err("Administrator privileges required".to_string());
    }

    log::info!("Attempting to grant SeLockMemoryPrivilege to: {}", account);

    grant_privilege_via_secedit(account).map_err(|e| {
        // Provide more specific error context
        if e.contains("configure failed") && e.contains("privileges") {
            "Privilege assignment failed - may already be assigned (restart needed)".to_string()
        } else if e.contains("UTF-8") || e.contains("encoding") {
            "File encoding issue during policy modification".to_string()
        } else {
            format!("Setup failed: {}", e)
        }
    })
}

/// `--setup-large-pages`: the diagnostic, then, when the right is missing from this token and this process is elevated, the grant, to the
/// account TMR runs as. It doesn't ask: the command is the request.
pub fn setup_large_pages() -> String {
    let mut report = diagnose_large_pages();
    if check_privilege_state() == PrivilegeState::NotAssigned && is_elevated() {
        let account = account();
        let outcome = grant_large_pages(&account);
        report.push_str(&grant_report(&account, outcome));
    }
    report
}

/// What `--setup-large-pages` says of its grant to `account`, one line each, and the manual steps when it failed
fn grant_report(account: &str, outcome: Result<(), String>) -> String {
    let mut messages = Vec::new();
    match outcome {
        Ok(()) => {
            messages.push(format!("✅ Successfully granted SeLockMemoryPrivilege to {}, which keeps it until it is removed", account));
            messages.push("".to_string());
            messages.push("🔄 RESTART REQUIRED: Restart Windows (or sign out and back in) for it to take effect".to_string());
        }
        Err(grant_error) => {
            messages.push(format!("❌ Automatic setup failed: {}", grant_error));
            messages.push("".to_string());

            // Provide context-aware guidance
            if grant_error.contains("already") || grant_error.contains("assigned") {
                messages.push("💡 POSSIBLE CAUSE: Privilege already set but restart needed".to_string());
                messages.push("   If you recently used gpedit.msc or other tools to grant the privilege,".to_string());
                messages.push("   restart TMR (or log off/log on) to apply the changes.".to_string());
                messages.push("".to_string());
                messages.push("🔄 TRY FIRST: Restart TMR and run again".to_string());
                messages.push("".to_string());
            }

            messages.push("📖 Manual Setup (if restart doesn't work):".to_string());
            messages.push("1. Run gpedit.msc as Administrator".to_string());
            messages.push(
                "2. Navigate to: Computer Configuration → Windows Settings → Security Settings → Local Policies → User Rights Assignment"
                    .to_string(),
            );
            messages.push("3. Double-click 'Lock pages in memory'".to_string());
            messages.push("4. Add your user account or 'Administrators' group".to_string());
            messages.push("5. Restart Windows (or sign out and back in)".to_string());
        }
    }
    messages.join("\n") + "\n"
}

/// The large-page check's lines: the account, the right's state in this token, enabling it there when it is disabled, and a test
/// allocation. It never grants the right (`grant_large_pages` does, asked). The allocator calls it too, before a large-page request
/// (`WindowsBackend::allocate_with_virtualalloc2`).
pub fn enable_large_pages() -> Result<String, String> {
    let mut messages = Vec::new();

    // Show current user
    if let Ok(username) = std::env::var("USERNAME")
        && let Ok(domain) = std::env::var("USERDOMAIN")
    {
        messages.push(format!("👤 User: {}\\{}", domain, username));
    }

    match check_privilege_state() {
        PrivilegeState::NotAssigned => {
            messages.push("❌ SeLockMemoryPrivilege: NOT in current session token".to_string());

            if is_elevated() {
                // A session asks first (`large_page_question`), and `--setup-large-pages` grants it next (`setup_large_pages`)
                messages.push("✅ Running as Administrator: TMR can grant it".to_string());
            } else {
                messages.push("❌ Not running as Administrator".to_string());
                messages.push("".to_string());
                messages.push("🔧 SOLUTIONS (try in order):".to_string());
                messages.push(
                    "1. Restart TMR as Administrator: it asks to grant it (tmr.exe --setup-large-pages grants it at once)".to_string(),
                );
                messages.push("2. If recently configured: Restart TMR normally (privilege may need activation)".to_string());
                messages.push("3. Manual setup via gpedit.msc".to_string());
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
/// grants the right: a session asks first (`large_page_question`), and `--setup-large-pages` grants it after this (`setup_large_pages`).
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

    // Show current user
    if let Ok(username) = std::env::var("USERNAME") {
        if let Ok(domain) = std::env::var("USERDOMAIN") {
            report.push_str(&format!("👤 User: {}\\{}\n", domain, username));
        } else {
            report.push_str(&format!("👤 User: {}\n", username));
        }
    }

    report.push('\n');

    // Enable it in this token, and try it
    let _setup_successful = match enable_large_pages() {
        Ok(setup_result) => {
            report.push_str("Auto-Setup Results:\n");
            report.push_str(&setup_result);
            report.push('\n');

            // Check if setup was actually successful
            setup_result.contains("✅ Large page test allocation successful")
        }
        Err(setup_error) => {
            report.push_str("Auto-Setup Failed:\n");
            report.push_str(&setup_error);
            report.push('\n');
            false
        }
    };

    // Manual instructions are already shown above in the messages, no need to duplicate
    report
}

/// `secedit`, with no console window of its own. Its output is piped to TMR (`output()`), but a console program started by a process with
/// no console gets a console window, which flashes on the desktop: the window's, launched with a double-click (TODO 29 S6). Nothing changes
/// for the CLI, whose console its output never reached.
///
/// System32's, by its full path (TODO 104): a bare name is looked for in the exe's folder first, which the user may be able to write to
/// (Downloads, for a bare tmr.exe, or a per-user install's folder), and TMR runs it elevated, so a secedit.exe put there would run as
/// admin.
fn secedit() -> Command {
    use std::os::windows::process::CommandExt;
    let mut system = [0u16; 260];
    // SAFETY: the buffer is writable for its length, which the slice passes; the call writes at most that and returns how much it wrote
    let len = unsafe { windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut system)) } as usize;
    let program = match system.get(..len).filter(|path| !path.is_empty()) {
        Some(path) => std::path::PathBuf::from(String::from_utf16_lossy(path)).join("secedit.exe"),
        None => std::path::PathBuf::from("secedit"),
    };
    let mut command = Command::new(program);
    command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
    command
}

fn grant_privilege_via_secedit(username: &str) -> Result<(), String> {
    // Use absolute paths in TEMP directory to avoid working directory issues
    let temp_dir = std::env::temp_dir();
    let temp_config = temp_dir.join("tmr_secpol.inf");
    let temp_db = temp_dir.join("tmr_secpol.sdb");
    let temp_config_str = temp_config.to_str().ok_or("Invalid temp path")?;
    let temp_db_str = temp_db.to_str().ok_or("Invalid temp path")?;

    // Step 1: Export current security policy
    let output =
        secedit().args(["/export", "/cfg", temp_config_str]).output().map_err(|e| format!("Failed to run secedit export: {}", e))?;

    if !output.status.success() {
        return Err(format!("secedit export failed: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // Step 2: Read and modify the policy file
    let bytes = fs::read(&temp_config).map_err(|e| format!("Failed to read security policy: {}", e))?;

    // Detect encoding - secedit typically exports as UTF-16 LE
    let is_utf16_le = bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE;

    log::info!("Exported policy file encoding: {}", if is_utf16_le { "UTF-16 LE" } else { "UTF-8/ANSI" });

    let content = if is_utf16_le {
        // Decode UTF-16 LE (skip 2-byte BOM). `as_chunks::<2>()` yields fixed-size [u8; 2] arrays (no per-index bounds checks); `.0` drops
        // any odd trailing byte, matching the previous `chunks_exact(2)` behaviour.
        let utf16_data: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|chunk| u16::from_le_bytes(*chunk)).collect();
        String::from_utf16(&utf16_data).map_err(|_| "Invalid UTF-16 data in policy file")?
    } else {
        // Assume UTF-8 or ASCII
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let modified_content = modify_security_policy(&content, username)?;

    // Step 3: Write modified policy in the SAME encoding as the original
    if is_utf16_le {
        // Write as UTF-16 LE with BOM (same as secedit export)
        // Convert string to UTF-16 code units
        let utf16_string: Vec<u16> = modified_content.encode_utf16().collect();

        // Create byte array with BOM + UTF-16 LE data
        let mut file_content = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        for code_unit in utf16_string {
            file_content.push((code_unit & 0xFF) as u8); // Low byte
            file_content.push(((code_unit >> 8) & 0xFF) as u8); // High byte
        }

        fs::write(&temp_config, file_content).map_err(|e| format!("Failed to write modified policy: {}", e))?;

        log::info!("Wrote policy file as UTF-16 LE to: {}", temp_config_str);
    } else {
        // Write as UTF-8 (fallback)
        fs::write(&temp_config, modified_content.as_bytes()).map_err(|e| format!("Failed to write modified policy: {}", e))?;

        log::info!("Wrote policy file as UTF-8 to: {}", temp_config_str);
    }

    // Step 4: Import the modified policy
    let output = secedit()
        .args(["/configure", "/db", temp_db_str, "/cfg", temp_config_str])
        .output()
        .map_err(|e| format!("Failed to run secedit configure: {}", e))?;

    // Clean up temporary files
    let _ = fs::remove_file(&temp_config);
    let _ = fs::remove_file(&temp_db);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let exit_code = output.status.code().unwrap_or(-1);

        log::warn!("secedit configure failed with exit code {}", exit_code);
        log::warn!("stdout: {}", stdout);
        log::warn!("stderr: {}", stderr);

        return Err(format!(
            "secedit configure failed (exit code {}). This is normal on Windows Server 2025.\n\
                           The privilege has been added to the policy file, but requires:\n\
                           1. Run 'gpupdate /force' as Administrator, OR\n\
                           2. Restart your session (logoff/logon), OR\n\
                           3. Restart the system\n\
                           Then run TMR again.",
            exit_code
        ));
    }

    log::info!("Successfully applied security policy with Large Page privilege");
    Ok(())
}

fn modify_security_policy(content: &str, username: &str) -> Result<String, String> {
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let mut found_privilege_line = false;

    // Look for existing SeLockMemoryPrivilege line
    for line in &mut lines {
        if line.starts_with("SeLockMemoryPrivilege") {
            found_privilege_line = true;

            // Check if user is already in the list
            if !line.contains(username) {
                // Add the user to the existing line
                *line = format!("{},{}", line, username);
                log::info!("Added {} to existing SeLockMemoryPrivilege line", username);
            } else {
                log::info!("User {} already has SeLockMemoryPrivilege", username);
            }
            break;
        }
    }

    // If no existing line found, add a new one in the [Privilege Rights] section
    if !found_privilege_line {
        let mut in_privilege_section = false;
        let mut privilege_section_found = false;

        for (i, line) in lines.iter().enumerate() {
            if line.trim() == "[Privilege Rights]" {
                in_privilege_section = true;
                privilege_section_found = true;
                continue;
            }

            if in_privilege_section && line.starts_with('[') {
                // End of privilege section, insert here
                lines.insert(i, format!("SeLockMemoryPrivilege = {}", username));
                log::info!("Added new SeLockMemoryPrivilege line for {}", username);
                break;
            }
        }

        // If we didn't find [Privilege Rights] section, add it
        if !privilege_section_found {
            lines.push(String::new());
            lines.push("[Privilege Rights]".to_string());
            lines.push(format!("SeLockMemoryPrivilege = {}", username));
            log::info!("Created new [Privilege Rights] section with SeLockMemoryPrivilege for {}", username);
        }
    }

    Ok(lines.join("\r\n"))
}

/// Whether the security policy assigns SeLockMemoryPrivilege to any account: a `secedit /export`, read and deleted
fn check_if_privilege_in_policy() -> Result<bool, String> {
    // In TEMP, as the grant's export is: not the current folder, which an elevated prompt has as System32 (TODO 104)
    let export = std::env::temp_dir().join("tmr_quick_check.inf");
    let output =
        secedit().arg("/export").arg("/cfg").arg(&export).arg("/quiet").output().map_err(|e| format!("Failed to export policy: {}", e))?;

    if !output.status.success() {
        return Err("Could not export security policy".to_string());
    }

    // The export is the whole security policy: gone before anything can return early
    let bytes = std::fs::read(&export).map_err(|e| format!("Failed to read policy file: {}", e));
    let _ = std::fs::remove_file(&export);
    let bytes = bytes?;

    let content = if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        // Decode UTF-16 LE (skip 2-byte BOM) — see the note in the other decode site: fixed-size [u8; 2] chunks avoid per-index bounds
        // checks; `.0` drops an odd trailing byte.
        let utf16_data: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|chunk| u16::from_le_bytes(*chunk)).collect();
        String::from_utf16(&utf16_data).map_err(|_| "Invalid UTF-16 data in policy file".to_string())?
    } else {
        // Assume UTF-8 or ASCII
        String::from_utf8_lossy(&bytes).into_owned()
    };

    // Simple check: does SeLockMemoryPrivilege line exist with any assignments?
    let has_privilege = content.lines().any(|line| {
        line.trim().starts_with("SeLockMemoryPrivilege") && line.contains('=') && !line.split('=').nth(1).unwrap_or("").trim().is_empty()
    });

    Ok(has_privilege)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// The large-page question (TODO 29): none when the token has the right, enabled or not, or when TMR isn't elevated, and the policy
    /// left unread then; elevated without it, the restart when the policy assigns it, else the grant, to the account TMR runs as. Stand-ins
    /// for the policy and the account: no test reads or changes the security policy.
    #[test]
    fn the_grant_is_offered_only_elevated_and_without_the_right_in_the_policy() {
        let ask = |in_token, elevated, in_policy: bool| {
            let read = Cell::new(false);
            let question = question_for(
                in_token,
                elevated,
                || {
                    read.set(true);
                    in_policy
                },
                || r"PC\owner".to_string(),
            );
            (question, read.get())
        };
        assert_eq!(ask(true, true, false), (None, false), "the right in the token");
        assert_eq!(ask(true, false, false), (None, false), "the right in the token, not elevated");
        assert_eq!(ask(false, false, false), (None, false), "not elevated: the diagnostic says what to do");
        assert_eq!(ask(false, true, true), (Some(LargePageQuestion::Restart), true), "the right in the policy: only the restart missing");
        assert_eq!(ask(false, true, false), (Some(LargePageQuestion::Grant { account: r"PC\owner".to_string() }), true), "nowhere");
    }
}
