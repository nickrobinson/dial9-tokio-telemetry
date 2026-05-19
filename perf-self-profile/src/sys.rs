#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux::fp_profiler;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux::write_symbol_data;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use linux::{
    PerfSampler, is_ctimer_active, resolve_symbol, resolve_symbol_with_maps,
    resolve_symbols_with_maps,
};

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) use unsupported::write_symbol_data;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub use unsupported::{PerfSampler, resolve_symbol};
