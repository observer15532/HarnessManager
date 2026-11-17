//! Thin, Windows-only Win32 helpers.
//!
//! Handles are kept as `usize` so the code does not depend on how windows-sys
//! spells its handle aliases.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MessageBoxW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

pub const MB_OK: u32 = 0x0000_0000;
pub const MB_YESNO: u32 = 0x0000_0004;
pub const MB_ICONERROR: u32 = 0x0000_0010;
pub const MB_ICONQUESTION: u32 = 0x0000_0020;
pub const MB_SETFOREGROUND: u32 = 0x0001_0000;
pub const IDYES: i32 = 6;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A Windows job object holding the whole dsh process tree.
///
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means the tree dies when the last job
/// handle closes, so a launcher crash cannot leave an orphaned `dsh` behind.
pub struct Job {
    handle: usize,
}

impl Job {
    pub fn new() -> Option<Job> {
        unsafe {
            let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(raw);
                return None;
            }
            Some(Job {
                handle: raw as usize,
            })
        }
    }

    /// Attach a freshly spawned process (by its raw handle) to the job.
    pub fn assign(&self, process_handle: usize) -> bool {
        unsafe {
            AssignProcessToJobObject(self.handle as HANDLE, process_handle as HANDLE) != 0
        }
    }

    pub fn terminate(&self) -> bool {
        unsafe { TerminateJobObject(self.handle as HANDLE, 1) != 0 }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                CloseHandle(self.handle as HANDLE);
            }
            self.handle = 0;
        }
    }
}

/// Create the single-instance mutex. `None` means another instance owns it.
pub fn acquire_single_instance(name: &str) -> Option<usize> {
    let name = wide(name);
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == 183 {
            // ERROR_ALREADY_EXISTS
            CloseHandle(handle);
            return None;
        }
        Some(handle as usize)
    }
}

pub fn message_box(text: &str, caption: &str, flags: u32) -> i32 {
    let text = wide(text);
    let caption = wide(caption);
    unsafe {
        MessageBoxW(
            0 as HWND,
            text.as_ptr(),
            caption.as_ptr(),
            flags | MB_SETFOREGROUND,
        )
    }
}

/// Open a URL, a file, or a directory with its default handler.
pub fn shell_open(target: &Path) -> bool {
    let target = wide_path(target);
    let op = wide("open");
    unsafe {
        let result = ShellExecuteW(
            0 as HWND,
            op.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
        (result as isize) > 32
    }
}

pub fn shell_open_url(url: &str) -> bool {
    let target = wide(url);
    let op = wide("open");
    unsafe {
        let result = ShellExecuteW(
            0 as HWND,
            op.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
        (result as isize) > 32
    }
}

/// Drain every pending window message. This keeps the tray icon's hidden
/// window alive; without it the shell never delivers clicks or menu events.
pub fn pump_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, 0 as HWND, 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
