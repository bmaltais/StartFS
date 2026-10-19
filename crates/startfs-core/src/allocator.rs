//! Giving freed heap memory back to the operating system.
//!
//! glibc keeps what a burst freed in its per-thread arenas: a join once left a daemon at 840 MiB
//! resident with 24 MiB in use (#78, #91). `malloc_trim` hands those pages back.

/// Return freed memory to the OS. Does nothing where the allocator is not glibc's.
pub fn release_freed() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim only releases free pages and may be called from any thread.
        unsafe {
            libc::malloc_trim(0);
        }
    }
}
