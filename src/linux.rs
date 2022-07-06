use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_void};
use core::fmt;
use core::panic::PanicInfo;
use core::sync::atomic::{self, Ordering};

use log::{error, Level, Metadata, Record};
use log::{LevelFilter, SetLoggerError};

extern "C" {
    /// Linux provided alloc function
    fn nvalloc_linux_alloc(size: usize, align: usize) -> *mut u8;
    /// Linux provided free function
    fn nvalloc_linux_free(ptr: *mut u8, size: usize, align: usize);
    /// Linux provided printk function
    fn nvalloc_printk(format: *const u8, module_name: *const u8, args: *const c_void);
}

#[no_mangle]
extern "C" fn nvalloc_init_logging() -> i64 {
    match init() {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

struct LinuxAlloc;
unsafe impl GlobalAlloc for LinuxAlloc {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        nvalloc_linux_alloc(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        nvalloc_linux_free(ptr, layout.size(), layout.align());
    }
}

#[global_allocator]
static LINUX_ALLOC: LinuxAlloc = LinuxAlloc;

#[alloc_error_handler]
fn on_oom(layout: Layout) -> ! {
    error!("Unable to allocate {} bytes", layout.size());
    loop {
        atomic::compiler_fence(Ordering::SeqCst);
    }
}

#[inline(never)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("{info}");
    loop {
        atomic::compiler_fence(Ordering::SeqCst);
    }
}

/// Printing facilities.
///
/// C header: [`include/linux/printk.h`](../../../../include/linux/printk.h)
///
/// Reference: <https://www.kernel.org/doc/html/latest/core-api/printk-basics.html>
struct KPrintLogger;

impl log::Log for KPrintLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        cfg_if::cfg_if! {
            if #[cfg(feature = "max_level_debug")] {
                metadata.level() <= Level::Debug
            } else if #[cfg(feature = "max_level_info")] {
                metadata.level() <= Level::Info
            } else if #[cfg(feature = "max_level_error")] {
                metadata.level() <= Level::Error
            } else {
                metadata.level() <= Level::Warn
            }
        }
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            match record.metadata().level() {
                Level::Error => unsafe { call_printk(&format_strings::ERR, record.args()) },
                Level::Warn => unsafe { call_printk(&format_strings::WARNING, record.args()) },
                Level::Info => unsafe { call_printk(&format_strings::INFO, record.args()) },
                Level::Debug => unsafe { call_printk(&format_strings::DEBUG, record.args()) },
                Level::Trace => unsafe { call_printk(&format_strings::DEBUG, record.args()) },
            }
        }
    }

    fn flush(&self) {}
}

static LOGGER: KPrintLogger = KPrintLogger;

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(LevelFilter::Info))
}

// Linux bindings
mod bindings {
    pub const KERN_EMERG: &[u8; 3] = b"\x010\0";
    pub const KERN_ALERT: &[u8; 3] = b"\x011\0";
    pub const KERN_CRIT: &[u8; 3] = b"\x012\0";
    pub const KERN_ERR: &[u8; 3] = b"\x013\0";
    pub const KERN_WARNING: &[u8; 3] = b"\x014\0";
    pub const KERN_NOTICE: &[u8; 3] = b"\x015\0";
    pub const KERN_INFO: &[u8; 3] = b"\x016\0";
    pub const KERN_DEBUG: &[u8; 3] = b"\x017\0";
}

// Called from `vsprintf` with format specifier `%pA`.
#[no_mangle]
extern "C" fn rust_fmt_argument(
    buf: *mut c_char,
    end: *mut c_char,
    ptr: *const c_void,
) -> *mut c_char {
    use fmt::Write;
    // SAFETY: The C contract guarantees that `buf` is valid if it's less than `end`.
    let mut w = unsafe { RawFormatter::from_ptrs(buf.cast(), end.cast()) };
    let _ = w.write_fmt(unsafe { *(ptr as *const fmt::Arguments<'_>) });
    w.pos().cast()
}

/// Format strings.
///
/// Public but hidden since it should only be used from public macros.
#[doc(hidden)]
pub mod format_strings {
    use super::bindings;

    /// The length we copy from the `KERN_*` kernel prefixes.
    const LENGTH_PREFIX: usize = 2;

    /// The length of the fixed format strings.
    pub const LENGTH: usize = 10;

    /// Generates a fixed format string for the kernel's [`_printk`].
    ///
    /// The format string is always the same for a given level, i.e. for a
    /// given `prefix`, which are the kernel's `KERN_*` constants.
    ///
    /// [`_printk`]: ../../../../include/linux/printk.h
    const fn generate(is_cont: bool, prefix: &[u8; 3]) -> [u8; LENGTH] {
        // Ensure the `KERN_*` macros are what we expect.
        assert!(prefix[0] == b'\x01');
        if is_cont {
            assert!(prefix[1] == b'c');
        } else {
            assert!(prefix[1] >= b'0' && prefix[1] <= b'7');
        }
        assert!(prefix[2] == b'\x00');

        let suffix: &[u8; LENGTH - LENGTH_PREFIX] = if is_cont {
            b"%pA\0\0\0\0\0"
        } else {
            b"%s: %pA\0"
        };

        [
            prefix[0], prefix[1], suffix[0], suffix[1], suffix[2], suffix[3], suffix[4], suffix[5],
            suffix[6], suffix[7],
        ]
    }

    // Generate the format strings at compile-time.
    //
    // This avoids the compiler generating the contents on the fly in the stack.
    //
    // Furthermore, `static` instead of `const` is used to share the strings
    // for all the kernel.
    pub static EMERG: [u8; LENGTH] = generate(false, bindings::KERN_EMERG);
    pub static ALERT: [u8; LENGTH] = generate(false, bindings::KERN_ALERT);
    pub static CRIT: [u8; LENGTH] = generate(false, bindings::KERN_CRIT);
    pub static ERR: [u8; LENGTH] = generate(false, bindings::KERN_ERR);
    pub static WARNING: [u8; LENGTH] = generate(false, bindings::KERN_WARNING);
    pub static NOTICE: [u8; LENGTH] = generate(false, bindings::KERN_NOTICE);
    pub static INFO: [u8; LENGTH] = generate(false, bindings::KERN_INFO);
    pub static DEBUG: [u8; LENGTH] = generate(false, bindings::KERN_DEBUG);
}

const __LOG_PREFIX: &[u8] = b"nvalloc\0";

/// Prints a message via the kernel's [`_printk`].
///
/// Public but hidden since it should only be used from public macros.
///
/// # Safety
///
/// The format string must be one of the ones in [`format_strings`], and
/// the module name must be null-terminated.
///
/// [`_printk`]: ../../../../include/linux/_printk.h
#[doc(hidden)]
pub unsafe fn call_printk(format_string: &[u8; format_strings::LENGTH], args: &fmt::Arguments<'_>) {
    // `_printk` does not seem to fail in any path.
    nvalloc_printk(
        format_string.as_ptr() as _,
        __LOG_PREFIX.as_ptr(),
        args as *const _ as *const c_void,
    );
}

/// Allows formatting of [`fmt::Arguments`] into a raw buffer.
///
/// It does not fail if callers write past the end of the buffer so that they can calculate the
/// size required to fit everything.
///
/// # Invariants
///
/// The memory region between `pos` (inclusive) and `end` (exclusive) is valid for writes if `pos`
/// is less than `end`.
pub(crate) struct RawFormatter {
    // Use `usize` to use `saturating_*` functions.
    beg: usize,
    pos: usize,
    end: usize,
}

impl RawFormatter {
    /// Creates a new instance of [`RawFormatter`] with an empty buffer.
    fn new() -> Self {
        // INVARIANT: The buffer is empty, so the region that needs to be writable is empty.
        Self {
            beg: 0,
            pos: 0,
            end: 0,
        }
    }

    /// Creates a new instance of [`RawFormatter`] with the given buffer pointers.
    ///
    /// # Safety
    ///
    /// If `pos` is less than `end`, then the region between `pos` (inclusive) and `end`
    /// (exclusive) must be valid for writes for the lifetime of the returned [`RawFormatter`].
    pub(crate) unsafe fn from_ptrs(pos: *mut u8, end: *mut u8) -> Self {
        // INVARIANT: The safety requierments guarantee the type invariants.
        Self {
            beg: pos as _,
            pos: pos as _,
            end: end as _,
        }
    }

    /// Creates a new instance of [`RawFormatter`] with the given buffer.
    ///
    /// # Safety
    ///
    /// The memory region starting at `buf` and extending for `len` bytes must be valid for writes
    /// for the lifetime of the returned [`RawFormatter`].
    pub(crate) unsafe fn from_buffer(buf: *mut u8, len: usize) -> Self {
        let pos = buf as usize;
        // INVARIANT: We ensure that `end` is never less then `buf`, and the safety requirements
        // guarantees that the memory region is valid for writes.
        Self {
            pos,
            beg: pos,
            end: pos.saturating_add(len),
        }
    }

    /// Returns the current insert position.
    ///
    /// N.B. It may point to invalid memory.
    pub(crate) fn pos(&self) -> *mut u8 {
        self.pos as _
    }

    /// Return the number of bytes written to the formatter.
    pub(crate) fn bytes_written(&self) -> usize {
        self.pos - self.beg
    }
}

impl fmt::Write for RawFormatter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // `pos` value after writing `len` bytes. This does not have to be bounded by `end`, but we
        // don't want it to wrap around to 0.
        let pos_new = self.pos.saturating_add(s.len());

        // Amount that we can copy. `saturating_sub` ensures we get 0 if `pos` goes past `end`.
        let len_to_copy = core::cmp::min(pos_new, self.end).saturating_sub(self.pos);

        if len_to_copy > 0 {
            // SAFETY: If `len_to_copy` is non-zero, then we know `pos` has not gone past `end`
            // yet, so it is valid for write per the type invariants.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    s.as_bytes().as_ptr(),
                    self.pos as *mut u8,
                    len_to_copy,
                )
            };
        }

        self.pos = pos_new;
        Ok(())
    }
}