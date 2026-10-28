//! Windows' own dialogs and the shell, for the window's command bar (TODO 109): the Open dialog, for a config and for the two results files
//! Compare takes, and opening a file or a folder as Explorer does. Reactor 0.100.0 has no file dialog (nor has its master), so the `windows`
//! crate's IFileOpenDialog, owned by the window.
//!
//! Each runs on a thread of its own, in a single-threaded apartment of its own, which the window's thread-pool waiter joins: on the window's
//! thread the dialog's modal loop would pump the window's messages inside `update`, so Reactor's next message would arrive while it still
//! handled the last, and ShellExecute may load shell extensions, which want an apartment, and take a while.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

/// The window's handle, for a dialog it owns: the active window of the window's thread, which asks from `update` as a button is clicked.
/// An integer, to cross to the dialog's thread.
pub fn owner() -> isize {
    // SAFETY: no arguments; it reads the calling thread's own state
    unsafe { GetActiveWindow() }.0 as isize
}

/// What the Open dialog asks for
pub struct Open {
    pub title: &'static str,
    /// The filter's name and its patterns: `("TMR and TM5 configs", "*.cfg;*.json")`
    pub filter: (&'static str, &'static str),
    /// Where it opens
    pub folder: PathBuf,
    pub multiple: bool,
}

/// `work` on a thread of its own, `name`d, in a single-threaded apartment of its own; what it gave, or why it couldn't
fn in_apartment<T: Send + 'static>(name: &str, work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let thread = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        // SAFETY: this thread's apartment, which `Apartment` ends with the thread's work
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok().map_err(|e| e.message())?;
        let _apartment = Apartment;
        work()
    });
    thread.map_err(|e| format!("{name} didn't start: {e}"))?.join().map_err(|_| format!("{name} failed"))?
}

/// The Open dialog, owned by `owner`, on a thread of its own: the files chosen, none when it was cancelled, or why it couldn't open
pub fn open(owner: isize, ask: Open) -> Result<Vec<PathBuf>, String> {
    in_apartment("the Open dialog", move || open_here(owner, &ask))
}

/// The Open dialog, on this thread, which has an apartment
fn open_here(owner: isize, ask: &Open) -> Result<Vec<PathBuf>, String> {
    let failed = |e: windows::core::Error| e.message();
    // SAFETY: COM is initialised on this thread
    let dialog: IFileOpenDialog = unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }.map_err(failed)?;
    let (name, patterns, title) = (HSTRING::from(ask.filter.0), HSTRING::from(ask.filter.1), HSTRING::from(ask.title));
    let filters = [COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(patterns.as_ptr()) }, COMDLG_FILTERSPEC {
        pszName: w!("All files"),
        pszSpec: w!("*.*"),
    }];
    // SAFETY: the filters' and the title's strings outlive the calls, which copy them; the dialog is this thread's
    unsafe {
        dialog.SetFileTypes(&filters).map_err(failed)?;
        dialog.SetTitle(&title).map_err(failed)?;
        let mut options = dialog.GetOptions().map_err(failed)? | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM;
        if ask.multiple {
            options |= FOS_ALLOWMULTISELECT;
        }
        dialog.SetOptions(options).map_err(failed)?;
        // A folder that isn't there leaves the dialog where it last was
        if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(ask.folder.as_os_str()), None) {
            let _ = dialog.SetFolder(&folder);
        }
    }
    // SAFETY: `owner` is the window's handle, or 0 for none; the dialog is this thread's
    match unsafe { dialog.Show(Some(HWND(owner as *mut _))) } {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => return Ok(Vec::new()),
        Err(e) => return Err(e.message()),
    }
    let mut paths = Vec::new();
    // SAFETY: the dialog's results, read on its thread; each name is freed once, after it is copied
    unsafe {
        let items = dialog.GetResults().map_err(failed)?;
        for i in 0..items.GetCount().map_err(failed)? {
            let name = items.GetItemAt(i).and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH)).map_err(failed)?;
            let path = name.to_string();
            CoTaskMemFree(Some(name.0 as *const _));
            paths.push(PathBuf::from(path.map_err(|e| e.to_string())?));
        }
    }
    Ok(paths)
}

/// This thread's COM apartment, ended when the dialog is done
struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with the `CoInitializeEx` that made it, on the same thread
        unsafe { CoUninitialize() };
    }
}

/// Opens `path` as Explorer does, on a thread of its own: a folder in Explorer, a file in the app Windows has for its kind (a `.log` or
/// `.json` in Notepad unless the user chose another)
pub fn shell_open(path: PathBuf) -> Result<(), String> {
    in_apartment("opening a file", move || shell_open_here(&path))
}

/// `shell_open`, on this thread, which has an apartment
fn shell_open_here(path: &Path) -> Result<(), String> {
    let file = HSTRING::from(path.as_os_str());
    // SAFETY: the strings outlive the call; no owner window
    let result = unsafe { ShellExecuteW(None, w!("open"), &file, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Above 32 is success (ShellExecute's own convention)
    match result.0 as isize {
        code if code > 32 => Ok(()),
        code => Err(format!("Windows couldn't open {} (ShellExecute error {code})", path.display())),
    }
}
