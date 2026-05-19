//! ctimer-based CPU sampler, provides a fallback for CPU profiling
//! when kernel restrictions make perf inaccessible.
//!
//! Uses per-thread CPU timers to deliver SIGPROF,
//! then unwinds frame pointers via safe_load.

use core::{mem, ptr};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use super::fp_profiler::{
    self, ctimer, sample_buffer,
    unwind::{self, MAX_FRAMES},
};
use super::gettid;

#[cfg(target_os = "linux")]
use libc::{timer_getoverrun, timer_settime};

// Bionic exposes these but the Rust `libc` crate doesn't bind them yet.
#[cfg(target_os = "android")]
unsafe extern "C" {
    fn timer_getoverrun(timerid: libc::timer_t) -> libc::c_int;
    fn timer_settime(
        timerid: libc::timer_t,
        flags: libc::c_int,
        new_value: *const libc::itimerspec,
        old_value: *mut libc::itimerspec,
    ) -> libc::c_int;
}

use super::sampler::SamplerBackend;

use crate::sampler::{Sample, SamplerConfig};

static CTIMER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_ctimer_active() -> bool {
    CTIMER_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Debug)]
pub(crate) struct CtimerSampler {}

impl CtimerSampler {
    /// Install signal handlers and start the ctimer engine. Does NOT register
    /// any threads. Callers must invoke `track_current_thread` per thread to
    /// begin receiving samples.
    pub fn start(config: &SamplerConfig) -> io::Result<Self> {
        use crate::SamplingMode;

        let freq = match config.sampling {
            SamplingMode::FrequencyHz(hz) => hz.max(1),
            SamplingMode::Period(p) => {
                // Period-based sampling counts kernel events (context switches,
                // tracepoints) which have no userspace equivalent. ctimer can
                // only do frequency-based CPU-time sampling.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "ctimer fallback does not support period-based sampling \
                         (SamplingMode::Period({p})): event-based sources like \
                         context switches require perf to be accessible"
                    ),
                ));
            }
        };
        let interval_ns = 1_000_000_000i64 / (freq as i64);

        unsafe {
            fp_profiler::install_handler().map_err(|e| {
                io::Error::other(format!("failed to install safe_load SIGSEGV handler: {e}"))
            })?;
        }

        unsafe {
            ctimer::start(interval_ns, sigprof_handler)
                .map_err(|e| io::Error::other(format!("failed to start ctimer: {e}")))?;
        }

        CTIMER_ACTIVE.store(true, Ordering::Release);

        Ok(Self {})
    }
}

impl SamplerBackend for CtimerSampler {
    fn track_current_thread(&mut self) -> io::Result<()> {
        ctimer::register_thread()
            .map_err(|e| io::Error::other(format!("ctimer::register_thread failed: {e}")))
    }

    fn stop_tracking_current_thread(&mut self) {
        ctimer::unregister_thread();
    }

    fn has_pending(&self) -> bool {
        sample_buffer::has_pending()
    }

    fn for_each_sample(&mut self, f: &mut dyn FnMut(&Sample)) {
        let dropped = sample_buffer::take_dropped_count();
        if dropped > 0 {
            tracing::debug!("[ctimer] dropped {dropped} samples (buffer full)");
        }

        let mut sample = Sample {
            ip: 0,
            pid: 0,
            tid: 0,
            time: 0,
            cpu: None,
            period: 0,
            callchain: Vec::with_capacity(MAX_FRAMES),
            raw: None,
        };

        sample_buffer::drain(|raw| {
            let n = (raw.num_frames as usize).min(MAX_FRAMES);
            sample.callchain.clear();
            sample.callchain.extend_from_slice(&raw.frames[..n]);
            sample.ip = sample.callchain.first().copied().unwrap_or(0);
            sample.pid = raw.pid;
            sample.tid = raw.tid;
            sample.time = raw.time;
            sample.cpu = raw.cpu;
            sample.period = raw.period;
            f(&sample);
        });
    }

    fn drain_samples(&mut self) -> Vec<Sample> {
        let mut samples = Vec::new();
        self.for_each_sample(&mut |s| samples.push(s.clone()));
        samples
    }

    fn disable(&self) {
        ctimer::disable();
    }

    fn enable(&self) {
        ctimer::enable();
    }
}

impl Drop for CtimerSampler {
    fn drop(&mut self) {
        ctimer::disarm_all_timers();
        CTIMER_ACTIVE.store(false, Ordering::Release);
    }
}

// Fired by per-thread CPU timers, must be async-signal-safe.
extern "C" fn sigprof_handler(
    _signo: libc::c_int,
    _info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    if !ctimer::is_running() {
        if ctimer::is_disarm_requested()
            && let Some(t) = ctimer::current_thread_timer_id()
        {
            // Self-disarm so the timer doesn't keep firing for the rest of the
            // thread's lifetime. timer_settime is async-signal-safe.
            // SAFETY: `t` is this thread's own live timer_t, zero itimerspec
            // is valid and disarms without deleting.
            unsafe {
                let zero: libc::itimerspec = mem::zeroed();
                timer_settime(t, 0, &zero, ptr::null_mut());
            }
        }
        return;
    }

    // SAFETY: all operations are async-signal-safe.
    unsafe {
        let Some(mut slot) = sample_buffer::claim_slot() else {
            return; // buffer full, sample dropped (counted internally)
        };

        let pid = libc::getpid() as u32;
        let tid = gettid() as u32;

        let mut cpu_raw = 0u32;
        // SAFETY: `getcpu` writes one `u32` through `&mut cpu_raw`, node/cache pointers are null (allowed).
        let cpu = if libc::syscall(
            libc::SYS_getcpu,
            &mut cpu_raw,
            ptr::null_mut::<libc::c_void>(),
            ptr::null_mut::<libc::c_void>(),
        ) == 0
        {
            Some(cpu_raw)
        } else {
            None
        };

        let mut ts: libc::timespec = mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        let time = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;

        // Account for the number of missed timer expirations.
        let interval_ns = ctimer::interval_ns() as u64;
        let overruns = ctimer::current_thread_timer_id()
            .map(|t| timer_getoverrun(t).max(0) as u64)
            .unwrap_or(0);
        let period = interval_ns.saturating_mul(1 + overruns);

        slot.write(pid, tid, time, cpu, period);

        // Unwind into the slot's frame buffer.
        //
        // On Android, the safe_load SIGSEGV handler (which makes
        // frame-pointer unwinding fault-tolerant) doesn't work
        // because Android's libsigchain intercepts SIGSEGV before
        // the app's handler. Record only the interrupted PC to
        // avoid crashing; stacks are single-frame but still useful
        // for identifying hot functions.
        //
        // We also can't use `libc::ucontext_t` on Android because the
        // libc crate's struct is missing the 120-byte `__padding` between
        // `uc_sigmask` and `uc_mcontext` (Bionic pads sigmask to 128 bytes,
        // but the libc crate's sigset_t is only 8 bytes). Read the PC
        // directly at the correct offset.
        #[cfg(target_os = "android")]
        {
            let pc = android_ucontext_pc(ucontext);
            let frames = slot.frames_mut();
            if !frames.is_empty() {
                frames[0] = pc;
            }
            slot.set_num_frames(1);
        }
        #[cfg(not(target_os = "android"))]
        {
            let result = unwind::unwind_from_ucontext(ucontext, slot.frames_mut());
            slot.set_num_frames(result.frames_written as u32);
        }

        slot.commit();
    }
}

/// Read the PC register from a ucontext on Android aarch64.
///
/// The `libc` crate's `ucontext_t` for Android is missing the 120-byte
/// `__padding` field that Bionic inserts between `uc_sigmask` (8 bytes)
/// and `uc_mcontext` to pad the sigmask area to 128 bytes. This makes
/// `(*uc).uc_mcontext.pc` read from the wrong offset.
///
/// Bionic aarch64 layout:
///   uc_flags:      8 bytes (offset 0)
///   uc_link:       8 bytes (offset 8)
///   uc_stack:      24 bytes (offset 16)  [ss_sp(8) + ss_flags(4) + pad(4) + ss_size(8)]
///   uc_sigmask:    8 bytes (offset 40)
///   __padding:     120 bytes (offset 48)  [128 - sizeof(sigset_t)]
///   uc_mcontext:   (offset 168)
///     fault_address: 8 bytes
///     regs[31]:      248 bytes
///     sp:            8 bytes
///     pc:            8 bytes  (offset 168 + 8 + 248 + 8 = 432)
#[cfg(all(target_os = "android", target_arch = "aarch64"))]
unsafe fn android_ucontext_pc(ucontext: *mut libc::c_void) -> u64 {
    const PC_OFFSET: usize = 432; // 168 (mcontext) + 264 (fault_addr + regs[31] + sp)
    let base = ucontext as *const u8;
    unsafe { core::ptr::read_unaligned(base.add(PC_OFFSET) as *const u64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SamplingMode;
    use crate::sampler::EventSource;

    #[test]
    fn start_rejects_period_mode() {
        let config = SamplerConfig {
            sampling: SamplingMode::Period(1),
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
            max_tracked_threads: 256,
        };
        let err = CtimerSampler::start(&config).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
