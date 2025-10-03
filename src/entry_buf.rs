//! Module limiting the unsafe scope of the `EntryBuf` type.

use super::MAX_ENTRY_SIZE;

/// Represents an `EntryBuf`'s initial state, in which the caller is writing the entry.
pub(crate) enum Writing {}

/// Represents an `EntryBuf`'s later state, in which the caller can read the `\n`-terminated entry.
pub(crate) enum Reading {}

/// The state of an `EntryBuf`: `Writing` or `Reading`.
pub(crate) trait State {}

impl State for Writing {}
impl State for Reading {}

/// A buffer for a single entry, intended to be placed on the stack.
pub(crate) struct EntryBuf<S: State> {
    /// The actual buffer.
    /// Safety invariant: `&buf[0..len]` is initialized and valid UTF-8.
    buf: std::mem::MaybeUninit<[u8; MAX_ENTRY_SIZE]>,

    /// The number of bytes of `buf` which are initialized, in range `[0, MAX_ENTRY_SIZE]`.
    /// In state `Writing`, the range is further reduced to `[0, MAX_ENTRY_SIZE)`, as the final
    /// byte is reserved for the newline.
    len: usize,

    _state: std::marker::PhantomData<S>,
}

impl EntryBuf<Writing> {
    pub(crate) fn new() -> Self {
        Self {
            buf: std::mem::MaybeUninit::uninit(),
            len: 0,
            _state: std::marker::PhantomData,
        }
    }

    /// Terminates with a newline, using the reserved last byte if necessary.
    pub(crate) fn terminate(mut self) -> EntryBuf<Reading> {
        debug_assert!(self.len < MAX_ENTRY_SIZE);
        unsafe {
            *self.unwritten() = b'\n';
        }
        self.len += 1;
        EntryBuf {
            buf: self.buf,
            len: self.len,
            _state: std::marker::PhantomData,
        }
    }

    /// Gets a pointer to the unwritten/uninitialized portion of the buffer.
    /// This is returned as a raw pointer because it's unsound to take a reference to it.
    fn unwritten(&mut self) -> *mut u8 {
        unsafe { (self.buf.as_mut_ptr() as *mut u8).add(self.len) }
    }
}

impl EntryBuf<Reading> {
    /// Gets the written/initialized prefix of the buffer.
    pub(crate) fn get(&self) -> &str {
        // SAFETY:
        // * `self.len <= MAX_ENTRY_SIZE` so the slice is in-bounds.
        // * `&self.buf[0..self.len]` is initialized and valid UTF-8 by
        //   construction and the `write_str` method.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                self.buf.as_ptr() as *const u8,
                self.len,
            ))
        }
    }
}

impl std::fmt::Write for EntryBuf<Writing> {
    /// Writes as many full UTF-8 sequences as possible from of `s` into the
    /// buffer without using the reserved last byte, returning `Err` on
    /// truncation. Note this behavior is different than say
    /// `arrayvec::{ArrayVec, ArrayString}`, which write nothing if the entire
    /// entry doesn't fit.
    fn write_str(&mut self, s: &str) -> Result<(), std::fmt::Error> {
        if self.len == MAX_ENTRY_SIZE {
            // This path can only be taken if terminate() was already called.
            return Err(std::fmt::Error);
        }
        let s = s.as_bytes();
        let mut to_write = s.len();
        if to_write > MAX_ENTRY_SIZE - 1 - self.len {
            to_write = MAX_ENTRY_SIZE - 1 - self.len;
            while to_write > 0 && (s[to_write] & 0b1100_0000) == 0b1000_0000 {
                // We cut in the middle of a UTF-8 sequence; back up to the start of the sequence.
                to_write -= 1;
            }
        }
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), self.unwritten(), to_write);
        }
        self.len += to_write;
        if to_write == s.len() {
            Ok(())
        } else {
            Err(std::fmt::Error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryBuf, MAX_ENTRY_SIZE};
    use std::fmt::Write;

    /// Tests that an entry well under the limit is not truncated.
    #[test]
    fn well_under_limit() {
        let mut buf = EntryBuf::new();
        buf.write_str("foo ").unwrap();
        buf.write_str("bar").unwrap();
        let buf = buf.terminate();
        assert_eq!(buf.get(), "foo bar\n");
    }

    /// Tests that an entry one under the limit is not truncated (it just fits with the `\n`).
    #[test]
    fn one_under_limit() {
        let mut buf = EntryBuf::new();
        let e = "e".repeat(MAX_ENTRY_SIZE - 1);
        buf.write_str(&e).unwrap();
        let buf = buf.terminate();
        assert_eq!(buf.get(), format!("{e}\n"));
    }

    /// Tests that an entry at the limit is truncated and still ends in '\n'.
    #[test]
    fn at_limit() {
        let mut buf = EntryBuf::new();
        let e = "e".repeat(MAX_ENTRY_SIZE);
        buf.write_str(&e).unwrap_err();
        let e_shortened = &e[0..MAX_ENTRY_SIZE - 1];
        let buf = buf.terminate();
        assert_eq!(buf.get(), format!("{e_shortened}\n"));
    }

    /// Tests that a multi-byte UTF-8 character is not split.
    #[test]
    fn multi_byte_utf8() {
        let mut buf = EntryBuf::new();
        let e = "e".repeat(MAX_ENTRY_SIZE - 2) + "é";
        buf.write_str(&e).unwrap_err();
        let e_shortened = &e[0..MAX_ENTRY_SIZE - 2];
        let buf = buf.terminate();
        assert_eq!(buf.get(), format!("{e_shortened}\n"));
    }

    /// Tests that an entry over the limit is truncated and still ends in '\n'.
    #[test]
    fn over_limit() {
        let mut buf = EntryBuf::new();
        let e = "e".repeat(MAX_ENTRY_SIZE + 1);
        buf.write_str(&e).unwrap_err();
        let e_shortened = &e[0..MAX_ENTRY_SIZE - 1];
        let buf = buf.terminate();
        assert_eq!(buf.get(), format!("{e_shortened}\n"));
    }
}
