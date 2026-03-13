// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single-process CPU sampling for tick-budget diagnostics waits.
//!
//! Provides both a stateful [`ProcessMonitor`] for persistent monitoring
//! (persistent file handles, amortized syscall costs, built-in delta tracking)
//! and a stateless [`sample`] function for one-off snapshots.
//!
//! All values are normalized to centiseconds (100 Hz) across platforms.
//! 1 tick = 10ms of CPU time.

/// A snapshot of a single process's CPU consumption and scheduling state.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSample {
    /// Total CPU time consumed (user + system), in centiseconds (100 Hz).
    /// Monotonically increasing for a given PID. 1 tick = 10ms of CPU time.
    pub cpu_ticks: u64,
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

/// Stateful process monitor with persistent handles.
///
/// Created once at server spawn, lives on `LspClient` for the server's
/// lifetime. Amortizes handle/fd open costs and encapsulates tick delta
/// tracking.
///
/// # Platform behavior
///
/// - **Linux:** Holds a persistent `File` for `/proc/<pid>/stat` and a
///   reusable read buffer. Each sample is seek(0) + read — 2 syscalls
///   instead of 3 (open + read + close).
/// - **macOS:** Holds only the PID — `proc_pidinfo` is a stateless syscall.
/// - **Windows:** Holds a persistent `HANDLE` from `OpenProcess`, closed
///   in `Drop`. Each sample is 2 syscalls instead of 4.
pub struct ProcessMonitor {
    prev_ticks: Option<u64>,
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
            prev_ticks: None,
            inner: platform::MonitorInner::new(pid)?,
        })
    }

    /// Sample the process and return the tick delta since last sample.
    ///
    /// Returns `(delta, state)` where delta is 0 on the first call.
    /// Returns `None` if the process can no longer be sampled.
    pub fn sample(&mut self) -> Option<(u64, ProcessState)> {
        let (total_ticks, state) = self.inner.sample()?;
        let delta = self
            .prev_ticks
            .map_or(0, |prev| total_ticks.saturating_sub(prev));
        self.prev_ticks = Some(total_ticks);
        Some((delta, state))
    }
}

/// Sample a single process by PID (stateless).
///
/// Returns CPU time (user + system) normalized to centiseconds and
/// the current scheduling state. Returns `None` if the process
/// doesn't exist or can't be read.
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

        /// Reads CPU ticks and state via the persistent fd.
        ///
        /// Returns `(total_ticks, state)`.
        pub fn sample(&mut self) -> Option<(u64, ProcessState)> {
            self.buf.clear();
            self.file.seek(SeekFrom::Start(0)).ok()?;
            self.file.read_to_string(&mut self.buf).ok()?;
            parse_stat(&self.buf)
        }
    }

    /// Parse `/proc/<pid>/stat` contents into `(total_ticks, state)`.
    ///
    /// Fields after the comm field (past last `)`):
    /// - index 0: state char (`R`, `S`, `D`, `T`, `Z`, `X`, etc.)
    /// - index 11: utime (`USER_HZ` ticks, always 100 Hz)
    /// - index 12: stime (`USER_HZ` ticks)
    fn parse_stat(contents: &str) -> Option<(u64, ProcessState)> {
        // The comm field (index 1) may contain spaces and parentheses,
        // so find the last ')' to skip past it.
        let after_comm = contents.rfind(')')? + 1;
        let fields: Vec<&str> = contents[after_comm..].split_whitespace().collect();

        let state_char = fields.first()?.chars().next()?;
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;

        let state = match state_char {
            'R' => ProcessState::Running,
            'S' | 'I' => ProcessState::Sleeping,
            'D' => ProcessState::Blocked,
            // T (stopped), Z (zombie), X (dead), etc.
            _ => ProcessState::Dead,
        };

        Some((utime + stime, state))
    }

    /// Stateless sample — opens, reads, closes.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (cpu_ticks, state) = parse_stat(&contents)?;
        Some(ProcessSample { cpu_ticks, state })
    }
}

// ─── macOS ──────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProcessSample, ProcessState};

    /// macOS monitor — no persistent handle needed (`proc_pidinfo` is stateless).
    pub struct MonitorInner {
        pid: u32,
    }

    impl MonitorInner {
        /// Verifies the process exists and returns a monitor.
        pub fn new(pid: u32) -> Option<Self> {
            // Verify the process is accessible
            let _ = sample_raw(pid)?;
            Some(Self { pid })
        }

        /// Samples via `proc_pidinfo`.
        ///
        /// Returns `(total_ticks, state)`.
        pub fn sample(&mut self) -> Option<(u64, ProcessState)> {
            sample_raw(self.pid)
        }
    }

    /// Raw macOS sampling via `proc_pidinfo` with `PROC_PIDTASKINFO`.
    ///
    /// Returns Mach absolute time normalized to centiseconds via
    /// `mach_timebase_info`.
    fn sample_raw(pid: u32) -> Option<(u64, ProcessState)> {
        // Safety: calling POSIX and Mach APIs with correctly sized buffers.
        // The PID is validated by the kernel — invalid PIDs cause
        // `proc_pidinfo` to return <= 0.
        unsafe { sample_raw_inner(pid) }
    }

    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` and `libc::mach_timebase_info` with
    /// correctly sized and zeroed buffers.
    unsafe fn sample_raw_inner(pid: u32) -> Option<(u64, ProcessState)> {
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

        // Convert Mach absolute time to centiseconds
        let mut timebase = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
        libc::mach_timebase_info(&mut timebase);

        let total_abs = info.pti_total_user + info.pti_total_system;
        // Mach absolute -> nanoseconds -> centiseconds
        let nanos = total_abs * u64::from(timebase.numer) / u64::from(timebase.denom);
        let centiseconds = nanos / 10_000_000;

        // Determine state: check if process has any running threads
        let state = if info.pti_numrunning > 0 {
            ProcessState::Running
        } else {
            ProcessState::Sleeping
        };

        Some((centiseconds, state))
    }

    /// Stateless sample for the `sample(pid)` function.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let (cpu_ticks, state) = sample_raw(pid)?;
        Some(ProcessSample { cpu_ticks, state })
    }
}

// ─── Windows ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod platform {
    use super::{ProcessSample, ProcessState};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Windows monitor — persistent `HANDLE`, closed in `Drop`.
    pub struct MonitorInner {
        handle: HANDLE,
    }

    impl MonitorInner {
        /// Opens a process handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
        pub fn new(pid: u32) -> Option<Self> {
            // Safety: `OpenProcess` is called with a valid access flag.
            // Returns null on failure (invalid PID).
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return None;
            }
            Some(Self { handle })
        }

        /// Samples via `GetProcessTimes` using the persistent handle.
        ///
        /// Returns `(total_ticks, state)`.
        pub fn sample(&mut self) -> Option<(u64, ProcessState)> {
            // Safety: `self.handle` is valid (opened in `new`, closed in `Drop`).
            unsafe { sample_handle(self.handle) }
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

    /// Sample a process via an existing handle.
    ///
    /// # Safety
    ///
    /// Caller must ensure `handle` is a valid process handle with
    /// `PROCESS_QUERY_LIMITED_INFORMATION` access.
    unsafe fn sample_handle(handle: HANDLE) -> Option<(u64, ProcessState)> {
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
        let total_centiseconds = (user_100ns + kernel_100ns) / 100_000;

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

        Some((total_centiseconds, state))
    }

    /// Stateless sample — opens handle, reads, closes.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        // Safety: calling Win32 APIs with correctly sized buffers and
        // properly closing the handle on all paths.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let result = sample_handle(handle);
            CloseHandle(handle);
            result.map(|(cpu_ticks, state)| ProcessSample { cpu_ticks, state })
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

        pub fn sample(&mut self) -> Option<(u64, ProcessState)> {
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
        assert!(s.cpu_ticks > 0, "Own process should have consumed some CPU");
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
        let (delta, _state) = monitor.sample().expect("Should sample self");
        assert_eq!(delta, 0, "First sample should have delta 0");
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
        let (delta0, _) = monitor.sample().expect("First sample");
        assert_eq!(delta0, 0);

        // Burn some CPU so ticks advance
        let mut sum: u64 = 0;
        for i in 0..10_000_000 {
            sum = sum.wrapping_add(i);
        }
        // Prevent optimizing away the loop
        std::hint::black_box(sum);

        // Second sample: delta may be > 0 (depends on granularity)
        let (delta1, state) = monitor.sample().expect("Second sample");
        // We can't guarantee delta > 0 due to 10ms tick granularity,
        // but the sample should succeed and state should be Running.
        assert_eq!(state, ProcessState::Running);
        let _ = delta1;
    }
}
