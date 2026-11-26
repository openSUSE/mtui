//! Small encoding and clock helpers shared across the crate.

/// Lower-case hex of `bytes`.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode exactly `N` bytes from `2 * N` hex digits; `None` on any other
/// length or a non-hex digit.
pub(crate) fn hex_decode<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let (chunks, _) = hex.as_bytes().as_chunks::<2>();
    for (i, chunk) in chunks.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// Nanoseconds since the Unix epoch, saturating at `u64::MAX`; `0` before it.
pub(crate) fn epoch_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_is_lower_case() {
        let bytes = [0x00, 0x0f, 0xa5, 0xff];
        let hex = hex_encode(&bytes);
        assert_eq!(hex, "000fa5ff");
        assert_eq!(hex_decode::<4>(&hex), Some(bytes));
    }

    #[test]
    fn hex_decode_refuses_wrong_length_and_non_hex() {
        assert_eq!(hex_decode::<2>("00"), None);
        assert_eq!(hex_decode::<2>("000000"), None);
        assert_eq!(hex_decode::<2>("00zz"), None);
    }
}
