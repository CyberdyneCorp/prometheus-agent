//! The encrypting SQLite VFS (spec: session-storage, Encryption at rest
//! option): a shim over the platform VFS that seals every page image it
//! writes to a database, its write-ahead log and its rollback journal
//! ([`super::pages`]) and opens every one it reads. Temporary files
//! (sorter spills, statement journals, transient databases) get a stream
//! cipher under a random key of their own, so no plaintext of an encrypted
//! store ever reaches a file. The shared-memory WAL index holds page
//! numbers and checksums only and passes through.
//!
//! One VFS is registered per key, named after the key's fingerprint; its
//! `pAppData` owns the key for the life of the process. Everything not
//! about file contents is forwarded to the platform VFS unchanged.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Mutex, PoisonError};

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit as _, StreamCipher as _, StreamCipherSeek as _};
use rusqlite::ffi;

use super::pages::{self, Key, Layout, PAGE};

/// What a VFS instance owns.
struct Shared {
    base: *mut ffi::sqlite3_vfs,
    key: Key,
}

/// How one open file's bytes are transformed.
enum Mode {
    /// Page images sealed with the database key.
    Pages(Layout),
    /// A temporary file: a `ChaCha20` keystream at the file offset.
    Stream([u8; 32]),
}

/// The file SQLite allocates `szOsFile` bytes for: ours, then the
/// platform file right after it.
#[repr(C)]
struct File {
    head: ffi::sqlite3_file,
    inner: *mut ffi::sqlite3_file,
    key: *const Key,
    mode: *mut Mode,
}

/// Where the platform file starts inside ours (8-byte aligned).
const INNER: usize = (std::mem::size_of::<File>() + 7) & !7;

static REGISTERED: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());

/// The name of the VFS sealing pages with `key`, registering it on first
/// use.
///
/// # Errors
///
/// No platform VFS, or SQLite refused the registration.
pub(crate) fn name_for(key: &Key) -> Result<String, String> {
    let name = format!("pmt-crypt-{}", super::fingerprint(key));
    let mut registered = REGISTERED.lock().unwrap_or_else(PoisonError::into_inner);
    if registered.iter().any(|(n, _)| *n == name) {
        return Ok(name);
    }
    // SAFETY: sqlite3_vfs_find with a null name returns the default VFS
    // (or null); SQLite keeps it alive for the process.
    let base = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
    if base.is_null() {
        return Err("SQLite has no default VFS".to_owned());
    }
    let shared = Box::into_raw(Box::new(Shared {
        base,
        key: key.clone(),
    }));
    let zname = CString::new(name.clone()).map_err(|e| e.to_string())?;
    // SAFETY: `base` is a valid registered VFS (checked non-null above).
    let base_ref = unsafe { &*base };
    let vfs = Box::new(ffi::sqlite3_vfs {
        iVersion: 2,
        szOsFile: c_int::try_from(INNER).unwrap_or(c_int::MAX) + base_ref.szOsFile,
        mxPathname: base_ref.mxPathname,
        pNext: std::ptr::null_mut(),
        zName: zname.into_raw(),
        pAppData: shared.cast(),
        xOpen: Some(x_open),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: None,
        xDlError: None,
        xDlSym: None,
        xDlClose: None,
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: Some(x_get_last_error),
        xCurrentTimeInt64: Some(x_current_time_int64),
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    });
    let vfs = Box::into_raw(vfs);
    // SAFETY: `vfs` is a fully initialised VFS that lives for the rest of
    // the process (leaked on purpose: SQLite holds the pointer).
    let rc = unsafe { ffi::sqlite3_vfs_register(vfs, 0) };
    if rc != ffi::SQLITE_OK {
        return Err(format!("SQLite refused the encrypting VFS ({rc})"));
    }
    registered.push((name.clone(), vfs as usize));
    Ok(name)
}

/// Sets the per-page reserved space of `conn`'s main database (before its
/// first page is written).
pub(crate) fn reserve_bytes(conn: &rusqlite::Connection, bytes: c_int) -> rusqlite::Result<()> {
    let mut value = bytes;
    // SAFETY: the handle is a live connection owned by `conn`; the file
    // control reads and writes one c_int through the pointer.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_RESERVE_BYTES,
            (&raw mut value).cast::<c_void>(),
        )
    };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None))
    }
}

/// The shared state of `vfs`.
///
/// # Safety
///
/// `vfs` must be one registered by [`name_for`].
unsafe fn shared<'a>(vfs: *mut ffi::sqlite3_vfs) -> &'a Shared {
    // SAFETY: per the contract, pAppData is the leaked `Shared`.
    unsafe { &*(*vfs).pAppData.cast::<Shared>() }
}

fn mode_for(flags: c_int) -> Result<Mode, ()> {
    if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
        return Ok(Mode::Pages(Layout::Database));
    }
    if flags & ffi::SQLITE_OPEN_WAL != 0 {
        return Ok(Mode::Pages(Layout::Wal));
    }
    if flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0 {
        return Ok(Mode::Pages(Layout::Journal));
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| ())?;
    Ok(Mode::Stream(key))
}

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    // SAFETY: SQLite calls this VFS's xOpen with this VFS.
    let shared = unsafe { shared(vfs) };
    let ours = file.cast::<File>();
    // SAFETY: SQLite gave us szOsFile bytes at `file`: our header, then
    // room for the platform file at INNER.
    // INNER is a multiple of 8 and SQLite allocates sqlite3_file with
    // 8-byte alignment, so the platform file is aligned.
    #[allow(clippy::cast_ptr_alignment)]
    let inner = unsafe { file.cast::<u8>().add(INNER).cast::<ffi::sqlite3_file>() };
    // SAFETY: `ours` points to writable memory of at least size_of::<File>().
    unsafe {
        (*ours).head.pMethods = std::ptr::null();
    }
    let Ok(mode) = mode_for(flags) else {
        return ffi::SQLITE_CANTOPEN;
    };
    // SAFETY: base is the platform VFS; inner has its szOsFile bytes.
    let rc = unsafe {
        match (*shared.base).xOpen {
            Some(open) => open(shared.base, name, inner, flags, out_flags),
            None => ffi::SQLITE_CANTOPEN,
        }
    };
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    // SAFETY: the open succeeded; we now own the header fields.
    unsafe {
        (*ours).inner = inner;
        (*ours).key = &raw const shared.key;
        (*ours).mode = Box::into_raw(Box::new(mode));
        (*ours).head.pMethods = &raw const METHODS;
    }
    ffi::SQLITE_OK
}

macro_rules! forward_vfs {
    ($name:ident, $field:ident, ($($arg:ident: $ty:ty),*)) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg: $ty),*) -> c_int {
            // SAFETY: SQLite calls this VFS's methods with this VFS; the
            // platform VFS gets the same arguments with its own pointer.
            unsafe {
                let base = shared(vfs).base;
                match (*base).$field {
                    Some(f) => f(base, $($arg),*),
                    None => ffi::SQLITE_ERROR,
                }
            }
        }
    };
}

forward_vfs!(x_delete, xDelete, (name: *const c_char, sync: c_int));
forward_vfs!(x_access, xAccess, (name: *const c_char, flags: c_int, out: *mut c_int));
forward_vfs!(x_full_pathname, xFullPathname, (name: *const c_char, n: c_int, out: *mut c_char));
forward_vfs!(x_randomness, xRandomness, (n: c_int, out: *mut c_char));
forward_vfs!(x_sleep, xSleep, (micros: c_int));
forward_vfs!(x_current_time, xCurrentTime, (out: *mut f64));
forward_vfs!(x_get_last_error, xGetLastError, (n: c_int, out: *mut c_char));
forward_vfs!(x_current_time_int64, xCurrentTimeInt64, (out: *mut ffi::sqlite3_int64));

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(f_close),
    xRead: Some(f_read),
    xWrite: Some(f_write),
    xTruncate: Some(f_truncate),
    xSync: Some(f_sync),
    xFileSize: Some(f_file_size),
    xLock: Some(f_lock),
    xUnlock: Some(f_unlock),
    xCheckReservedLock: Some(f_check_reserved_lock),
    xFileControl: Some(f_file_control),
    xSectorSize: Some(f_sector_size),
    xDeviceCharacteristics: Some(f_device_characteristics),
    xShmMap: Some(f_shm_map),
    xShmLock: Some(f_shm_lock),
    xShmBarrier: Some(f_shm_barrier),
    xShmUnmap: Some(f_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

/// The platform file under `file` and its methods.
///
/// # Safety
///
/// `file` must have been opened by [`x_open`].
unsafe fn inner(
    file: *mut ffi::sqlite3_file,
) -> (*mut ffi::sqlite3_file, &'static ffi::sqlite3_io_methods) {
    // SAFETY: per the contract, `inner` was set by a successful open and
    // its methods table is the platform VFS's static one.
    unsafe {
        let inner = (*file.cast::<File>()).inner;
        (inner, &*(*inner).pMethods)
    }
}

macro_rules! forward_file {
    ($name:ident, $field:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty, $missing:expr) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file, $($arg: $ty),*) -> $ret {
            // SAFETY: SQLite calls the methods of a file our xOpen opened.
            unsafe {
                let (inner, methods) = inner(file);
                match methods.$field {
                    Some(f) => f(inner, $($arg),*),
                    None => $missing,
                }
            }
        }
    };
}

forward_file!(f_truncate, xTruncate, (size: ffi::sqlite3_int64) -> c_int, ffi::SQLITE_IOERR);
forward_file!(f_sync, xSync, (flags: c_int) -> c_int, ffi::SQLITE_OK);
forward_file!(f_file_size, xFileSize, (out: *mut ffi::sqlite3_int64) -> c_int, ffi::SQLITE_IOERR);
forward_file!(f_lock, xLock, (level: c_int) -> c_int, ffi::SQLITE_OK);
forward_file!(f_unlock, xUnlock, (level: c_int) -> c_int, ffi::SQLITE_OK);
forward_file!(f_check_reserved_lock, xCheckReservedLock, (out: *mut c_int) -> c_int, ffi::SQLITE_OK);
forward_file!(f_file_control, xFileControl, (op: c_int, arg: *mut c_void) -> c_int, ffi::SQLITE_NOTFOUND);
forward_file!(f_shm_map, xShmMap, (page: c_int, size: c_int, extend: c_int, out: *mut *mut c_void) -> c_int, ffi::SQLITE_IOERR);
forward_file!(f_shm_lock, xShmLock, (offset: c_int, n: c_int, flags: c_int) -> c_int, ffi::SQLITE_IOERR);
forward_file!(f_shm_unmap, xShmUnmap, (delete: c_int) -> c_int, ffi::SQLITE_OK);

unsafe extern "C" fn f_shm_barrier(file: *mut ffi::sqlite3_file) {
    // SAFETY: SQLite calls the methods of a file our xOpen opened.
    unsafe {
        let (inner, methods) = inner(file);
        if let Some(f) = methods.xShmBarrier {
            f(inner);
        }
    }
}

unsafe extern "C" fn f_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    // A journal header is one sector: kept below a page so that only page
    // images are page-sized in a journal.
    // SAFETY: SQLite calls the methods of a file our xOpen opened.
    let size = unsafe {
        let (inner, methods) = inner(file);
        methods.xSectorSize.map_or(512, |f| f(inner))
    };
    size.clamp(512, 512)
}

unsafe extern "C" fn f_device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: SQLite calls the methods of a file our xOpen opened.
    let caps = unsafe {
        let (inner, methods) = inner(file);
        methods.xDeviceCharacteristics.map_or(0, |f| f(inner))
    };
    // Powersafe overwrite: SQLite never pads or splits a WAL frame, so a
    // page image is always written whole. Batch-atomic writes would bypass
    // the journal layout.
    (caps | ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE) & !ffi::SQLITE_IOCAP_BATCH_ATOMIC
}

unsafe extern "C" fn f_close(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: SQLite closes a file our xOpen opened, once; the mode box is
    // ours and freed here.
    unsafe {
        let ours = file.cast::<File>();
        let (inner, methods) = inner(file);
        let rc = methods.xClose.map_or(ffi::SQLITE_OK, |f| f(inner));
        drop(Box::from_raw((*ours).mode));
        (*ours).head.pMethods = std::ptr::null();
        rc
    }
}

/// Reads `buf.len()` raw bytes at `offset` from the platform file.
fn raw_read(file: *mut ffi::sqlite3_file, buf: &mut [u8], offset: u64) -> c_int {
    let (Ok(n), Ok(at)) = (c_int::try_from(buf.len()), i64::try_from(offset)) else {
        return ffi::SQLITE_IOERR_READ;
    };
    // SAFETY: `file` was opened by x_open; buf is valid for n bytes.
    unsafe {
        let (inner, methods) = inner(file);
        methods.xRead.map_or(ffi::SQLITE_IOERR_READ, |f| {
            f(inner, buf.as_mut_ptr().cast(), n, at)
        })
    }
}

fn raw_write(file: *mut ffi::sqlite3_file, buf: &[u8], offset: u64) -> c_int {
    let (Ok(n), Ok(at)) = (c_int::try_from(buf.len()), i64::try_from(offset)) else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    // SAFETY: `file` was opened by x_open; buf is valid for n bytes.
    unsafe {
        let (inner, methods) = inner(file);
        methods.xWrite.map_or(ffi::SQLITE_IOERR_WRITE, |f| {
            f(inner, buf.as_ptr().cast(), n, at)
        })
    }
}

fn file_size(file: *mut ffi::sqlite3_file) -> u64 {
    let mut size: ffi::sqlite3_int64 = 0;
    // SAFETY: `file` was opened by x_open; size is a valid out pointer.
    let rc = unsafe { f_file_size(file, &raw mut size) };
    if rc == ffi::SQLITE_OK {
        u64::try_from(size).unwrap_or(0)
    } else {
        0
    }
}

fn keystream(key: &[u8; 32], buf: &mut [u8], offset: u64) {
    let mut cipher = ChaCha20::new(key.into(), &[0u8; 12].into());
    cipher.seek(offset);
    cipher.apply_keystream(buf);
}

unsafe extern "C" fn f_read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let (Ok(len), Ok(offset)) = (usize::try_from(amount), u64::try_from(offset)) else {
        return ffi::SQLITE_IOERR_READ;
    };
    // SAFETY: SQLite passes a buffer of `amount` bytes and a file our
    // xOpen opened; key and mode live as long as the file.
    let (buf, key, mode) = unsafe {
        let ours = file.cast::<File>();
        (
            std::slice::from_raw_parts_mut(buf.cast::<u8>(), len),
            &*(*ours).key,
            &*(*ours).mode,
        )
    };
    let rc = raw_read(file, buf, offset);
    if rc != ffi::SQLITE_OK && rc != ffi::SQLITE_IOERR_SHORT_READ {
        return rc;
    }
    match mode {
        Mode::Stream(stream) => {
            // Only the bytes that exist: SQLite expects zeros past the end.
            let present = file_size(file).saturating_sub(offset);
            let n = usize::try_from(present).unwrap_or(len).min(len);
            keystream(stream, &mut buf[..n], offset);
            rc
        }
        Mode::Pages(layout) => match open_pages(file, key, *layout, buf, offset) {
            Ok(()) => rc,
            Err(rc) => rc,
        },
    }
}

/// The number of the page image at `start`: from its offset in the
/// database, else from the 4 big-endian bytes before it (a WAL frame
/// header's first field, a journal record's first field), taken from
/// `around` (the bytes at `offset` of this read or write) when they are
/// there and from the file otherwise.
fn page_number(
    file: *mut ffi::sqlite3_file,
    layout: Layout,
    start: u64,
    (around, offset): (&[u8], u64),
) -> Result<u32, c_int> {
    let before = match layout {
        Layout::Database => {
            return u32::try_from(start / PAGE as u64 + 1).map_err(|_| ffi::SQLITE_IOERR);
        }
        Layout::Wal => pages::FRAME_HEADER,
        Layout::Journal => 4,
    };
    let at = start.checked_sub(before).ok_or(ffi::SQLITE_IOERR)?;
    let mut number = [0u8; 4];
    match usize::try_from(at.wrapping_sub(offset)) {
        Ok(i) if at >= offset && i + 4 <= around.len() => {
            number.copy_from_slice(&around[i..i + 4]);
        }
        _ => {
            if raw_read(file, &mut number, at) != ffi::SQLITE_OK {
                return Err(ffi::SQLITE_IOERR);
            }
        }
    }
    Ok(u32::from_be_bytes(number))
}

/// Opens page image `page` read at `start`. A blank database page is one
/// SQLite never wrote (it leaves holes: freed pages it skips writing), as
/// is any blank page past the end of the file (a short read's zeros);
/// those stay zeros. Anything else must authenticate as its own page.
fn open_page(
    file: *mut ffi::sqlite3_file,
    (key, layout): (&Key, Layout),
    page: &mut [u8],
    start: u64,
    number: Result<u32, c_int>,
) -> Result<(), c_int> {
    if pages::is_blank(page) && (layout == Layout::Database || start >= file_size(file)) {
        return Ok(());
    }
    pages::open(key, page, number?).map_err(|_| ffi::SQLITE_IOERR_READ)
}

/// Opens the page images inside `buf` (read at `offset`); one only partly
/// inside is read whole on its own first.
fn open_pages(
    file: *mut ffi::sqlite3_file,
    key: &Key,
    layout: Layout,
    buf: &mut [u8],
    offset: u64,
) -> Result<(), c_int> {
    let end = offset + buf.len() as u64;
    for start in pages::pages(layout, offset, buf.len()) {
        let stop = start + PAGE as u64;
        // Headers lie between page images: never changed by opening one.
        let number = page_number(file, layout, start, (buf, offset));
        if start >= offset && stop <= end {
            let at = usize::try_from(start - offset).map_err(|_| ffi::SQLITE_IOERR_READ)?;
            open_page(file, (key, layout), &mut buf[at..at + PAGE], start, number)?;
            continue;
        }
        let mut page = vec![0u8; PAGE];
        let rc = raw_read(file, &mut page, start);
        if rc != ffi::SQLITE_OK && rc != ffi::SQLITE_IOERR_SHORT_READ {
            return Err(rc);
        }
        open_page(file, (key, layout), &mut page, start, number)?;
        let from = start.max(offset);
        let to = stop.min(end);
        let src = usize::try_from(from - start).map_err(|_| ffi::SQLITE_IOERR_READ)?;
        let dst = usize::try_from(from - offset).map_err(|_| ffi::SQLITE_IOERR_READ)?;
        let n = usize::try_from(to - from).map_err(|_| ffi::SQLITE_IOERR_READ)?;
        buf[dst..dst + n].copy_from_slice(&page[src..src + n]);
    }
    Ok(())
}

unsafe extern "C" fn f_write(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let (Ok(len), Ok(offset)) = (usize::try_from(amount), u64::try_from(offset)) else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    // SAFETY: SQLite passes a buffer of `amount` bytes and a file our
    // xOpen opened; key and mode live as long as the file.
    let (plain, key, mode) = unsafe {
        let ours = file.cast::<File>();
        (
            std::slice::from_raw_parts(buf.cast::<u8>(), len),
            &*(*ours).key,
            &*(*ours).mode,
        )
    };
    let mut out = plain.to_vec();
    match mode {
        Mode::Stream(stream) => keystream(stream, &mut out, offset),
        Mode::Pages(layout) => {
            if let Err(rc) = seal_pages(file, key, *layout, (plain, &mut out), offset) {
                return rc;
            }
        }
    }
    raw_write(file, &out, offset)
}

/// Seals the page images of `plain` (written at `offset`) in `out`, its
/// copy.
fn seal_pages(
    file: *mut ffi::sqlite3_file,
    key: &Key,
    layout: Layout,
    (plain, out): (&[u8], &mut [u8]),
    offset: u64,
) -> Result<(), c_int> {
    let len = plain.len();
    for start in pages::pages(layout, offset, len) {
        // A page image is always written whole (powersafe overwrite);
        // anything else would need its old bytes.
        if start < offset || start + PAGE as u64 > offset + len as u64 {
            tracing::error!(
                offset,
                len,
                "partial page write refused by the encrypting VFS"
            );
            return Err(ffi::SQLITE_IOERR_WRITE);
        }
        let at = usize::try_from(start - offset).map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
        // SQLite writes a WAL frame's header, and a journal record's page
        // number, before the page itself.
        let number = page_number(file, layout, start, (plain, offset))
            .map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
        pages::seal(key, &mut out[at..at + PAGE], number).map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
    }
    Ok(())
}

/// The name of the platform VFS (a plaintext copy's target).
pub(crate) fn default_name() -> String {
    // SAFETY: sqlite3_vfs_find(null) returns the default VFS or null; its
    // zName is a NUL-terminated string SQLite keeps alive.
    unsafe {
        let base = ffi::sqlite3_vfs_find(std::ptr::null());
        if base.is_null() || (*base).zName.is_null() {
            return "unix".to_owned();
        }
        CStr::from_ptr((*base).zName).to_string_lossy().into_owned()
    }
}
