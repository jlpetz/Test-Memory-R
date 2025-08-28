use std::ffi::c_void;
use std::ptr::null_mut;
use std::process::Command;
use std::fs;
use encoding_rs::*;
use windows::Win32::Foundation::{HANDLE, LUID, GetLastError, WIN32_ERROR, CloseHandle};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, 
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    GetTokenInformation, TokenPrivileges, TOKEN_PRIVILEGES,
};
use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_LARGE_PAGES, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Diagnostics::Debug::{FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM};
use windows::core::PWSTR;

#[derive(Debug, PartialEq)]
enum PrivilegeState {
    NotAssigned,
    AssignedButDisabled,
    AssignedAndEnabled,
}

fn format_win32_error(err: WIN32_ERROR) -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM,
            None,
            err.0,
            0,
            PWSTR(buffer.as_mut_ptr()),
            buffer.len() as u32,
            None,
        )
    };
    if len == 0 {
        format!("Unknown error {}", err.0)
    } else {
        String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string()
    }
}


pub fn check_large_page_privilege() -> Result<(), &'static str> {
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
        let ptr = VirtualAlloc(
            Some(null_mut()),
            test_size,
            MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
            PAGE_READWRITE,
        );

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

unsafe fn check_privilege_in_token(token: HANDLE, privilege_luid: LUID) -> PrivilegeState {
    use windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES;
    
    // Get the size needed for token privileges
    let mut return_length = 0u32;
    let _ = GetTokenInformation(
        token,
        TokenPrivileges,
        None,
        0,
        &mut return_length,
    );

    if return_length == 0 {
        return PrivilegeState::NotAssigned;
    }

    // Allocate buffer and get actual privileges
    let mut buffer = vec![0u8; return_length as usize];
    if GetTokenInformation(
        token,
        TokenPrivileges,
        Some(buffer.as_mut_ptr() as *mut c_void),
        return_length,
        &mut return_length,
    ).is_err() {
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
        
        if privilege_ptr.Luid.LowPart == privilege_luid.LowPart && 
           privilege_ptr.Luid.HighPart == privilege_luid.HighPart {
            
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

unsafe fn enable_privilege_in_token(token: HANDLE, privilege_luid: LUID) -> Result<(), &'static str> {
    use windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES;
    
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: privilege_luid,
            Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(SE_PRIVILEGE_ENABLED.0),
        }],
    };

    let result = AdjustTokenPrivileges(
        token, 
        false, 
        Some(&privileges), 
        0, 
        None, 
        None
    );

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
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    
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
    unsafe {
        let process = GetCurrentProcess();
        let mut token: HANDLE = HANDLE::default();

        // Open token with maximum permissions
        let token_access = TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY | 
                          windows::Win32::Security::TOKEN_DUPLICATE |
                          windows::Win32::Security::TOKEN_ASSIGN_PRIMARY;

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
                Attributes: windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES(SE_PRIVILEGE_ENABLED.0) | 
                           windows::Win32::Security::SE_PRIVILEGE_ENABLED_BY_DEFAULT,
            }],
        };

        let result = AdjustTokenPrivileges(
            token,
            false,
            Some(&privileges),
            std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
            None,
            None,
        );

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

pub fn auto_grant_large_page_privilege() -> Result<String, String> {
    if !is_elevated() {
        return Err("Administrator privileges required".to_string());
    }
    
    let username = std::env::var("USERNAME").unwrap_or_else(|_| "Administrator".to_string());
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| 
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "WORKGROUP".to_string())
    );
    let full_username = format!("{}\\{}", domain, username);
    
    log::info!("Attempting to grant SeLockMemoryPrivilege to: {}", full_username);
    
    match grant_privilege_via_secedit(&full_username) {
        Ok(()) => {
            Ok(format!("Successfully granted SeLockMemoryPrivilege to {}", full_username))
        }
        Err(e) => {
            // Provide more specific error context
            if e.contains("configure failed") && e.contains("privileges") {
                Err("Privilege assignment failed - may already be assigned (restart needed)".to_string())
            } else if e.contains("UTF-8") || e.contains("encoding") {
                Err("File encoding issue during policy modification".to_string())
            } else {
                Err(format!("Setup failed: {}", e))
            }
        }
    }
}

/// Enhanced large page setup with automatic privilege granting
pub fn setup_large_pages_automatically() -> Result<String, String> {
    let mut messages = Vec::new();
    
    // Show current user
    if let Ok(username) = std::env::var("USERNAME")
        && let Ok(domain) = std::env::var("USERDOMAIN") {
            messages.push(format!("👤 User: {}\\{}", domain, username));
        }
    
    match check_privilege_state() {
        PrivilegeState::NotAssigned => {
            messages.push("❌ SeLockMemoryPrivilege: NOT in current session token".to_string());
            
            if is_elevated() {
                messages.push("✅ Running as Administrator - attempting automatic setup...".to_string());
                
                match auto_grant_large_page_privilege() {
                    Ok(success_msg) => {
                        messages.push(format!("✅ {}", success_msg));
                        messages.push("".to_string());
                        messages.push("🔄 RESTART REQUIRED: Restart TMR for changes to take effect".to_string());
                        return Ok(messages.join("\n"));
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
                        messages.push("2. Navigate to: Computer Configuration → Windows Settings → Security Settings → Local Policies → User Rights Assignment".to_string());
                        messages.push("3. Double-click 'Lock pages in memory'".to_string());
                        messages.push("4. Add your user account or 'Administrators' group".to_string());
                        messages.push("5. Restart TMR".to_string());
                    }
                }
            } else {
                messages.push("❌ Not running as Administrator".to_string());
                messages.push("".to_string());
                messages.push("🔧 SOLUTIONS (try in order):".to_string());
                messages.push("1. Restart TMR as Administrator for automatic setup".to_string());
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
    unsafe {
        let test_size = 2 * 1024 * 1024; // 2MB
        let ptr = VirtualAlloc(
            Some(null_mut()),
            test_size,
            MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
            PAGE_READWRITE,
        );

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

pub fn diagnose_and_setup_large_pages() -> String {
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
    
    // Attempt automatic setup
    let setup_successful = match setup_large_pages_automatically() {
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
    
    // Only show manual instructions if setup failed or has issues
    if !setup_successful {
        report.push_str("Manual Setup Instructions:\n");
        report.push_str("==========================\n");
        report.push_str("1. Run gpedit.msc as Administrator\n");
        report.push_str("2. Navigate to: Computer Configuration → Windows Settings → Security Settings → Local Policies → User Rights Assignment\n");
        report.push_str("3. Double-click 'Lock pages in memory'\n");
        report.push_str("4. Click 'Add User or Group'\n");
        report.push_str("5. Add your user account or 'Administrators' group\n");
        report.push_str("6. Click OK and restart TMR\n\n");
        
        report.push_str("Windows Server Notes:\n");
        report.push_str("- Windows Server has stricter large page requirements than desktop Windows\n");
        report.push_str("- Group Policy changes require logoff/logon or restart to take effect\n");
        report.push_str("- TMR will automatically fall back to regular pages if large pages fail\n");
    }
    report
}

fn grant_privilege_via_secedit(username: &str) -> Result<(), String> {
    let temp_config = "temp_secpol.inf";
    let temp_db = "temp_secpol.sdb";
    
    // Step 1: Export current security policy
    let output = Command::new("secedit")
        .args(["/export", "/cfg", temp_config])
        .output()
        .map_err(|e| format!("Failed to run secedit export: {}", e))?;
    
    if !output.status.success() {
        return Err(format!("secedit export failed: {}", 
            String::from_utf8_lossy(&output.stderr)));
    }
    
    // Step 2: Read and modify the policy file
	let bytes = fs::read(temp_config)
		.map_err(|e| format!("Failed to read security policy: {}", e))?;

	// Try UTF-16 LE first (most common for secedit), then Windows-1252
	let content = if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
		let (decoded, _, _) = UTF_16LE.decode(&bytes[2..]); // Skip BOM
		decoded.into_owned()
	} else {
		let (decoded, _, _) = WINDOWS_1252.decode(&bytes);
		decoded.into_owned()
	};
    
    let modified_content = modify_security_policy(&content, username)?;
    
    // Step 3: Write modified policy as UTF-8 with BOM (modern Windows likes this)
	let mut file_content = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
	file_content.extend_from_slice(modified_content.as_bytes());
	fs::write(temp_config, file_content)
		.map_err(|e| format!("Failed to write modified policy: {}", e))?;
    
    // Step 4: Import the modified policy
    let output = Command::new("secedit")
        .args(["/configure", "/db", temp_db, "/cfg", temp_config])
        .output()
        .map_err(|e| format!("Failed to run secedit configure: {}", e))?;
    
    // Clean up temporary files
    let _ = fs::remove_file(temp_config);
    let _ = fs::remove_file(temp_db);
    
    if !output.status.success() {
        return Err(format!("secedit configure failed: {}", 
            String::from_utf8_lossy(&output.stderr)));
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

pub fn check_restart_needed() -> bool {
    // If token shows NotAssigned but we're Administrator, check if privilege might be in policy
    match check_privilege_state() {
        PrivilegeState::NotAssigned => {
            if is_elevated() {
                // Quick check: try to export policy and see if SeLockMemoryPrivilege exists
                check_if_privilege_in_policy().unwrap_or_default()
            } else {
                false
            }
        }
        _ => false, // If privilege is in token (even disabled), no restart needed
    }
}

fn check_if_privilege_in_policy() -> Result<bool, String> {
    use std::process::Command;
    
    let output = Command::new("secedit")
        .args(["/export", "/cfg", "temp_quick_check.inf", "/quiet"])
        .output()
        .map_err(|e| format!("Failed to export policy: {}", e))?;
    
    if !output.status.success() {
        return Err("Could not export security policy".to_string());
    }
    
    let bytes = std::fs::read("temp_quick_check.inf")
        .map_err(|e| format!("Failed to read policy file: {}", e))?;
    
    let content = if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let (decoded, _, _) = UTF_16LE.decode(&bytes[2..]);
        decoded.into_owned()
    } else {
        let (decoded, _, _) = WINDOWS_1252.decode(&bytes);
        decoded.into_owned()
    };
    
    let _ = std::fs::remove_file("temp_quick_check.inf");
    
    // Simple check: does SeLockMemoryPrivilege line exist with any assignments?
    let has_privilege = content.lines()
        .any(|line| {
            line.trim().starts_with("SeLockMemoryPrivilege") && 
            line.contains('=') &&
            !line.split('=').nth(1).unwrap_or("").trim().is_empty()
        });
    
    Ok(has_privilege)
}