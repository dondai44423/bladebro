//! Kernel-owned cleanup when Windows terminates the driver without destructors.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

// Win32 JOBOBJECT_BASIC_LIMIT_INFORMATION / JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    process_time: i64,
    job_time: i64,
    flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_processes: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io_counters: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, size: u32)
        -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

/// Join before spawning Chrome so every descendant inherits the job, without
/// a spawn/assignment race. Windows force-termination skips Drop and signals;
/// the kernel closes our non-inheritable job handle and kills the owned tree.
pub(crate) fn guard_browser_process_tree() -> crate::Result<()> {
    // Keep the handle for the PROCESS lifetime: closing it while we are alive
    // would also kill us. Static destructors do not run; the OS closes it even
    // on TerminateProcess. Nested jobs work on supported Windows versions.
    static JOB: OnceLock<std::result::Result<OwnedHandle, String>> = OnceLock::new();
    let job = JOB.get_or_init(|| unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return Err(format!(
                "CreateJobObjectW: {}",
                std::io::Error::last_os_error()
            ));
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let mut limits = ExtendedLimits::default();
        limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if SetInformationJobObject(
            handle.as_raw_handle(),
            9, // JobObjectExtendedLimitInformation
            &limits as *const _ as *const c_void,
            std::mem::size_of::<ExtendedLimits>() as u32,
        ) == 0
            || AssignProcessToJobObject(handle.as_raw_handle(), GetCurrentProcess()) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(handle)
    });
    job.as_ref().map(|_| ()).map_err(|error| {
        crate::BladeError::Other(format!("cannot guard the browser process tree: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn hard_exit_cleans_owned_children_only() {
        const ROLE: &str = "BLADE_JOB_TEST_ROLE";
        const PID_FILE: &str = "BLADE_JOB_TEST_PID_FILE";
        if let Ok(role) = std::env::var(ROLE) {
            if role == "guarded" {
                guard_browser_process_tree().unwrap();
                guard_browser_process_tree().unwrap(); // idempotent, no nested re-assignment
            }
            let mut child = Command::new("cmd.exe")
                .args(["/C", "ping -t 127.0.0.1 >nul"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::fs::write(std::env::var(PID_FILE).unwrap(), child.id().to_string()).unwrap();
            let _ = child.wait();
            return;
        }

        // A negative control proves this assertion detects the original leak;
        // then two fresh owned jobs must clean up without touching the control.
        for guarded in [false, true, true] {
            let marker = std::env::temp_dir().join(format!(
                "blade-job-{}-{}.pid",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut fixture = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::windows_job::tests::hard_exit_cleans_owned_children_only",
                    "--nocapture",
                ])
                .env(ROLE, if guarded { "guarded" } else { "unguarded" })
                .env(PID_FILE, &marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                {
                    break pid;
                }
                if Instant::now() >= deadline {
                    super::super::kill_process_force(fixture.id());
                    fixture.wait().unwrap();
                    panic!("owned child did not report its pid");
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let initially_alive = super::super::process_alive(pid);
            let mut unrelated = Command::new("cmd.exe")
                .args(["/C", "ping -t 127.0.0.1 >nul"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            fixture.kill().unwrap();
            fixture.wait().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while guarded && super::super::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let alive = super::super::process_alive(pid);
            if alive {
                super::super::kill_process_force(pid);
            }
            let unrelated_alive = unrelated.try_wait().unwrap().is_none();
            super::super::kill_process_force(unrelated.id());
            unrelated.wait().unwrap();
            std::fs::remove_file(marker).unwrap();
            assert!(
                initially_alive,
                "fixture child must be alive before termination"
            );
            assert_eq!(
                alive, !guarded,
                "only the guarded process tree must die on hard exit"
            );
            assert!(unrelated_alive, "the unrelated control must survive");
        }
    }
}
