//! Windows Job Object containment: closing the returned handle kills the process and every descendant.
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::System::{JobObjects::*, Threading::*};

/// Put `pid` in a new kill-on-close job. Keep the handle alive for as long as the process tree must live;
/// `subject` names the owner in error messages.
pub fn contain(pid: u32, subject: &str) -> Result<OwnedHandle, String> {
    unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return Err(format!("Create {subject} job: {}", std::io::Error::last_os_error()));
        }
        let job = OwnedHandle::from_raw_handle(raw);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of_val(&limits) as u32,
        ) == 0
        {
            return Err(format!("Set {subject} job limits: {}", std::io::Error::last_os_error()));
        }
        let raw_process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if raw_process.is_null() {
            return Err(format!("Open {subject} process: {}", std::io::Error::last_os_error()));
        }
        let process = OwnedHandle::from_raw_handle(raw_process);
        if AssignProcessToJobObject(raw, process.as_raw_handle()) == 0 {
            return Err(format!("Assign {subject} job: {}", std::io::Error::last_os_error()));
        }
        Ok(job)
    }
}
