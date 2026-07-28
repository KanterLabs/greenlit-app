//! The one module in the entire workspace permitted to use `unsafe`.
//!
//! It wraps the kernel `mount(2)` call used to stack a writable overlay over
//! the read-only repo checkout. Every other module keeps the crate-level
//! `#![deny(unsafe_code)]`; this file re-enables it for this single, audited
//! FFI boundary. There is exactly one `unsafe` block, and it does exactly one
//! thing: invoke `libc::mount`.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Mount an overlay filesystem at `target` with the given `data` option string
/// (`lowerdir=…,upperdir=…,workdir=…`, built by
/// [`crate::strategy::overlay_options`]).
///
/// Returns `Ok(())` on success, or the `io::Error` carrying the kernel `errno`
/// on failure (which the caller classifies to decide whether to fall back).
///
/// # Errors
///
/// Fails if `target` or `data` cannot be encoded as C strings (they contain an
/// interior NUL byte), or if `mount(2)` itself fails.
pub(crate) fn mount_overlay(target: &Path, data: &str) -> io::Result<()> {
    // "overlay" is both the source placeholder and the filesystem type for an
    // overlayfs mount (see `man 8 mount`, overlay section). These `CString`s,
    // and `data_c`, are bound to locals that outlive the `mount` call below, so
    // the raw pointers handed to the kernel stay valid for its full duration.
    let source = cstring("overlay")?;
    let fstype = cstring("overlay")?;
    let target_c = cstring_path(target)?;
    let data_c = cstring(data)?;

    // SAFETY: `libc::mount` is an FFI call into the kernel. Its contract is that
    // the four pointer arguments are either NULL or point to NUL-terminated C
    // strings valid for the duration of the call:
    //  - `source`, `fstype`, `target_c`, and `data_c` are non-null pointers into
    //    `CString`s owned by the locals above, each guaranteed NUL-terminated by
    //    `CString` and kept alive across the call (they are dropped only at the
    //    end of this function, after `mount` returns).
    //  - `flags` is `0`: no `MS_*` bit is set, which is a valid flag value.
    // The call performs no Rust-side memory access and cannot create dangling
    // references; the kernel validates the mount semantically and reports any
    // problem via `errno`, which we read immediately with `last_os_error`.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            0,
            data_c.as_ptr().cast::<libc::c_void>(),
        )
    };

    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Encode a `&str` as a C string, mapping an interior NUL to an `io::Error`.
fn cstring(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// Encode a path as a C string from its raw bytes (no lossy UTF-8 round-trip).
fn cstring_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}
