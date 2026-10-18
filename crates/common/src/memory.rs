//! Event-driven allocator maintenance for the memory-constrained device.

/// Set before starting daemon helper threads. Extra glibc arenas retain freed rasters.
pub fn configure_daemon_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe { libc::mallopt(libc::M_ARENA_MAX, 1); }
}

/// Called only after releasing a batch of large buffers, never from an idle timer.
pub fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe { libc::malloc_trim(0); }
}
