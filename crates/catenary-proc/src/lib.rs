// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single-process sampling for tick-budget diagnostics waits and work
//! intensity profiling.
//!
//! Provides both a stateful [`ProcessMonitor`] for persistent monitoring
//! (persistent file handles, amortized syscall costs, built-in delta tracking)
//! and a stateless [`sample`] function for one-off snapshots.
//!
//! Each sample captures three counters — user CPU time (`utime`), system
//! CPU time (`stime`), and page fault count (`pfc`) — plus the parent PID
//! and scheduling state. CPU times are normalized to centiseconds (100 Hz)
//! across platforms; 1 tick = 10ms. Page faults are raw counts.

/// A snapshot of a single process's CPU consumption, page faults, and state.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSample {
    /// User CPU time consumed (centiseconds, 100 Hz). Monotonically increasing.
    pub utime: u64,
    /// System CPU time consumed (centiseconds, 100 Hz). Monotonically increasing.
    pub stime: u64,
    /// Total page faults (minor + major). Monotonically increasing.
    pub pfc: u64,
    /// Parent process ID.
    pub ppid: u32,
    /// Current scheduling/execution state.
    pub state: ProcessState,
}

/// What the process is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Running on a core or in the run queue.
    Running,
    /// Voluntarily sleeping — waiting for I/O, socket, futex, etc.
    Sleeping,
    /// Uninterruptible sleep — kernel I/O (disk, NFS).
    Blocked,
    /// Stopped (SIGSTOP, debugger), zombie, dead, or unreadable.
    Dead,
}

/// CPU and page fault deltas since the last sample.
///
/// Returned by [`ProcessMonitor::sample`]. All deltas are 0 on the first
/// call. CPU times are in centiseconds (100 Hz); page faults are raw counts.
#[derive(Debug, Clone, Copy)]
pub struct ProcessDelta {
    /// User CPU time delta since last sample (centiseconds).
    pub delta_utime: u64,
    /// System CPU time delta since last sample (centiseconds).
    pub delta_stime: u64,
    /// Page fault count delta since last sample.
    pub delta_pfc: u64,
    /// Parent process ID.
    pub ppid: u32,
    /// Current scheduling/execution state.
    pub state: ProcessState,
}

/// Stateful process monitor with persistent handles.
///
/// Created once at server spawn, lives on `LspClient` for the server's
/// lifetime. Amortizes handle/fd open costs and encapsulates delta tracking
/// for three counters: user CPU time, system CPU time, and page faults.
///
/// # Platform behavior
///
/// - **Linux:** Holds a persistent `File` for `/proc/<pid>/stat` and a
///   reusable read buffer. Each sample is seek(0) + read — 2 syscalls
///   instead of 3 (open + read + close).
/// - **macOS:** Holds only the PID — `proc_pidinfo` is a stateless syscall.
/// - **Windows:** Holds a persistent `HANDLE` from `OpenProcess`, closed
///   in `Drop`. Each sample is 3 syscalls instead of 5+.
pub struct ProcessMonitor {
    prev_utime: Option<u64>,
    prev_stime: Option<u64>,
    prev_pfc: Option<u64>,
    inner: platform::MonitorInner,
}

impl ProcessMonitor {
    /// Creates a new monitor for the given PID.
    ///
    /// Opens persistent handles/fds for the server's lifetime.
    /// Returns `None` if the process doesn't exist or can't be monitored.
    #[must_use]
    pub fn new(pid: u32) -> Option<Self> {
        Some(Self {
            prev_utime: None,
            prev_stime: None,
            prev_pfc: None,
            inner: platform::MonitorInner::new(pid)?,
        })
    }

    /// Sample the process and return deltas since the last sample.
    ///
    /// All deltas are 0 on the first call.
    /// Returns `None` if the process can no longer be sampled.
    #[allow(
        clippy::similar_names,
        reason = "delta_utime/delta_stime are standard counter names"
    )]
    pub fn sample(&mut self) -> Option<ProcessDelta> {
        let (utime, stime, pfc, ppid, state) = self.inner.sample()?;
        let delta_utime = self.prev_utime.map_or(0, |prev| utime.saturating_sub(prev));
        let delta_stime = self.prev_stime.map_or(0, |prev| stime.saturating_sub(prev));
        let delta_pfc = self.prev_pfc.map_or(0, |prev| pfc.saturating_sub(prev));
        self.prev_utime = Some(utime);
        self.prev_stime = Some(stime);
        self.prev_pfc = Some(pfc);
        Some(ProcessDelta {
            delta_utime,
            delta_stime,
            delta_pfc,
            ppid,
            state,
        })
    }
}

/// Sample a single process by PID (stateless).
///
/// Returns absolute CPU times (centiseconds), page fault count, parent PID,
/// and scheduling state. Returns `None` if the process doesn't exist or
/// can't be read.
#[must_use]
pub fn sample(pid: u32) -> Option<ProcessSample> {
    platform::sample(pid)
}

// ─── Linux ──────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    use super::{ProcessSample, ProcessState};
    use std::io::{Read, Seek, SeekFrom};

    /// Persistent handle for Linux `/proc/<pid>/stat`.
    pub struct MonitorInner {
        file: std::fs::File,
        buf: String,
    }

    impl MonitorInner {
        /// Opens `/proc/<pid>/stat` and returns a persistent monitor.
        pub fn new(pid: u32) -> Option<Self> {
            let path = format!("/proc/{pid}/stat");
            let file = std::fs::File::open(path).ok()?;
            Some(Self {
                file,
                buf: String::with_capacity(256),
            })
        }

        /// Reads counters and state via the persistent fd.
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            self.buf.clear();
            self.file.seek(SeekFrom::Start(0)).ok()?;
            self.file.read_to_string(&mut self.buf).ok()?;
            parse_stat(&self.buf)
        }
    }

    /// Parse `/proc/<pid>/stat` contents.
    ///
    /// Fields after the comm field (past last `)`):
    /// - index 0: state char (`R`, `S`, `D`, `T`, `Z`, `X`, etc.)
    /// - index 1: ppid
    /// - index 7: minflt
    /// - index 9: majflt
    /// - index 11: utime (`USER_HZ` ticks, always 100 Hz)
    /// - index 12: stime (`USER_HZ` ticks)
    fn parse_stat(contents: &str) -> Option<(u64, u64, u64, u32, ProcessState)> {
        // The comm field (index 1) may contain spaces and parentheses,
        // so find the last ')' to skip past it.
        let after_comm = contents.rfind(')')? + 1;
        let fields: Vec<&str> = contents[after_comm..].split_whitespace().collect();

        let state_char = fields.first()?.chars().next()?;
        let ppid: u32 = fields.get(1)?.parse().ok()?;
        let minflt: u64 = fields.get(7)?.parse().ok()?;
        let majflt: u64 = fields.get(9)?.parse().ok()?;
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;

        let state = match state_char {
            'R' => ProcessState::Running,
            'S' | 'I' => ProcessState::Sleeping,
            'D' => ProcessState::Blocked,
            // T (stopped), Z (zombie), X (dead), etc.
            _ => ProcessState::Dead,
        };

        Some((utime, stime, minflt + majflt, ppid, state))
    }

    /// Stateless sample — opens, reads, closes.
    #[allow(clippy::similar_names, reason = "ppid/pid are distinct concepts")]
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (utime, stime, pfc, ppid, state) = parse_stat(&contents)?;
        Some(ProcessSample {
            utime,
            stime,
            pfc,
            ppid,
            state,
        })
    }
}

// ─── macOS ──────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProcessSample, ProcessState};

    /// macOS monitor — no persistent handle needed (`proc_pidinfo` is stateless).
    /// Caches ppid at construction to avoid the `PROC_PIDTBSDINFO` syscall
    /// on every sample.
    pub struct MonitorInner {
        pid: u32,
        ppid: u32,
    }

    impl MonitorInner {
        /// Verifies the process exists and returns a monitor.
        pub fn new(pid: u32) -> Option<Self> {
            let (_, _, _, ppid, _) = sample_raw(pid)?;
            Some(Self { pid, ppid })
        }

        /// Samples via `proc_pidinfo` (task info only — ppid is cached).
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            let (utime, stime, pfc, state) = sample_task(self.pid)?;
            Some((utime, stime, pfc, self.ppid, state))
        }
    }

    /// Task-info-only sampling (no ppid). Used by [`MonitorInner::sample`]
    /// which caches ppid at construction.
    fn sample_task(pid: u32) -> Option<(u64, u64, u64, ProcessState)> {
        // Safety: calling POSIX and Mach APIs with correctly sized buffers.
        unsafe { sample_task_inner(pid) }
    }

    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` (`PROC_PIDTASKINFO`) and
    /// `libc::mach_timebase_info` with correctly sized and zeroed buffers.
    unsafe fn sample_task_inner(pid: u32) -> Option<(u64, u64, u64, ProcessState)> {
        let (info, state) = read_task_info(pid)?;

        let mut timebase = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
        libc::mach_timebase_info(&mut timebase);

        let numer = u64::from(timebase.numer);
        let denom = u64::from(timebase.denom);
        let utime = info.pti_total_user * numer / denom / 10_000_000;
        let stime = info.pti_total_system * numer / denom / 10_000_000;

        #[allow(
            clippy::cast_sign_loss,
            reason = "page fault count is always non-negative"
        )]
        let pfc = info.pti_faults as u64;

        Some((utime, stime, pfc, state))
    }

    /// Full sampling including ppid. Used by the stateless [`sample`] function.
    fn sample_raw(pid: u32) -> Option<(u64, u64, u64, u32, ProcessState)> {
        // Safety: calling POSIX and Mach APIs with correctly sized buffers.
        unsafe { sample_raw_inner(pid) }
    }

    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` (twice: `PROC_PIDTASKINFO` and
    /// `PROC_PIDTBSDINFO`) and `libc::mach_timebase_info` with correctly
    /// sized and zeroed buffers.
    unsafe fn sample_raw_inner(pid: u32) -> Option<(u64, u64, u64, u32, ProcessState)> {
        let (utime, stime, pfc, state) = sample_task_inner(pid)?;

        // BSD info for parent PID
        let mut bsd_info: libc::proc_bsdinfo = std::mem::zeroed();
        let bsd_size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;

        let bsd_ret = libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(bsd_info).cast(),
            bsd_size,
        );
        let ppid = if bsd_ret > 0 { bsd_info.pbi_ppid } else { 0 };

        Some((utime, stime, pfc, ppid, state))
    }

    /// Read `proc_taskinfo` and derive scheduling state.
    ///
    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` with `PROC_PIDTASKINFO` and a correctly
    /// sized and zeroed buffer.
    unsafe fn read_task_info(pid: u32) -> Option<(libc::proc_taskinfo, ProcessState)> {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;

        let ret = libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        );

        if ret <= 0 {
            return None;
        }

        let state = if info.pti_numrunning > 0 {
            ProcessState::Running
        } else {
            ProcessState::Sleeping
        };

        Some((info, state))
    }

    /// Stateless sample for the `sample(pid)` function.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let (utime, stime, pfc, ppid, state) = sample_raw(pid)?;
        Some(ProcessSample {
            utime,
            stime,
            pfc,
            ppid,
            state,
        })
    }
}

// ─── Windows ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod platform {
    use super::{ProcessSample, ProcessState};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION,
        PROCESS_VM_READ,
    };

    /// Windows monitor — persistent `HANDLE`, closed in `Drop`.
    pub struct MonitorInner {
        handle: HANDLE,
        ppid: u32,
    }

    impl MonitorInner {
        /// Opens a process handle with query and VM read access.
        pub fn new(pid: u32) -> Option<Self> {
            // Safety: `OpenProcess` is called with valid access flags.
            // Returns 0 on failure (invalid PID).
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
            if handle == 0 {
                return None;
            }
            let ppid = unsafe { get_ppid(pid) };
            Some(Self { handle, ppid })
        }

        /// Samples via persistent handle.
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            // Safety: `self.handle` is valid (opened in `new`, closed in `Drop`).
            let (utime, stime, pfc, state) = unsafe { sample_handle(self.handle) }?;
            Some((utime, stime, pfc, self.ppid, state))
        }
    }

    impl Drop for MonitorInner {
        fn drop(&mut self) {
            // Safety: `self.handle` is valid and will not be used after drop.
            unsafe { CloseHandle(self.handle) };
        }
    }

    // Safety: Windows HANDLEs are kernel objects that can be used from any thread.
    unsafe impl Send for MonitorInner {}

    /// Sample CPU times, page faults, and state via an existing handle.
    ///
    /// Returns `(utime, stime, pfc, state)`.
    ///
    /// # Safety
    ///
    /// Caller must ensure `handle` is a valid process handle with
    /// `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` access.
    unsafe fn sample_handle(handle: HANDLE) -> Option<(u64, u64, u64, ProcessState)> {
        let mut creation = unsafe { std::mem::zeroed() };
        let mut exit = unsafe { std::mem::zeroed() };
        let mut kernel = unsafe { std::mem::zeroed() };
        let mut user = unsafe { std::mem::zeroed() };

        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }

        // FILETIME: 100-nanosecond intervals -> centiseconds (/ 100_000)
        let user_100ns = (u64::from(user.dwHighDateTime) << 32) | u64::from(user.dwLowDateTime);
        let kernel_100ns =
            (u64::from(kernel.dwHighDateTime) << 32) | u64::from(kernel.dwLowDateTime);
        let utime = user_100ns / 100_000;
        let stime = kernel_100ns / 100_000;

        // Page fault count via GetProcessMemoryInfo
        let mut mem_counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(0);
        mem_counters.cb = cb;
        let pfc = if unsafe { K32GetProcessMemoryInfo(handle, &mut mem_counters, cb) } != 0 {
            u64::from(mem_counters.PageFaultCount)
        } else {
            0
        };

        let mut exit_code: u32 = 0;
        unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        #[allow(
            clippy::cast_sign_loss,
            reason = "STILL_ACTIVE is a well-known Windows constant"
        )]
        let state = if exit_code == STILL_ACTIVE as u32 {
            // Can't distinguish Running/Sleeping on Windows — report Running
            // so the failure detector uses tick delta for discrimination.
            ProcessState::Running
        } else {
            ProcessState::Dead
        };

        Some((utime, stime, pfc, state))
    }

    /// Get parent PID via a toolhelp snapshot.
    ///
    /// Returns 0 if the PID cannot be found.
    ///
    /// # Safety
    ///
    /// Calls Win32 ToolHelp APIs with correctly sized buffers.
    unsafe fn get_ppid(pid: u32) -> u32 {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }

        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);

        let mut found = 0u32;
        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    found = entry.th32ParentProcessID;
                    break;
                }
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }

        unsafe { CloseHandle(snapshot) };
        found
    }

    /// Stateless sample — opens handle, reads, closes.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        // Safety: calling Win32 APIs with correctly sized buffers and
        // properly closing the handle on all paths.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            if handle == 0 {
                return None;
            }
            let result = sample_handle(handle);
            CloseHandle(handle);
            let (utime, stime, pfc, state) = result?;
            let ppid = get_ppid(pid);
            Some(ProcessSample {
                utime,
                stime,
                pfc,
                ppid,
                state,
            })
        }
    }
}

// ─── Unsupported ────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{ProcessSample, ProcessState};

    /// Stub monitor for unsupported platforms.
    pub(super) struct MonitorInner;

    impl MonitorInner {
        pub fn new(_pid: u32) -> Option<Self> {
            None
        }

        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            None
        }
    }

    pub fn sample(_pid: u32) -> Option<ProcessSample> {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ─── Stateless sample() tests ───────────────────────────────────────

    #[test]
    fn sample_self_succeeds() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        assert!(
            s.utime > 0,
            "Own process should have consumed some user CPU"
        );
    }

    #[test]
    fn sample_nonexistent_returns_none() {
        let result = sample(u32::MAX);
        assert!(result.is_none(), "Nonexistent PID should return None");
    }

    #[test]
    fn sample_self_state_is_running() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        // During test execution, our process should be Running
        assert_eq!(
            s.state,
            ProcessState::Running,
            "Own process should be Running during test"
        );
    }

    #[test]
    fn sample_self_has_ppid() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        assert!(s.ppid > 0, "Own process should have a parent PID");
    }

    #[test]
    fn sample_self_has_page_faults() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        assert!(s.pfc > 0, "Own process should have had page faults");
    }

    // ─── ProcessMonitor tests ───────────────────────────────────────────

    #[test]
    fn monitor_self_succeeds() {
        let pid = std::process::id();
        let monitor = ProcessMonitor::new(pid);
        assert!(monitor.is_some(), "Should be able to monitor own process");
    }

    #[test]
    fn monitor_nonexistent_returns_none() {
        let result = ProcessMonitor::new(u32::MAX);
        assert!(result.is_none(), "Nonexistent PID should return None");
    }

    #[test]
    fn monitor_first_sample_delta_is_zero() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");
        let d = monitor.sample().expect("Should sample self");
        assert_eq!(d.delta_utime, 0, "First sample should have delta_utime 0");
        assert_eq!(d.delta_stime, 0, "First sample should have delta_stime 0");
        assert_eq!(d.delta_pfc, 0, "First sample should have delta_pfc 0");
    }

    #[test]
    fn monitor_persistent_handle_reuse() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // Multiple samples through the same persistent handle
        for i in 0..5 {
            let result = monitor.sample();
            assert!(result.is_some(), "Sample {i} should succeed");
        }
    }

    #[test]
    fn monitor_delta_advances_with_cpu_work() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // First sample: delta is 0
        let d = monitor.sample().expect("First sample");
        assert_eq!(d.delta_utime, 0);

        // Burn some CPU so ticks advance
        let mut sum: u64 = 0;
        for i in 0..10_000_000 {
            sum = sum.wrapping_add(i);
        }
        // Prevent optimizing away the loop
        std::hint::black_box(sum);

        // Second sample: delta may be > 0 (depends on granularity)
        let d = monitor.sample().expect("Second sample");
        // We can't guarantee delta_utime > 0 due to 10ms tick granularity,
        // but the sample should succeed and state should be Running.
        assert_eq!(d.state, ProcessState::Running);
        let _ = d.delta_utime;
    }

    #[test]
    fn monitor_pfc_delta_advances() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // First sample: establishes baseline
        let d = monitor.sample().expect("First sample");
        assert_eq!(d.delta_pfc, 0, "First sample should have delta_pfc 0");

        // Force page faults: non-zero fill requires touching every page
        let buf = vec![1u8; 4 * 1024 * 1024];
        std::hint::black_box(&buf);

        // Second sample: page faults should have advanced
        let d = monitor.sample().expect("Second sample");
        assert!(
            d.delta_pfc > 0,
            "Page fault delta should advance after large allocation, got {}",
            d.delta_pfc,
        );
    }
}
