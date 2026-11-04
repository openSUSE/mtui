//! Opaque server-minted session handles.
//!
//! Revision 2026-07-28 removes protocol-level sessions (SEP-2567), so each
//! http session carries explicit server-side state keyed by one of these
//! handles. Minted per http session by the
//! [`SessionRegistry`](crate::provider::SessionRegistry), indexed handle →
//! session while the session lives, released with the slot on drop/sweep/close.
//! It is carried on the server and read back through the registry lookup; no
//! tool takes or returns it. It is a bearer: resolving it yields its session
//! with no further check, so it never renders in logs or errors.

use std::fmt;
use std::str::FromStr;

/// A server-minted opaque handle for one live http session: 128 bits of OS
/// entropy, with a 32 lowercase-hex-character rendering
/// ([`to_hex`](Self::to_hex)) as its textual form.
///
/// `Clone` but deliberately not `Copy`: every live copy is bearer bytes, so
/// copies stay explicit. Revocation is the registry index entry's removal on
/// drop/sweep/close — wiping one copy would not revoke the others — so the
/// handle is not zeroized.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionHandle([u8; 16]);

/// A [`SessionHandle`] that failed to parse: the expected shape is fixed, so
/// the message never echoes the offending input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseHandleError;

impl fmt::Display for ParseHandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session handle must be 32 hexadecimal characters")
    }
}

impl std::error::Error for ParseHandleError {}

impl SessionHandle {
    /// Mint a fresh handle from the OS entropy source.
    ///
    /// # Errors
    ///
    /// Returns [`getrandom::Error`] when the OS source is unavailable; the
    /// caller ([`SessionRegistry::try_make_server`](crate::provider::SessionRegistry::try_make_server))
    /// refuses the session rather than reusing a handle.
    pub fn mint() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The canonical 32-character lowercase-hex rendering: the textual form
    /// [`FromStr`] parses back.
    #[cfg_attr(not(test), expect(dead_code, reason = "no non-test caller yet"))]
    #[must_use]
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }
}

/// A bearer: [`Debug`] never renders the value, so logs cannot be replayed as
/// identity. There is deliberately no [`Display`](fmt::Display): the wire
/// rendering is the explicit [`to_hex`](SessionHandle::to_hex), so a future
/// `{handle}` in logging code fails to compile instead of leaking a
/// hijack-capable identity.
impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionHandle(..)")
    }
}

impl FromStr for SessionHandle {
    type Err = ParseHandleError;

    /// Parses the 32-character hex form [`to_hex`](SessionHandle::to_hex)
    /// emits. Hex digits are case-insensitive
    /// by construction (`to_digit(16)`), so an uppercase rendering maps to the
    /// same handle; [`to_hex`](SessionHandle::to_hex) always emits canonical
    /// lowercase.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 {
            return Err(ParseHandleError);
        }
        let (chunks, _) = s.as_bytes().as_chunks::<2>();
        let mut bytes = [0u8; 16];
        for (i, chunk) in chunks.iter().enumerate() {
            let hi = (chunk[0] as char).to_digit(16).ok_or(ParseHandleError)?;
            let lo = (chunk[1] as char).to_digit(16).ok_or(ParseHandleError)?;
            bytes[i] = ((hi << 4) | lo) as u8;
        }
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn mint_produces_distinct_handles() {
        let minted: HashSet<SessionHandle> = (0..256)
            .map(|_| SessionHandle::mint().expect("OS entropy"))
            .collect();
        assert_eq!(minted.len(), 256, "minted handles must not collide");
    }

    #[test]
    fn clone_preserves_the_handle_value() {
        // `Copy` is deliberately absent from the derive list above: every live
        // copy is bearer bytes, so copies stay explicit. That absence is pinned
        // by `clippy::clone_on_copy` on the `.clone()` below (deriving `Copy`
        // fails the clippy gate); this test only pins that `Clone` — the sole
        // way to copy a bearer — preserves the value.
        let handle = SessionHandle::mint().expect("OS entropy");
        let dup = handle.clone();
        assert_eq!(handle, dup);
        assert_eq!(handle.to_hex(), dup.to_hex());
    }

    #[test]
    fn hex_parse_round_trip() {
        let handle = SessionHandle::mint().expect("OS entropy");
        let text = handle.to_hex();
        assert_eq!(text.len(), 32, "hex form is 32 hex chars");
        assert_eq!(text.parse::<SessionHandle>().expect("parse"), handle);
    }

    #[test]
    fn parse_accepts_uppercase_but_canonicalises_lower() {
        let handle = SessionHandle::mint().expect("OS entropy");
        let upper = handle.to_hex().to_ascii_uppercase();
        assert_eq!(upper.parse::<SessionHandle>().expect("parse"), handle);
        assert_eq!(handle.to_hex(), handle.to_hex().to_ascii_lowercase());
    }

    #[test]
    fn parse_rejects_malformed_input() {
        let bads = [
            String::new(),
            "xyz".to_owned(),
            "0".repeat(31),
            "0".repeat(33),
            "0g".repeat(16),
        ];
        for bad in &bads {
            assert!(bad.parse::<SessionHandle>().is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn debug_and_error_never_render_values() {
        let handle = SessionHandle::mint().expect("OS entropy");
        let hex = handle.to_hex();
        assert!(
            !format!("{handle:?}").contains(&hex),
            "Debug must not leak the bearer"
        );
        let err = "0".repeat(31).parse::<SessionHandle>().expect_err("reject");
        assert_eq!(
            err.to_string(),
            "session handle must be 32 hexadecimal characters"
        );
        assert!(
            !err.to_string().contains('0'),
            "error must not echo the input"
        );
    }
}
