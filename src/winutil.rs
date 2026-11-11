//! Small Win32 helpers.

use std::os::windows::io::AsRawHandle;
use std::os::windows::ffi::OsStrExt;
use std::process::Child;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE,
    SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SM_CMONITORS,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn error_box(text: &str) {
    let (t, c) = (wide(text), wide("Aero"));
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONERROR) };
}

pub fn beep(ok: bool) {
    unsafe { MessageBeep(if ok { MB_ICONINFORMATION } else { MB_ICONERROR }) };
}

/// Returns false if another Aero instance is already running.
pub fn single_instance() -> bool {
    let name = wide("Local\\AeroClipper.SingleInstance");
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // Intentionally leaked: the mutex lives as long as the process.
        !(h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS)
    }
}

/// A job object that kills its processes when Aero exits, even if Aero crashes,
/// so ffmpeg never keeps recording in the background on its own.
pub struct KillOnCloseJob(HANDLE);

unsafe impl Send for KillOnCloseJob {}

impl KillOnCloseJob {
    pub fn new() -> Option<Self> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            Some(Self(h))
        }
    }

    pub fn assign(&self, child: &Child) {
        unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Lets a second launch of Aero ask the running one to show its window.
pub struct ShowSignal(HANDLE);

unsafe impl Send for ShowSignal {}
unsafe impl Sync for ShowSignal {}

impl ShowSignal {
    const NAME: &str = r"Local\AeroClipper.Show";

    pub fn create() -> Option<Self> {
        let name = wide(Self::NAME);
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        (!h.is_null()).then_some(Self(h))
    }

    /// Called by a second instance.
    pub fn signal_running_instance() {
        let name = wide(Self::NAME);
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
            if !h.is_null() {
                SetEvent(h);
                CloseHandle(h);
            }
        }
    }

    /// Blocks until another instance signals. Returns false on error.
    pub fn wait(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
    }
}

/// Moves a file to the Recycle Bin (undoable, unlike a plain delete).
pub fn recycle(path: &std::path::Path) -> bool {
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]); // double-null-terminated list
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as _;
    unsafe { SHFileOperationW(&mut op) == 0 }
}

pub fn monitor_count() -> u32 {
    unsafe { GetSystemMetrics(SM_CMONITORS).max(1) as u32 }
}

/// Waits (up to `timeout`) for the process with this PID to exit. Returns at once if it's gone.
pub fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) {
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, timeout.as_millis() as u32);
            CloseHandle(h);
        }
    }
}
