use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const REALTIME_SPIN_WINDOW: Duration = Duration::from_micros(250);
pub const FALLBACK_SPIN_WINDOW: Duration = Duration::from_millis(1);
pub const BARRIER_LEAD: Duration = Duration::from_micros(100);
const MAX_SLEEP_SLICE: Duration = Duration::from_millis(50);

pub fn init_timer_resolution() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
}

pub fn cleanup_timer_resolution() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Media::timeEndPeriod(1);
    }
}

pub fn allowed_cpus() -> Vec<usize> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(set) = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)) {
            let mut cpus = Vec::new();
            for cpu in 0..nix::sched::CpuSet::count() {
                if set.is_set(cpu).unwrap_or(false) {
                    cpus.push(cpu);
                }
            }
            return cpus;
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        if let Ok(count) = std::thread::available_parallelism() {
            return (0..count.get()).collect();
        }
    }

    Vec::new()
}

#[cfg(target_os = "linux")]
pub fn pin_current_thread(cpu: usize) -> bool {
    let mut set = nix::sched::CpuSet::new();
    if set.set(cpu).is_err() {
        return false;
    }
    nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set).is_ok()
}

#[cfg(windows)]
pub fn pin_current_thread(cpu: usize) -> bool {
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        SetThreadAffinityMask(GetCurrentThread(), 1usize << cpu) != 0
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn pin_current_thread(cpu: usize) -> bool {
    let _ = cpu;
    false
}

#[cfg(target_os = "linux")]
pub fn elevate_current_thread() -> bool {
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 80;
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) == 0
    }
}

#[cfg(windows)]
pub fn elevate_current_thread() -> bool {
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) != 0
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn elevate_current_thread() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn monotonic_ns() -> u64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut spec);
    }
    spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub fn monotonic_ns() -> u64 {
    0
}

pub fn fq_status() -> (Option<bool>, String) {
    use std::process::Command;

    let route = match Command::new("ip")
        .args(["-o", "route", "get", "1.1.1.1"])
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).to_string(),
        _ => return (None, "[SYNC] cannot verify fq: `ip` command unavailable".to_string()),
    };
    let iface = match route
        .split_whitespace()
        .skip_while(|word| *word != "dev")
        .nth(1)
    {
        Some(iface) => iface.to_string(),
        None => return (None, "[SYNC] cannot verify fq: no default interface found".to_string()),
    };
    let text = match Command::new("tc").args(["qdisc", "show", "dev", &iface]).output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).to_string(),
        _ => return (None, format!("[SYNC] cannot verify fq on {iface}: `tc` command unavailable")),
    };
    if text.split_whitespace().any(|word| word == "fq") {
        (Some(true), format!("[SYNC] fq qdisc active on {iface}"))
    } else {
        let found = text.lines().next().unwrap_or("none").trim().to_string();
        (
            Some(false),
            format!(
                "[SYNC] WARNING: fq qdisc NOT active on {iface} (found: {found}); txtime disabled. Run: sudo tc qdisc replace dev {iface} root fq"
            ),
        )
    }
}

pub fn lock_memory() -> bool {
    #[cfg(target_os = "linux")]
    {
        return nix::sys::mman::mlockall(nix::sys::mman::MlockAllFlags::MCL_CURRENT).is_ok();
    }

    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

pub fn wait_until(deadline: Instant, spin: Duration, stopping: &AtomicBool) -> Instant {
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Instant::now();
        }
        let now = Instant::now();
        if now >= deadline {
            return now;
        }
        let remaining = deadline - now;
        if remaining > spin {
            let sleep = remaining.saturating_sub(spin).min(MAX_SLEEP_SLICE);
            std::thread::sleep(sleep);
        } else {
            std::hint::spin_loop();
        }
    }
}

pub struct SpinBarrier {
    parties: usize,
    arrived: AtomicUsize,
    generation: AtomicUsize,
    aborted: AtomicBool,
}

impl SpinBarrier {
    pub fn new(parties: usize) -> Self {
        Self {
            parties,
            arrived: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
            aborted: AtomicBool::new(false),
        }
    }

    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
    }

    pub fn wait(&self, stopping: &AtomicBool) -> bool {
        if self.aborted.load(Ordering::Acquire) {
            return false;
        }
        let generation = self.generation.load(Ordering::Acquire);
        if self.arrived.fetch_add(1, Ordering::AcqRel) + 1 == self.parties {
            self.arrived.store(0, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::Release);
            return true;
        }
        while self.generation.load(Ordering::Acquire) == generation {
            if self.aborted.load(Ordering::Acquire) || stopping.load(Ordering::Relaxed) {
                return false;
            }
            std::hint::spin_loop();
        }
        true
    }
}
