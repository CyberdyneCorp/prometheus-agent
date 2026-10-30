/// Reads the first byte behind `ptr`.
///
/// # Safety
/// `ptr` must be valid for a one-byte read.
pub unsafe fn first_byte(ptr: *const u8) -> u8 {
    // SAFETY: the caller guarantees `ptr` is valid for a one-byte read.
    unsafe { *ptr }
}
