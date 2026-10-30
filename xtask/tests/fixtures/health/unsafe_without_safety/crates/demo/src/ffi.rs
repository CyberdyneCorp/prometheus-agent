/// Reads the first byte behind `ptr`.
///
/// # Safety
/// `ptr` must be valid for a one-byte read.
pub unsafe fn first_byte(ptr: *const u8) -> u8 {
    unsafe { *ptr }
}
