//! Error type for `mtui-config`.
//!
//! Parsing is **lenient** by design (see [`crate::Config::load`]): a missing or
//! malformed config file is logged and skipped, and defaults are applied — a
//! bad option never hard-fails startup. This mirrors upstream mtui, which logs
//! the `configparser` error and carries on.
//!
//! [`ConfigError`] therefore surfaces only from the *internal* single-file read
//! helper; the public loader converts it into a `tracing::error!` and continues.
//! It is still a proper typed error so callers who want strict behaviour (e.g.
//! a future `--strict-config`) can opt in.

use std::path::PathBuf;

use thiserror::Error;

/// An error reading or parsing a single config file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file could not be read from disk.
    #[error("failed to read config file {path}: {source}")]
    Io {
        /// The offending path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The config file was not valid TOML (or violated the expected schema).
    #[error("failed to parse config file {path}: {source}")]
    Toml {
        /// The offending path.
        path: PathBuf,
        /// The underlying TOML deserialisation error.
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_message_names_path() {
        let err = ConfigError::Io {
            path: PathBuf::from("/nope.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "boom"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/nope.toml"), "got: {msg}");
        assert!(msg.contains("boom"), "got: {msg}");
    }

    #[test]
    fn toml_error_message_names_path() {
        // Produce a real toml::de::Error by parsing invalid TOML.
        let de_err = toml::from_str::<toml::Table>("= not valid").unwrap_err();
        let err = ConfigError::Toml {
            path: PathBuf::from("/bad.toml"),
            source: de_err,
        };
        assert!(err.to_string().contains("/bad.toml"));
    }
}
