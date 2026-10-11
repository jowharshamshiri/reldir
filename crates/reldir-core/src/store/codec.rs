//! Byte-level building blocks shared by the binary formats and the canonical
//! JSON documents: varints, a bounds-checked cursor, and JSON string escaping.
//!
//! Every decoder here is total: malformed input is a [`Corruption`], never a
//! panic, and every length read from input is checked against the bytes that
//! remain before anything is sliced or allocated.

use super::Corruption;

/// The most bytes a `u64` varint can take: ten groups of seven bits.
pub const MAX_VARINT_BYTES: usize = 10;

/// Append `value` as an unsigned LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// How many bytes [`put_varint`] writes for `value`.
pub fn varint_len(value: u64) -> usize {
    let bits = 64 - value.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

/// A read position over a byte slice.
pub struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn position(&self) -> usize {
        self.at
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Corruption> {
        if n > self.remaining() {
            return Err(Corruption::Truncated);
        }
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, Corruption> {
        Ok(self.take(1)?[0])
    }

    pub fn u32_le(&mut self) -> Result<u32, Corruption> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64_le(&mut self) -> Result<u64, Corruption> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Corruption> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// A minimal LEB128 varint of at most ten bytes. A redundant trailing zero
    /// group is refused as well as an overflow: an encoding with two spellings
    /// would give one structure two object ids.
    pub fn varint(&mut self) -> Result<u64, Corruption> {
        let mut value: u64 = 0;
        for index in 0..MAX_VARINT_BYTES {
            let byte = self.u8().map_err(|_| Corruption::MalformedVarint)?;
            if index == MAX_VARINT_BYTES - 1 && byte > 1 {
                return Err(Corruption::MalformedVarint);
            }
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                if index > 0 && byte == 0 {
                    return Err(Corruption::MalformedVarint);
                }
                return Ok(value);
            }
        }
        Err(Corruption::MalformedVarint)
    }

    /// A varint length followed by that many bytes, the length bounded by
    /// what remains so a corrupt length never drives an allocation.
    pub fn length_prefixed(&mut self) -> Result<&'a [u8], Corruption> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| Corruption::Truncated)?;
        self.take(len)
    }

    /// Fail unless every byte was consumed.
    pub fn finish(&self) -> Result<(), Corruption> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(Corruption::TrailingBytes(n)),
        }
    }
}

/// Append `text` as a JSON string in the RFC 8785 rendering: `"` and `\`
/// escaped, the control characters with a short form (`\b \t \n \f \r`) in
/// it, the other controls as `\u00xx` in lowercase hex, and everything else --
/// DEL and all non-ASCII included -- as its literal UTF-8.
pub fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The largest integer an RFC 8785 rendering represents exactly: JSON numbers
/// are IEEE doubles there, so a larger one has no canonical integer form.
pub const MAX_JSON_INTEGER: u64 = (1 << 53) - 1;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test8203_varints_reject_overlong_overflowing_and_truncated_encodings() {
        for (bytes, ok) in [
            (&[0x00][..], Some(0)),
            (&[0x7f], Some(127)),
            (&[0x80, 0x01], Some(128)),
            (&[0x80, 0x00], None),
            (&[0xff, 0x80, 0x00], None),
            (&[0x80], None),
            (&[], None),
            (
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
                Some(u64::MAX),
            ),
            (
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
                None,
            ),
            (&[0x80; 11], None),
        ] {
            let decoded = Cursor::new(bytes).varint();
            match ok {
                Some(value) => assert_eq!(decoded, Ok(value), "{bytes:?}"),
                None => assert_eq!(decoded, Err(Corruption::MalformedVarint), "{bytes:?}"),
            }
        }
    }

    #[test]
    fn test8204_a_length_prefix_longer_than_the_input_is_truncation_not_allocation() {
        let mut bytes = vec![];
        put_varint(&mut bytes, u64::MAX);
        bytes.extend_from_slice(b"abc");
        assert_eq!(
            Cursor::new(&bytes).length_prefixed(),
            Err(Corruption::Truncated)
        );
    }

    #[test]
    fn test8205_json_strings_render_as_rfc_8785_does() {
        let mut out = String::new();
        push_json_string(&mut out, "a\"b\\c\u{8}\t\n\u{c}\r\u{1}\u{1f}\u{7f}é€😀/");
        assert_eq!(
            out,
            "\"a\\\"b\\\\c\\b\\t\\n\\f\\r\\u0001\\u001f\u{7f}é€😀/\""
        );
        let parsed: String = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, "a\"b\\c\u{8}\t\n\u{c}\r\u{1}\u{1f}\u{7f}é€😀/");
    }

    proptest! {
        #[test]
        fn test8206_varints_round_trip_with_their_predicted_length(value in any::<u64>()) {
            let mut bytes = vec![];
            put_varint(&mut bytes, value);
            prop_assert_eq!(bytes.len(), varint_len(value));
            let mut cursor = Cursor::new(&bytes);
            prop_assert_eq!(cursor.varint(), Ok(value));
            prop_assert!(cursor.finish().is_ok());
        }

        #[test]
        fn test8207_arbitrary_bytes_never_panic_the_cursor(bytes in prop::collection::vec(any::<u8>(), 0..32)) {
            let mut cursor = Cursor::new(&bytes);
            while cursor.length_prefixed().is_ok() {}
        }
    }
}
