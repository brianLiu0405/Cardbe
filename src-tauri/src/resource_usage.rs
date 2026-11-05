//! CPU and memory usage of the Cardbe process tree.
//!
//! The tree is the app process plus every descendant whose parent link and
//! creation time prove it was started by a member, which covers the WebView2
//! browser, renderer, GPU and utility processes on Windows. Process names are
//! never used, so another app's WebView2 processes stay out. A child must not
//! be older than its parent: a process whose recorded parent PID was reused by
//! a newer Cardbe process predates that process and is rejected with its
//! descendants. CPU time is tracked per (PID, creation time), so a reused PID
//! never inherits another process's counter.
//!
//! CPU % is processor time used by the tree during the interval divided by
//! the interval times the number of logical processors, so 100% means every
//! logical processor was busy (the Task Manager "CPU" convention). Groups are
//! normalized the same way, so main + children equals the total. Memory sums
//! each member's private working set on Windows and resident set elsewhere;
//! resident sets include shared pages, so their sum can exceed the memory the
//! tree uses exclusively.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Calls closer together than this reuse the previous sample, so CPU deltas
/// always span a meaningful interval.
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStats {
    /// Creation time in a platform unit; only compared within one source.
    pub start_time: u64,
    /// Kernel and user processor time over the process lifetime.
    pub cpu_time: Duration,
    pub memory_bytes: u64,
}

// Each platform reports only some of these; names follow the OS metrics.
#[allow(dead_code, clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMetric {
    PrivateWorkingSet,
    WorkingSet,
    ResidentSet,
}

pub trait ProcessSource {
    fn logical_cpus(&self) -> u32;
    fn memory_metric(&self) -> MemoryMetric;
    /// Replaces `out` with every running process and its parent PID.
    fn processes(&mut self, out: &mut Vec<ProcessEntry>) -> Result<(), String>;
    /// `None` when the process exited or cannot be queried.
    fn read(&mut self, pid: u32) -> Option<ProcessStats>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct UsageGroup {
    /// `None` until a second sample provides an interval.
    pub cpu_percent: Option<f64>,
    pub memory_bytes: u64,
    pub process_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ResourceUsage {
    pub total: UsageGroup,
    pub main: UsageGroup,
    pub children: UsageGroup,
    pub logical_cpus: u32,
    pub memory_metric: MemoryMetric,
}

struct Member {
    pid: u32,
    stats: ProcessStats,
}

pub struct ResourceMonitor<S> {
    source: S,
    root_pid: u32,
    // Buffers reused across samples to avoid per-second allocations.
    entries: Vec<ProcessEntry>,
    members: Vec<Member>,
    previous_cpu: HashMap<(u32, u64), Duration>,
    next_cpu: HashMap<(u32, u64), Duration>,
    last: Option<(Instant, ResourceUsage)>,
}

impl<S: ProcessSource> ResourceMonitor<S> {
    pub fn new(source: S, root_pid: u32) -> Self {
        Self {
            source,
            root_pid,
            entries: Vec::new(),
            members: Vec::new(),
            previous_cpu: HashMap::new(),
            next_cpu: HashMap::new(),
            last: None,
        }
    }

    pub fn sample(&mut self, now: Instant) -> Result<ResourceUsage, String> {
        if let Some((at, usage)) = &self.last {
            if now.saturating_duration_since(*at) < MIN_SAMPLE_INTERVAL {
                return Ok(*usage);
            }
        }
        self.collect_members()?;

        let elapsed = self.last.map(|(at, _)| now.saturating_duration_since(at));
        let cpus = self.source.logical_cpus().max(1);
        let mut main_cpu = Duration::ZERO;
        let mut children_cpu = Duration::ZERO;
        let mut main = UsageGroup::default();
        let mut children = UsageGroup::default();
        self.next_cpu.clear();
        for (index, member) in self.members.iter().enumerate() {
            let identity = (member.pid, member.stats.start_time);
            // A process first seen in this sample contributes from the next one.
            let used = self
                .previous_cpu
                .get(&identity)
                .map_or(Duration::ZERO, |previous| {
                    member.stats.cpu_time.saturating_sub(*previous)
                });
            self.next_cpu.insert(identity, member.stats.cpu_time);
            let (group, cpu) = if index == 0 {
                (&mut main, &mut main_cpu)
            } else {
                (&mut children, &mut children_cpu)
            };
            *cpu += used;
            group.memory_bytes += member.stats.memory_bytes;
            group.process_count += 1;
        }
        std::mem::swap(&mut self.previous_cpu, &mut self.next_cpu);

        let percent = |used: Duration| elapsed.map(|elapsed| cpu_percent(used, elapsed, cpus));
        main.cpu_percent = percent(main_cpu);
        children.cpu_percent = percent(children_cpu);
        let usage = ResourceUsage {
            total: UsageGroup {
                cpu_percent: percent(main_cpu + children_cpu),
                memory_bytes: main.memory_bytes + children.memory_bytes,
                process_count: main.process_count + children.process_count,
            },
            main,
            children,
            logical_cpus: cpus,
            memory_metric: self.source.memory_metric(),
        };
        self.last = Some((now, usage));
        Ok(usage)
    }

    /// Walks the tree from the root, breadth first, into `members`.
    fn collect_members(&mut self) -> Result<(), String> {
        let Self {
            source,
            root_pid,
            entries,
            members,
            ..
        } = self;
        source.processes(entries)?;
        let root = source
            .read(*root_pid)
            .ok_or("The app process cannot be queried")?;
        members.clear();
        members.push(Member {
            pid: *root_pid,
            stats: root,
        });
        let mut index = 0;
        while index < members.len() {
            let parent_pid = members[index].pid;
            let parent_start = members[index].stats.start_time;
            for entry in entries.iter() {
                if entry.parent_pid != parent_pid
                    || entry.pid == parent_pid
                    || members.iter().any(|member| member.pid == entry.pid)
                {
                    continue;
                }
                if let Some(stats) = source.read(entry.pid) {
                    if stats.start_time >= parent_start {
                        members.push(Member {
                            pid: entry.pid,
                            stats,
                        });
                    }
                }
            }
            index += 1;
        }
        Ok(())
    }

    #[cfg(test)]
    fn member_pids(&self) -> Vec<u32> {
        self.members.iter().map(|member| member.pid).collect()
    }
}

/// Share of all logical processors used during `elapsed`, from 0 to 100.
pub fn cpu_percent(used: Duration, elapsed: Duration, logical_cpus: u32) -> f64 {
    let capacity = elapsed.as_secs_f64() * f64::from(logical_cpus.max(1));
    if capacity <= 0.0 {
        return 0.0;
    }
    (used.as_secs_f64() / capacity * 100.0).clamp(0.0, 100.0)
}

/// Created on first use, so an app that never shows usage never samples.
#[derive(Default)]
pub struct ResourceMonitorState {
    inner: Arc<Mutex<MonitorSlot>>,
}

#[derive(Default)]
struct MonitorSlot {
    monitor: Option<ResourceMonitor<platform::SystemProcesses>>,
    failing: bool,
}

impl ResourceMonitorState {
    /// Samples on the calling thread; run it on a blocking worker.
    pub fn sample(&self) -> Result<ResourceUsage, String> {
        let mut slot = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let monitor = slot.monitor.get_or_insert_with(|| {
            ResourceMonitor::new(platform::SystemProcesses::new(), std::process::id())
        });
        let result = monitor.sample(Instant::now());
        // Log a failure once per streak instead of every second.
        match &result {
            Ok(_) => slot.failing = false,
            Err(error) if !slot.failing => {
                slot.failing = true;
                log::warn!(target: "resource_usage", "Resource usage unavailable: {error}");
            }
            Err(_) => {}
        }
        result
    }

    pub fn handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{MemoryMetric, ProcessEntry, ProcessSource, ProcessStats};
    use std::mem::{size_of, zeroed};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows_sys::Win32::System::Threading::{
        GetActiveProcessorCount, GetProcessTimes, OpenProcess, ALL_PROCESSOR_GROUPS,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    pub struct SystemProcesses {
        cpus: u32,
        /// Private working set needs Windows 10 21H2 or later; older systems
        /// report the full working set instead.
        private_working_set: bool,
    }

    impl SystemProcesses {
        pub fn new() -> Self {
            let mut source = Self {
                cpus: unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1),
                private_working_set: true,
            };
            source.private_working_set = source.read(std::process::id()).is_some();
            source
        }

        fn memory(&self, handle: HANDLE) -> Option<u64> {
            let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = unsafe { zeroed() };
            let size = if self.private_working_set {
                size_of::<PROCESS_MEMORY_COUNTERS_EX2>()
            } else {
                size_of::<PROCESS_MEMORY_COUNTERS>()
            } as u32;
            counters.cb = size;
            let read = unsafe {
                GetProcessMemoryInfo(
                    handle,
                    (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                    size,
                )
            };
            if read == 0 {
                return None;
            }
            Some(if self.private_working_set {
                counters.PrivateWorkingSetSize as u64
            } else {
                counters.WorkingSetSize as u64
            })
        }
    }

    impl ProcessSource for SystemProcesses {
        fn logical_cpus(&self) -> u32 {
            self.cpus
        }

        fn memory_metric(&self) -> MemoryMetric {
            if self.private_working_set {
                MemoryMetric::PrivateWorkingSet
            } else {
                MemoryMetric::WorkingSet
            }
        }

        fn processes(&mut self, out: &mut Vec<ProcessEntry>) -> Result<(), String> {
            out.clear();
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let snapshot = OwnedHandle(snapshot);
            let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
            while more {
                out.push(ProcessEntry {
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                });
                more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
            }
            Ok(())
        }

        fn read(&mut self, pid: u32) -> Option<ProcessStats> {
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return None;
            }
            let handle = OwnedHandle(handle);
            let mut creation: FILETIME = unsafe { zeroed() };
            let mut exit: FILETIME = unsafe { zeroed() };
            let mut kernel: FILETIME = unsafe { zeroed() };
            let mut user: FILETIME = unsafe { zeroed() };
            let read = unsafe {
                GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user)
            };
            if read == 0 {
                return None;
            }
            Some(ProcessStats {
                start_time: ticks(creation),
                // FILETIME durations count 100-nanosecond intervals.
                cpu_time: Duration::from_nanos((ticks(kernel) + ticks(user)).saturating_mul(100)),
                memory_bytes: self.memory(handle.0)?,
            })
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{MemoryMetric, ProcessEntry, ProcessSource, ProcessStats};
    use std::time::Duration;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    pub struct SystemProcesses {
        system: System,
        cpus: u32,
    }

    impl SystemProcesses {
        pub fn new() -> Self {
            Self {
                system: System::new(),
                cpus: std::thread::available_parallelism()
                    .map_or(1, |count| u32::try_from(count.get()).unwrap_or(u32::MAX)),
            }
        }
    }

    impl ProcessSource for SystemProcesses {
        fn logical_cpus(&self) -> u32 {
            self.cpus
        }

        fn memory_metric(&self) -> MemoryMetric {
            MemoryMetric::ResidentSet
        }

        fn processes(&mut self, out: &mut Vec<ProcessEntry>) -> Result<(), String> {
            // Parent links and start times only; usage is refreshed per member.
            // Linux tasks are threads sharing their process's memory and CPU
            // accounting, so including them would count usage more than once.
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().without_tasks(),
            );
            out.clear();
            out.extend(
                self.system
                    .processes()
                    .iter()
                    .map(|(pid, process)| ProcessEntry {
                        pid: pid.as_u32(),
                        parent_pid: process.parent().map_or(0, Pid::as_u32),
                    }),
            );
            if out.is_empty() {
                return Err("The process list is unavailable".into());
            }
            Ok(())
        }

        fn read(&mut self, pid: u32) -> Option<ProcessStats> {
            let pid = Pid::from_u32(pid);
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing()
                    .without_tasks()
                    .with_cpu()
                    .with_memory(),
            );
            let process = self.system.process(pid)?;
            Some(ProcessStats {
                start_time: process.start_time(),
                cpu_time: Duration::from_millis(process.accumulated_cpu_time()),
                memory_bytes: process.memory(),
            })
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;
        use std::sync::mpsc;

        #[test]
        fn excludes_native_threads_from_process_snapshots() {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (stop_tx, stop_rx) = mpsc::channel::<()>();
            let worker = std::thread::spawn(move || {
                let thread_path = std::fs::read_link("/proc/thread-self").unwrap();
                let tid: u32 = thread_path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                ready_tx.send(tid).unwrap();
                // Dropping the sender also releases the worker if the test fails.
                let _ = stop_rx.recv();
            });
            let tid = ready_rx.recv().unwrap();
            let mut source = SystemProcesses::new();
            let mut entries = Vec::new();
            let listed = source.processes(&mut entries);
            let listed_thread = entries.iter().any(|entry| entry.pid == tid);
            let root_stats = source.read(std::process::id());
            // The per-member refresh must not reintroduce threads after listing.
            let refreshed_thread = source.system.process(Pid::from_u32(tid)).is_some();
            drop(stop_tx);
            worker.join().unwrap();

            listed.unwrap();
            assert_ne!(tid, std::process::id());
            assert!(root_stats.unwrap().memory_bytes > 0);
            assert!(
                !listed_thread,
                "native thread {tid} was listed as a process"
            );
            assert!(
                !refreshed_thread,
                "refreshing process usage added native thread {tid}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[derive(Default)]
    struct FakeSource {
        cpus: u32,
        processes: HashMap<u32, (u32, ProcessStats)>,
        unreadable: Vec<u32>,
        list_fails: bool,
    }

    impl FakeSource {
        fn new(cpus: u32) -> Self {
            Self {
                cpus,
                ..Self::default()
            }
        }

        fn spawn(&mut self, pid: u32, parent_pid: u32, start_time: u64, memory_mib: u64) {
            self.processes.insert(
                pid,
                (
                    parent_pid,
                    ProcessStats {
                        start_time,
                        cpu_time: Duration::ZERO,
                        memory_bytes: memory_mib << 20,
                    },
                ),
            );
        }

        fn run(&mut self, pid: u32, cpu: Duration) {
            self.processes.get_mut(&pid).unwrap().1.cpu_time += cpu;
        }
    }

    impl ProcessSource for FakeSource {
        fn logical_cpus(&self) -> u32 {
            self.cpus
        }

        fn memory_metric(&self) -> MemoryMetric {
            MemoryMetric::PrivateWorkingSet
        }

        fn processes(&mut self, out: &mut Vec<ProcessEntry>) -> Result<(), String> {
            if self.list_fails {
                return Err("snapshot failed".into());
            }
            out.clear();
            out.extend(
                self.processes
                    .iter()
                    .map(|(&pid, &(parent_pid, _))| ProcessEntry { pid, parent_pid }),
            );
            Ok(())
        }

        fn read(&mut self, pid: u32) -> Option<ProcessStats> {
            if self.unreadable.contains(&pid) {
                return None;
            }
            self.processes.get(&pid).map(|(_, stats)| *stats)
        }
    }

    fn sorted(mut pids: Vec<u32>) -> Vec<u32> {
        pids.sort_unstable();
        pids
    }

    #[test]
    fn reports_main_process_usage_after_an_interval() {
        let mut source = FakeSource::new(4);
        source.spawn(10, 1, 100, 64);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();

        let first = monitor.sample(start).unwrap();
        assert_eq!(first.total.cpu_percent, None);
        assert_eq!(first.total.memory_bytes, 64 << 20);
        assert_eq!(first.main.process_count, 1);
        assert_eq!(first.children.process_count, 0);
        assert_eq!(first.logical_cpus, 4);

        monitor.source.run(10, Duration::from_millis(500));
        let second = monitor.sample(start + SECOND).unwrap();
        assert_eq!(second.main.cpu_percent, Some(12.5));
        assert_eq!(second.total.cpu_percent, Some(12.5));
        assert_eq!(second.children.cpu_percent, Some(0.0));
    }

    #[test]
    fn sums_descendants_into_children_and_total() {
        let mut source = FakeSource::new(2);
        source.spawn(10, 1, 100, 100);
        // WebView2 browser, its renderer and GPU processes.
        source.spawn(20, 10, 110, 50);
        source.spawn(21, 20, 120, 30);
        source.spawn(22, 20, 121, 20);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();
        monitor.sample(start).unwrap();

        monitor.source.run(10, Duration::from_millis(200));
        monitor.source.run(21, Duration::from_millis(300));
        monitor.source.run(22, Duration::from_millis(100));
        let usage = monitor.sample(start + SECOND).unwrap();
        assert_eq!(sorted(monitor.member_pids()), vec![10, 20, 21, 22]);
        assert_eq!(usage.main.cpu_percent, Some(10.0));
        assert_eq!(usage.children.cpu_percent, Some(20.0));
        assert_eq!(usage.total.cpu_percent, Some(30.0));
        assert_eq!(usage.main.memory_bytes, 100 << 20);
        assert_eq!(usage.children.memory_bytes, 100 << 20);
        assert_eq!(usage.total.memory_bytes, 200 << 20);
        assert_eq!(usage.children.process_count, 3);
        assert_eq!(usage.total.process_count, 4);
    }

    #[test]
    fn follows_children_that_start_and_exit() {
        let mut source = FakeSource::new(1);
        source.spawn(10, 1, 100, 10);
        source.spawn(20, 10, 110, 10);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();
        monitor.sample(start).unwrap();

        // A renderer is recreated: the old one exits and a new one starts.
        monitor.source.processes.remove(&20);
        monitor.source.spawn(30, 10, 200, 25);
        let usage = monitor.sample(start + SECOND).unwrap();
        assert_eq!(sorted(monitor.member_pids()), vec![10, 30]);
        assert_eq!(usage.children.memory_bytes, 25 << 20);

        // A child whose parent already exited is no longer provably Cardbe's.
        monitor.source.spawn(40, 30, 210, 5);
        monitor.source.processes.remove(&30);
        let usage = monitor.sample(start + 2 * SECOND).unwrap();
        assert_eq!(monitor.member_pids(), vec![10]);
        assert_eq!(
            usage.children,
            UsageGroup {
                cpu_percent: Some(0.0),
                memory_bytes: 0,
                process_count: 0,
            }
        );
    }

    #[test]
    fn excludes_unrelated_processes_and_reused_parent_pids() {
        let mut source = FakeSource::new(1);
        source.spawn(10, 1, 100, 10);
        source.spawn(20, 10, 110, 10);
        // Another app's WebView2 tree.
        source.spawn(50, 1, 90, 500);
        source.spawn(51, 50, 95, 500);
        // Started by an earlier process whose PID 20 now belongs to Cardbe.
        source.spawn(60, 20, 105, 700);
        source.spawn(61, 60, 106, 700);
        let mut monitor = ResourceMonitor::new(source, 10);

        let usage = monitor.sample(Instant::now()).unwrap();
        assert_eq!(sorted(monitor.member_pids()), vec![10, 20]);
        assert_eq!(usage.total.memory_bytes, 20 << 20);
    }

    #[test]
    fn a_reused_pid_does_not_inherit_cpu_time() {
        let mut source = FakeSource::new(1);
        source.spawn(10, 1, 100, 10);
        source.spawn(20, 10, 110, 10);
        source.run(20, 30 * SECOND);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();
        monitor.sample(start).unwrap();

        monitor.source.spawn(20, 10, 300, 10);
        monitor.source.run(20, Duration::from_millis(100));
        let usage = monitor.sample(start + SECOND).unwrap();
        assert_eq!(usage.children.cpu_percent, Some(0.0));

        monitor.source.run(20, Duration::from_millis(250));
        let usage = monitor.sample(start + 2 * SECOND).unwrap();
        assert_eq!(usage.children.cpu_percent, Some(25.0));
    }

    #[test]
    fn normalizes_cpu_by_logical_processors() {
        assert_eq!(cpu_percent(SECOND, SECOND, 1), 100.0);
        assert_eq!(cpu_percent(SECOND, SECOND, 8), 12.5);
        assert_eq!(cpu_percent(6 * SECOND, 2 * SECOND, 4), 75.0);
        // Rounding in platform counters never reports more than every core.
        assert_eq!(cpu_percent(5 * SECOND, SECOND, 4), 100.0);
        assert_eq!(cpu_percent(SECOND, Duration::ZERO, 4), 0.0);
        assert_eq!(cpu_percent(SECOND, SECOND, 0), 100.0);
    }

    #[test]
    fn rapid_calls_reuse_the_previous_sample() {
        let mut source = FakeSource::new(1);
        source.spawn(10, 1, 100, 10);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();
        monitor.sample(start).unwrap();
        monitor.source.run(10, Duration::from_millis(500));
        let usage = monitor.sample(start + SECOND).unwrap();
        assert_eq!(usage.total.cpu_percent, Some(50.0));

        monitor.source.run(10, Duration::from_millis(100));
        let cached = monitor
            .sample(start + SECOND + Duration::from_millis(10))
            .unwrap();
        assert_eq!(cached, usage);
        let next = monitor.sample(start + 2 * SECOND).unwrap();
        assert_eq!(next.total.cpu_percent, Some(10.0));
    }

    #[test]
    fn reports_failures_without_losing_the_baseline() {
        let mut source = FakeSource::new(1);
        source.spawn(10, 1, 100, 10);
        source.spawn(20, 10, 110, 10);
        source.unreadable.push(20);
        let mut monitor = ResourceMonitor::new(source, 10);
        let start = Instant::now();
        // A child that cannot be queried is skipped.
        let usage = monitor.sample(start).unwrap();
        assert_eq!(usage.total.process_count, 1);

        monitor.source.list_fails = true;
        assert!(monitor.sample(start + SECOND).is_err());
        monitor.source.list_fails = false;
        monitor.source.unreadable.push(10);
        assert!(monitor.sample(start + 2 * SECOND).is_err());

        monitor.source.unreadable.clear();
        monitor.source.run(10, 3 * SECOND);
        let usage = monitor.sample(start + 4 * SECOND).unwrap();
        assert_eq!(usage.main.cpu_percent, Some(75.0));
        assert_eq!(usage.total.process_count, 2);
    }

    /// Child process body for `samples_the_real_process_tree`.
    #[test]
    fn resource_usage_child_helper() {
        if std::env::var_os("CARDBE_RESOURCE_USAGE_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn samples_the_real_process_tree() {
        let mut monitor =
            ResourceMonitor::new(platform::SystemProcesses::new(), std::process::id());
        let start = Instant::now();
        let first = monitor.sample(start).unwrap();
        assert!(first.main.memory_bytes > 0);
        assert!(first.logical_cpus >= 1);

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "resource_usage::tests::resource_usage_child_helper",
                "--exact",
                "--test-threads=1",
            ])
            .env("CARDBE_RESOURCE_USAGE_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let usage = monitor.sample(start + SECOND).unwrap();
        let members = monitor.member_pids();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(members.contains(&child.id()), "{members:?}");
        assert!(usage.children.memory_bytes > 0);
        let cpu = usage.total.cpu_percent.unwrap();
        assert!((0.0..=100.0).contains(&cpu));

        monitor.sample(start + 2 * SECOND).unwrap();
        assert!(!monitor.member_pids().contains(&child.id()));
    }
}
