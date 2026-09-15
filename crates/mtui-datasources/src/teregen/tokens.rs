//! The on-disk cache for a minted teregen bearer token (P2-D2).
//!
//! One `0600` file under the XDG data dir
//! (`<data_dir>/teregen-token.json`), written with
//! [`mtui_config::atomic::write`]:
//!
//! ```json
//! {"base": "https://qam.suse.de/api/v2", "principal": "<u>",
//!  "token": "<64 hex>", "minted_at": "2026-09-13T18:22:01Z"}
//! ```
//!
//! `base` and `principal` are **scoping keys, not decoration**: a cached entry
//! is used only when both match the request, so a token minted against a
//! staging teregen is never presented to production and a token minted for
//! user A is never presented as user B. `minted_at` is informational (a probe
//! can print the age); mtui does **no** client-side expiry arithmetic — the
//! server's 30-day policy is the server's, and expiry arrives as a `401` that
//! triggers a re-mint.
//!
//! An unreadable or malformed file is treated as "no token" at `DEBUG` and
//! overwritten on the next mint (lenient, like [`mtui_config::Config::load`]).
//! A group/world-readable file **warns** and is still used, mirroring
//! `mtui_datasources::obs::oscrc`'s loose-permissions warning rather than
//! inventing a second policy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name under the XDG data dir.
const TOKEN_FILE_NAME: &str = "teregen-token.json";

/// A minted teregen bearer token, cached on disk.
///
/// [`Debug`] is hand-written to mask `token` — a `derive` would print it the
/// first time anyone adds a `tracing::debug!("{cached:?}")` (P2-D7).
#[derive(Clone, Serialize, Deserialize)]
pub struct CachedToken {
    /// The teregen v2 base URL this token was minted against.
    pub base: String,
    /// The principal (oscrc user) this token was minted for.
    pub principal: String,
    /// The 64-hex bearer token. Never logged; masked in [`Debug`].
    pub token: String,
    /// An RFC3339 timestamp of when the token was minted, informational only.
    pub minted_at: String,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("base", &self.base)
            .field("principal", &self.principal)
            .field("token", &"<redacted>")
            .field("minted_at", &self.minted_at)
            .finish()
    }
}

/// The on-disk cache for one [`CachedToken`], scoped to a single file path.
///
/// `Clone` is cheap (a single `PathBuf`) and lets a caller keep a handle for
/// its own diagnostics (e.g. `cargo xtask teregen-login` reporting the store
/// path/age) after handing one to [`crate::teregen::TeregenAuth`].
#[derive(Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// Resolve the store at the XDG data dir's `teregen-token.json`.
    ///
    /// Returns `None` when the data dir cannot be resolved (no `HOME`), the
    /// same condition under which [`mtui_config::data_dir`] itself gives up.
    #[must_use]
    pub fn new() -> Option<Self> {
        mtui_config::data_dir().map(|dir| Self {
            path: dir.join(TOKEN_FILE_NAME),
        })
    }

    /// Build a store at an explicit path — the test seam, avoiding any env
    /// mutation or `serial_test` requirement.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path this store reads/writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load a cached token, if one exists and is scoped to `base`/`principal`.
    ///
    /// Any fault — missing file, unreadable, malformed JSON, or a
    /// base/principal mismatch — is logged at `DEBUG` and folded to `None`
    /// rather than surfaced as an error: an absent or stale cache is not a
    /// failure, only a signal to mint.
    #[must_use]
    pub fn load(&self, base: &str, principal: &str) -> Option<CachedToken> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(
                    "teregen token store {} unreadable, treating as no token: {e}",
                    self.path.display()
                );
                return None;
            }
        };
        warn_loose_permissions(&self.path);

        let cached: CachedToken = match serde_json::from_slice(&bytes) {
            Ok(cached) => cached,
            Err(e) => {
                tracing::debug!(
                    "teregen token store {} is malformed, treating as no token: {e}",
                    self.path.display()
                );
                return None;
            }
        };

        if cached.base != base || cached.principal != principal {
            tracing::debug!(
                "teregen token store {} is scoped to a different base/principal, \
                 treating as no token",
                self.path.display()
            );
            return None;
        }

        Some(cached)
    }

    /// Atomically write `cached` to the store, `0600`.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from [`mtui_config::atomic::write`], or a
    /// serialization failure (`CachedToken` is always representable as JSON,
    /// so this should not occur in practice).
    pub fn store(&self, cached: &CachedToken) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(cached).map_err(std::io::Error::other)?;
        mtui_config::atomic::write(&data, &self.path)
    }
}

/// Warn (Unix only) when the token file is group/world-accessible.
///
/// Checks `st_mode & (S_IRWXG | S_IRWXO)`, mirroring
/// `obs::oscrc::warn_loose_permissions`. On non-Unix targets there are no such
/// permission bits, so this is a no-op.
#[cfg(unix)]
fn warn_loose_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            "teregen token store {} is group/world-accessible; tighten it to 0600",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_loose_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manual [`Debug`] must never print the token — the only case unit
    /// tests need cover directly; the store's I/O behaviour is covered by
    /// `tests/teregen_tokens.rs`.
    #[test]
    fn debug_never_prints_the_token() {
        let cached = CachedToken {
            base: "base".to_owned(),
            principal: "alice".to_owned(),
            token: "a".repeat(64),
            minted_at: "2026-09-13T18:22:01Z".to_owned(),
        };
        let rendered = format!("{cached:?}");
        assert!(!rendered.contains(&cached.token));
        assert!(rendered.contains("redacted"));
    }
}
