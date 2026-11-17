//! How much physical memory the machine has, and how much of it is free.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Installed physical memory, in bytes.
    pub total: u64,
    /// Physical memory that can be used without swapping, in bytes.
    pub available: u64,
}

/// `None` where there is no cheap way to ask (anything but Windows and Linux); loading
/// then only reacts to allocations that actually fail.
#[cfg(windows)]
pub fn status() -> Option<Status> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    // SAFETY: the struct is plain integers, so all-zero is a valid value.
    let mut info: MemoryStatusEx = unsafe { std::mem::zeroed() };
    info.length = size_of::<MemoryStatusEx>() as u32;
    // SAFETY: `info` is a valid MEMORYSTATUSEX with its length field set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut info) } != 0;
    ok.then_some(Status {
        total: info.total_phys,
        available: info.avail_phys,
    })
}

#[cfg(target_os = "linux")]
pub fn status() -> Option<Status> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn status() -> Option<Status> {
    None
}

/// Reads `MemTotal` and `MemAvailable` (both in kB) out of `/proc/meminfo`.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> Option<Status> {
    let field = |name: &str| {
        let line = text.lines().find_map(|l| l.strip_prefix(name))?;
        let kb: u64 = line.split_whitespace().next()?.parse().ok()?;
        kb.checked_mul(1024)
    };
    Some(Status {
        total: field("MemTotal:")?,
        available: field("MemAvailable:")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo() {
        let text = "MemTotal:       16314280 kB\nMemFree:          250000 kB\n\
                    MemAvailable:    8000000 kB\nBuffers:          100000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(Status {
                total: 16314280 * 1024,
                available: 8000000 * 1024
            })
        );
        assert_eq!(parse_meminfo("MemTotal: 5 kB\n"), None);
    }

    #[test]
    fn real_machine_is_plausible() {
        if let Some(status) = status() {
            assert!(status.total > 0);
            assert!(status.available <= status.total);
        }
    }
}
