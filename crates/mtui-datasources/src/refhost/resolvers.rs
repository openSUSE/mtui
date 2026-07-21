//! Strategies for producing a [`Refhosts`] from a YAML source.
//!
//! Ported from upstream `mtui/hosts/refhost/resolvers.py` +
//! `mtui/hosts/refhost/__init__.py` (the `_RefhostsFactory` binding).
//!
//! Each [`Resolver`] knows one way to obtain `refhosts.yml`:
//! - [`PathResolver`] builds a [`Refhosts`] from a local file
//!   (`config.refhosts_path`);
//! - [`HttpsResolver`] downloads it from an HTTPS URL, caching the payload on
//!   disk and only re-fetching once the cache is older than
//!   `config.refhosts_https_expiration`.
//!
//! The [`RefhostsFactory`] tries a set of named resolvers in the order given by
//! `config.refhosts_resolvers` (a comma-separated string), returning the first
//! success and logging every failure along the way.
//!
//! # Testability seams
//! Upstream injects the clock, the `stat` call, the URL opener, the file writer,
//! and the `Refhosts` factory as collaborators so the cache-refresh logic can be
//! driven offline. This port mirrors that with small object-safe traits
//! ([`Clock`], [`FileStat`], [`Fetcher`], [`FileWriter`], [`RefhostsBuilder`]);
//! production wiring uses [`SystemClock`], [`FsStat`], [`HttpFetcher`],
//! [`AtomicFileWriter`], and [`PathRefhostsBuilder`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mtui_config::SslVerify;
use tracing::warn;

use super::store::Refhosts;
use crate::error::RefhostError;
use crate::http::{HttpClient, VerifyPolicy, resolve_verify};

/// The subset of configuration a [`Resolver`] needs.
///
/// Resolvers take this borrowed view rather than the whole `mtui_config::Config`
/// so they stay decoupled from the full config surface and are trivial to
/// construct in tests (mirroring upstream's `SimpleNamespace` test config).
#[derive(Debug, Clone, Copy)]
pub struct ResolveConfig<'a> {
    /// Comma-separated, ordered list of resolver names to try.
    pub refhosts_resolvers: &'a str,
    /// Local filesystem path to a `refhosts.yml` database.
    pub refhosts_path: &'a Path,
    /// HTTPS URI of the refhosts database.
    pub refhosts_https_uri: &'a str,
    /// Seconds before a cached HTTPS fetch is considered stale.
    pub refhosts_https_expiration: u64,
    /// The global TLS verification policy.
    pub ssl_verify: &'a SslVerify,
}

// ---------------------------------------------------------------------------
// Seams (upstream: time_now_getter / statter / urlopener / file_writer /
// refhosts_factory)
// ---------------------------------------------------------------------------

/// A source of the current wall-clock time (upstream `time_now_getter`).
pub trait Clock: Send + Sync {
    /// Current time as seconds since the Unix epoch.
    fn now_unix(&self) -> u64;
}

/// The result of stat-ing the cache file (upstream `statter`).
///
/// A typed three-state so tests can drive every `_is_refresh_needed` branch
/// deterministically without real filesystem timing: the upstream code
/// distinguishes "missing → refresh" (`ENOENT`) from "other OSError →
/// propagate" and "present → compare mtime".
#[derive(Debug)]
pub enum StatResult {
    /// The cache file does not exist (upstream `ENOENT`) → force a refresh.
    NotFound,
    /// The cache file exists with this mtime (seconds since the Unix epoch).
    Mtime(u64),
    /// Stat-ing the file failed for a reason other than "not found"; the error
    /// propagates instead of silently forcing a refresh.
    Err(std::io::Error),
}

/// A `stat`-like probe of the cache file's freshness (upstream `statter`).
pub trait FileStat: Send + Sync {
    /// Stat `path`, mapping the outcome onto a [`StatResult`].
    fn stat(&self, path: &Path) -> StatResult;
}

/// Fetches the raw bytes of a URL under a [`VerifyPolicy`] (upstream
/// `urlopener` + `.read()`).
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// GET `uri` and return the body bytes.
    ///
    /// # Errors
    /// Returns [`RefhostError`] on any transport failure or non-2xx status.
    async fn fetch(&self, uri: &str, verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError>;
}

/// Persists downloaded payload bytes to the cache path (upstream `file_writer`).
pub trait FileWriter: Send + Sync {
    /// Write `bytes` to `path`.
    ///
    /// # Errors
    /// Returns [`RefhostError::Io`] if the file cannot be written.
    fn write(&self, bytes: &[u8], path: &Path) -> Result<(), RefhostError>;
}

/// Builds a [`Refhosts`] from a local path (upstream `refhosts_factory`).
///
/// Seam over [`Refhosts::from_path`] so [`HttpsResolver::resolve`] and
/// [`PathResolver::resolve`] can be tested without a real `refhosts.yml`.
pub trait RefhostsBuilder: Send + Sync {
    /// Build a [`Refhosts`] from the file at `path`.
    ///
    /// # Errors
    /// Returns [`RefhostError`] if the file cannot be read or parsed.
    fn build(&self, path: &Path) -> Result<Refhosts, RefhostError>;
}

// ---------------------------------------------------------------------------
// Production seam implementations
// ---------------------------------------------------------------------------

/// [`Clock`] backed by the real system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// [`FileStat`] backed by [`std::fs::metadata`].
#[derive(Debug, Default, Clone, Copy)]
pub struct FsStat;

impl FileStat for FsStat {
    fn stat(&self, path: &Path) -> StatResult {
        match std::fs::metadata(path) {
            Ok(meta) => match meta.modified() {
                Ok(mtime) => match mtime.duration_since(UNIX_EPOCH) {
                    Ok(d) => StatResult::Mtime(d.as_secs()),
                    // mtime predates the epoch → treat as very old (refresh).
                    Err(_) => StatResult::Mtime(0),
                },
                Err(e) => StatResult::Err(e),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatResult::NotFound,
            Err(e) => StatResult::Err(e),
        }
    }
}

/// [`Fetcher`] backed by the shared [`HttpClient`].
///
/// The client's TLS posture is fixed at build time (see [`HttpClient::new`]), so
/// this fetcher ignores the per-call `verify` argument beyond recording that the
/// resolver already resolved it — the client it holds was constructed with that
/// same policy. It is created via [`HttpFetcher::new`], which builds the client
/// from the effective [`VerifyPolicy`].
#[derive(Debug)]
pub struct HttpFetcher {
    client: HttpClient,
}

impl HttpFetcher {
    /// Build a fetcher whose client verifies TLS per `verify`.
    ///
    /// # Errors
    /// Returns [`RefhostError`] wrapping any client-build failure (e.g. an
    /// unreadable CA bundle).
    pub fn new(verify: VerifyPolicy) -> Result<Self, RefhostError> {
        let client = HttpClient::new(verify).map_err(|e| RefhostError::Io {
            path: "<http client>".to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, uri: &str, _verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError> {
        self.client
            .get_bytes(uri)
            .await
            .map_err(|e| RefhostError::Io {
                path: uri.to_string(),
                source: std::io::Error::other(e.to_string()),
            })
    }
}

/// [`FileWriter`] that writes atomically via [`mtui_config::atomic::write`], the
/// single secure temp-file + rename implementation shared across the workspace.
#[derive(Debug, Default, Clone, Copy)]
pub struct AtomicFileWriter;

impl FileWriter for AtomicFileWriter {
    fn write(&self, bytes: &[u8], path: &Path) -> Result<(), RefhostError> {
        mtui_config::atomic::write(bytes, path).map_err(|source| RefhostError::Io {
            path: path.display().to_string(),
            source,
        })
    }
}

/// [`RefhostsBuilder`] backed by [`Refhosts::from_path`].
#[derive(Debug, Default, Clone, Copy)]
pub struct PathRefhostsBuilder;

impl RefhostsBuilder for PathRefhostsBuilder {
    fn build(&self, path: &Path) -> Result<Refhosts, RefhostError> {
        Refhosts::from_path(path)
    }
}

// ---------------------------------------------------------------------------
// Resolver trait + strategies
// ---------------------------------------------------------------------------

/// A strategy for producing a [`Refhosts`] from a source.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Return a [`Refhosts`] built from this resolver's source.
    ///
    /// # Errors
    /// Returns [`RefhostError`] if this resolver cannot produce a usable
    /// database (missing file, HTTP failure, parse failure, …).
    async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError>;
}

/// Resolve refhosts from a local file at `config.refhosts_path`.
pub struct PathResolver {
    builder: Box<dyn RefhostsBuilder>,
}

impl PathResolver {
    /// A resolver using the production [`PathRefhostsBuilder`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            builder: Box::new(PathRefhostsBuilder),
        }
    }

    /// A resolver with an injected [`RefhostsBuilder`] (for tests).
    #[must_use]
    pub fn with_builder(builder: Box<dyn RefhostsBuilder>) -> Self {
        Self { builder }
    }
}

impl Default for PathResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Resolver for PathResolver {
    async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
        self.builder.build(config.refhosts_path)
    }
}

/// State shared by every refresh of one cache path, guarded by its
/// [`refresh_lock`].
///
/// `failed_at` remembers when the last download attempt failed, as a monotonic
/// [`std::time::Instant`] (deliberately not the scriptable [`Clock`] seam: the
/// comparison is "did that failure happen while *I* was waiting", a monotonic
/// ordering question, not a cache-age computation).
#[derive(Default)]
struct RefreshSlot {
    /// When the most recent download attempt failed; cleared on success.
    failed_at: Option<std::time::Instant>,
}

/// Process-wide per-cache-path refresh locks (single-flight, see
/// [`HttpsResolver::refresh_if_needed`]).
///
/// The lock cannot live on the [`HttpsResolver`] instance:
/// [`RefhostsFactory::production`] is constructed on demand at every call site
/// (Session autoconnect, `list_refhosts`, `add_host`), so concurrent resolves
/// each hold their own resolver and would never share an instance field. Keyed
/// by cache path rather than a single global so unrelated caches (in practice:
/// per-test temp paths — production configures exactly one
/// `config.refhosts_path`) never serialise each other; entries are one `Arc` +
/// one small `Mutex` each and are never removed, which is fine at that
/// cardinality.
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<RefreshSlot>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The process-wide refresh lock for `cache_path`.
fn refresh_lock(cache_path: &Path) -> Arc<tokio::sync::Mutex<RefreshSlot>> {
    let mut locks = REFRESH_LOCKS
        .lock()
        .expect("refresh-lock registry poisoned");
    Arc::clone(locks.entry(cache_path.to_path_buf()).or_default())
}

/// Resolve refhosts from an HTTPS URL, with on-disk caching.
///
/// `resolve` runs the cache-refresh check (re-downloading only when the cache is
/// missing or older than `config.refhosts_https_expiration`) and then builds the
/// [`Refhosts`] from the cached file.
pub struct HttpsResolver {
    clock: Box<dyn Clock>,
    stat: Box<dyn FileStat>,
    fetcher: Box<dyn Fetcher>,
    writer: Box<dyn FileWriter>,
    builder: Box<dyn RefhostsBuilder>,
    cache_path: PathBuf,
}

impl HttpsResolver {
    /// Build an HTTPS resolver from injected seams.
    #[must_use]
    pub fn new(
        clock: Box<dyn Clock>,
        stat: Box<dyn FileStat>,
        fetcher: Box<dyn Fetcher>,
        writer: Box<dyn FileWriter>,
        builder: Box<dyn RefhostsBuilder>,
        cache_path: PathBuf,
    ) -> Self {
        Self {
            clock,
            stat,
            fetcher,
            writer,
            builder,
            cache_path,
        }
    }

    /// Whether the cache should be re-downloaded.
    ///
    /// Missing cache → always refresh (upstream `ENOENT` branch). A non-"not
    /// found" stat error propagates. Otherwise refresh iff the cache is older
    /// than `expiration` seconds.
    ///
    /// # Errors
    /// Returns [`RefhostError::Io`] if stat-ing the cache fails for a reason
    /// other than "not found".
    fn is_refresh_needed(&self, expiration: u64) -> Result<bool, RefhostError> {
        match self.stat.stat(&self.cache_path) {
            StatResult::NotFound => Ok(true),
            StatResult::Err(source) => Err(RefhostError::Io {
                path: self.cache_path.display().to_string(),
                source,
            }),
            StatResult::Mtime(mtime) => {
                let age = self.clock.now_unix().saturating_sub(mtime);
                Ok(age > expiration)
            }
        }
    }

    /// Download `uri` under `verify` and persist it to the cache path.
    async fn refresh(&self, uri: &str, verify: VerifyPolicy) -> Result<(), RefhostError> {
        let bytes = self.fetcher.fetch(uri, verify).await?;
        self.writer.write(&bytes, &self.cache_path)
    }

    /// Refresh the cache if it is missing or stale.
    ///
    /// Concurrent resolves **single-flight** the download: the first caller to
    /// see a stale cache takes the per-cache-path [`refresh_lock`] and fetches;
    /// callers arriving meanwhile wait on the same lock and then *re-check*
    /// freshness — the leader's write bumped the cache mtime, so they build
    /// from the fresh cache instead of stampeding the server with duplicate
    /// downloads. One concurrent download, not one per caller — and that holds
    /// on failure too: a waiter that waited through a *failed* download fails
    /// fast ([`RefhostError::RefreshJustFailed`]) instead of repeating an
    /// attempt that would almost certainly fail again, so a down server costs
    /// the group one transport timeout, not one per waiter. A *later* caller
    /// (one that was not yet waiting when the failure happened) retries
    /// normally.
    async fn refresh_if_needed(&self, config: ResolveConfig<'_>) -> Result<(), RefhostError> {
        // Unlocked fast path: the common case (fresh cache) takes no lock.
        if !self.is_refresh_needed(config.refhosts_https_expiration)? {
            return Ok(());
        }
        let wait_started = std::time::Instant::now();
        let lock = refresh_lock(&self.cache_path);
        let mut slot = lock.lock().await;
        // Re-check under the lock: the cache may have been refreshed while
        // this task waited for the leader to finish.
        if self.is_refresh_needed(config.refhosts_https_expiration)? {
            // Still stale, so no successful refresh happened meanwhile. If a
            // download *failed* while this task waited, adopt that outcome
            // rather than serially re-fetching.
            if slot.failed_at.is_some_and(|failed| failed > wait_started) {
                return Err(RefhostError::RefreshJustFailed);
            }
            // refhosts.yml is served from an internal SUSE host; verify by
            // default but let the global [mtui] ssl_verify policy override.
            let default = VerifyPolicy::Default(true);
            let override_ = policy_override(config.ssl_verify);
            let verify = resolve_verify(default, override_);
            match self.refresh(config.refhosts_https_uri, verify).await {
                Ok(()) => slot.failed_at = None,
                Err(e) => {
                    slot.failed_at = Some(std::time::Instant::now());
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Resolver for HttpsResolver {
    async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
        self.refresh_if_needed(config).await?;
        self.builder.build(&self.cache_path)
    }
}

/// Map the global [`SslVerify`] onto the per-call verify override.
///
/// Upstream passes `config.ssl_verify` (which is `None` when unset) straight to
/// `resolve_verify(True, ...)`. Here an unset policy is modelled as
/// [`SslVerify::Enabled`] being the config default, so we only *override* the
/// per-site `Default(true)` when the user explicitly disabled verification or
/// named a CA bundle.
fn policy_override(ssl_verify: &SslVerify) -> Option<VerifyPolicy> {
    match ssl_verify {
        // Verify-by-default is already the per-site default; no override needed.
        SslVerify::Enabled => None,
        other => Some(VerifyPolicy::from_config(other)),
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Dispatches to a configured [`Resolver`] to build a [`Refhosts`].
///
/// Ported from upstream `_RefhostsFactory`: `config.refhosts_resolvers` is a
/// comma-separated, ordered list of resolver names; each is tried in turn.
/// Unknown names are logged and skipped; a resolver error is logged (with its
/// cause) and the next resolver is tried. If every resolver is exhausted,
/// [`RefhostError::ResolveFailed`] is returned.
pub struct RefhostsFactory {
    resolvers: Vec<(String, Box<dyn Resolver>)>,
}

impl RefhostsFactory {
    /// Build a factory from a name→resolver association list.
    ///
    /// The list preserves insertion order, but the *try* order is always the one
    /// named by `config.refhosts_resolvers` at resolve time.
    #[must_use]
    pub fn new(resolvers: Vec<(String, Box<dyn Resolver>)>) -> Self {
        Self { resolvers }
    }

    /// The production factory: an `https` resolver + a `path` resolver.
    ///
    /// # Errors
    /// Returns [`RefhostError`] if the HTTPS fetcher's client cannot be built
    /// (e.g. an unreadable configured CA bundle).
    pub fn production(cache_path: PathBuf, verify: VerifyPolicy) -> Result<Self, RefhostError> {
        let https = HttpsResolver::new(
            Box::new(SystemClock),
            Box::new(FsStat),
            Box::new(HttpFetcher::new(verify)?),
            Box::new(AtomicFileWriter),
            Box::new(PathRefhostsBuilder),
            cache_path,
        );
        Ok(Self::new(vec![
            ("https".to_string(), Box::new(https)),
            ("path".to_string(), Box::new(PathResolver::new())),
        ]))
    }

    fn get(&self, name: &str) -> Option<&dyn Resolver> {
        self.resolvers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.as_ref())
    }

    /// Try each configured resolver in order; return the first success.
    ///
    /// # Errors
    /// Returns [`RefhostError::ResolveFailed`] if no resolver produces a usable
    /// database.
    pub async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
        for name in config.refhosts_resolvers.split(',').map(str::trim) {
            let Some(resolver) = self.get(name) else {
                warn!("Refhosts: invalid resolver: {name}");
                continue;
            };
            match resolver.resolve(config).await {
                Ok(refhosts) => return Ok(refhosts),
                Err(e) => warn!("Refhosts: resolver {name} failed: {e}"),
            }
        }
        Err(RefhostError::ResolveFailed)
    }
}

#[cfg(test)]
mod tests {
    //! Ported from upstream `tests/test_refhost.py:405-600` (the "Resolvers and
    //! _RefhostsFactory" block). Upstream drives each collaborator with a
    //! `MagicMock`; the Rust analogues are the mock seams below, which record
    //! their calls for assertion.
    //!
    //! The refresh single-flight tests are original (not ported): upstream is
    //! single-threaded and has no locking to port.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Recorded `(uri, verify.verifies())` fetch calls.
    type FetchLog = Arc<Mutex<Vec<(String, bool)>>>;
    /// Recorded `(bytes, path)` writes.
    type WriteLog = Arc<Mutex<Vec<(Vec<u8>, PathBuf)>>>;

    const FIXTURE_HOSTS: &str = "\
default:
  - name: host.example.com
    arch: x86_64
    product:
      name: sles
      version:
        major: 15
        minor: 5
";

    fn cfg<'a>(
        resolvers: &'a str,
        path: &'a Path,
        uri: &'a str,
        expiration: u64,
        ssl: &'a SslVerify,
    ) -> ResolveConfig<'a> {
        ResolveConfig {
            refhosts_resolvers: resolvers,
            refhosts_path: path,
            refhosts_https_uri: uri,
            refhosts_https_expiration: expiration,
            ssl_verify: ssl,
        }
    }

    // --- mock seams --------------------------------------------------------

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_unix(&self) -> u64 {
            self.0
        }
    }

    /// A stat seam serving one scripted outcome.
    enum ScriptedStat {
        NotFound,
        Mtime(u64),
        Denied,
    }
    impl FileStat for ScriptedStat {
        fn stat(&self, _path: &Path) -> StatResult {
            match self {
                ScriptedStat::NotFound => StatResult::NotFound,
                ScriptedStat::Mtime(m) => StatResult::Mtime(*m),
                ScriptedStat::Denied => StatResult::Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                )),
            }
        }
    }

    /// A fetcher recording `(uri, verify.verifies())` and serving fixed bytes.
    #[derive(Clone)]
    struct RecordingFetcher {
        calls: FetchLog,
        payload: Vec<u8>,
    }
    impl RecordingFetcher {
        fn new(payload: &[u8]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                payload: payload.to_vec(),
            }
        }
    }
    #[async_trait]
    impl Fetcher for RecordingFetcher {
        async fn fetch(&self, uri: &str, verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError> {
            self.calls
                .lock()
                .unwrap()
                .push((uri.to_string(), verify.verifies()));
            Ok(self.payload.clone())
        }
    }

    /// A writer recording `(bytes, path)` of each write.
    #[derive(Clone, Default)]
    struct RecordingWriter {
        writes: WriteLog,
    }
    impl FileWriter for RecordingWriter {
        fn write(&self, bytes: &[u8], path: &Path) -> Result<(), RefhostError> {
            self.writes
                .lock()
                .unwrap()
                .push((bytes.to_vec(), path.to_path_buf()));
            Ok(())
        }
    }

    /// A builder recording the path it was asked to build and serving a store.
    #[derive(Clone)]
    struct RecordingBuilder {
        built: Arc<Mutex<Vec<PathBuf>>>,
    }
    impl RecordingBuilder {
        fn new() -> Self {
            Self {
                built: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }
    impl RefhostsBuilder for RecordingBuilder {
        fn build(&self, path: &Path) -> Result<Refhosts, RefhostError> {
            self.built.lock().unwrap().push(path.to_path_buf());
            Ok(Refhosts::from_hosts(Vec::new()))
        }
    }

    /// A resolver double: scripted success/failure, recording call count.
    struct MockResolver {
        outcome: Result<(), ()>,
        calls: Arc<AtomicU64>,
    }
    impl MockResolver {
        fn ok() -> (Self, Arc<AtomicU64>) {
            let calls = Arc::new(AtomicU64::new(0));
            (
                Self {
                    outcome: Ok(()),
                    calls: calls.clone(),
                },
                calls,
            )
        }
        fn failing() -> (Self, Arc<AtomicU64>) {
            let calls = Arc::new(AtomicU64::new(0));
            (
                Self {
                    outcome: Err(()),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }
    #[async_trait]
    impl Resolver for MockResolver {
        async fn resolve(&self, _config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                Ok(()) => Ok(Refhosts::from_hosts(Vec::new())),
                Err(()) => Err(RefhostError::ResolveFailed),
            }
        }
    }

    // --- PathResolver ------------------------------------------------------

    #[tokio::test]
    async fn path_resolver_uses_configured_path() {
        let builder = RecordingBuilder::new();
        let built = builder.built.clone();
        let resolver = PathResolver::with_builder(Box::new(builder));
        let path = PathBuf::from("/etc/refhosts.yml");
        let ssl = SslVerify::Enabled;
        resolver
            .resolve(cfg("path", &path, "https://x", 3600, &ssl))
            .await
            .unwrap();
        assert_eq!(*built.lock().unwrap(), vec![path]);
    }

    // --- HttpsResolver: resolve --------------------------------------------

    fn https_resolver(
        clock: u64,
        stat: ScriptedStat,
        fetcher: RecordingFetcher,
        writer: RecordingWriter,
        builder: RecordingBuilder,
        cache: &str,
    ) -> HttpsResolver {
        HttpsResolver::new(
            Box::new(FixedClock(clock)),
            Box::new(stat),
            Box::new(fetcher),
            Box::new(writer),
            Box::new(builder),
            PathBuf::from(cache),
        )
    }

    #[tokio::test]
    async fn resolve_uses_cache_path_after_refresh_check() {
        // Fresh cache (delta = 1, expiration 3600) → no fetch, build from cache.
        let fetcher = RecordingFetcher::new(b"");
        let builder = RecordingBuilder::new();
        let built = builder.built.clone();
        let fetch_calls = fetcher.calls.clone();
        let resolver = https_resolver(
            1_000_000,
            ScriptedStat::Mtime(999_999),
            fetcher,
            RecordingWriter::default(),
            builder,
            "/tmp/refhosts.yml",
        );
        let ssl = SslVerify::Enabled;
        resolver
            .resolve(cfg("https", Path::new("/x"), "https://x", 3600, &ssl))
            .await
            .unwrap();
        assert!(
            fetch_calls.lock().unwrap().is_empty(),
            "fresh cache: no fetch"
        );
        assert_eq!(
            *built.lock().unwrap(),
            vec![PathBuf::from("/tmp/refhosts.yml")]
        );
    }

    // --- HttpsResolver: is_refresh_needed ----------------------------------

    // --- HttpsResolver: refresh single-flight ------------------------------

    /// A fetcher that counts calls and dwells long enough for every concurrent
    /// resolver to pass its unlocked staleness pre-check while the leader is
    /// still mid-download — the exact window the thundering herd lived in.
    struct SlowCountingFetcher {
        calls: Arc<AtomicU64>,
        payload: Vec<u8>,
    }
    #[async_trait]
    impl Fetcher for SlowCountingFetcher {
        async fn fetch(&self, _uri: &str, _verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(self.payload.clone())
        }
    }

    /// A fetcher that counts calls, dwells like [`SlowCountingFetcher`], and
    /// always fails — a down server with a real transport timeout.
    struct SlowFailingFetcher {
        calls: Arc<AtomicU64>,
    }
    #[async_trait]
    impl Fetcher for SlowFailingFetcher {
        async fn fetch(&self, _uri: &str, _verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(RefhostError::ResolveFailed)
        }
    }

    /// A fetcher whose first call fails and later calls succeed.
    struct FailOnceFetcher {
        calls: Arc<AtomicU64>,
    }
    #[async_trait]
    impl Fetcher for FailOnceFetcher {
        async fn fetch(&self, _uri: &str, _verify: VerifyPolicy) -> Result<Vec<u8>, RefhostError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(RefhostError::ResolveFailed);
            }
            Ok(b"payload".to_vec())
        }
    }

    /// One production-shaped resolver: real clock/stat/writer against a real
    /// cache file, mirroring how every call site builds its own instance.
    fn fs_resolver(fetcher: Box<dyn Fetcher>, cache: &Path) -> HttpsResolver {
        HttpsResolver::new(
            Box::new(SystemClock),
            Box::new(FsStat),
            fetcher,
            Box::new(AtomicFileWriter),
            Box::new(RecordingBuilder::new()),
            cache.to_path_buf(),
        )
    }

    #[tokio::test]
    async fn concurrent_stale_resolves_share_one_fetch() {
        // Four resolver *instances* over one missing cache file — the shape of
        // four concurrent sessions each constructing their own factory. Only
        // one download may happen; the waiters must re-check and build from
        // the leader's freshly-written cache.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let calls = Arc::new(AtomicU64::new(0));
        let ssl = SslVerify::Enabled;

        let resolves = (0..4).map(|_| {
            let fetcher = Box::new(SlowCountingFetcher {
                calls: calls.clone(),
                payload: b"payload".to_vec(),
            });
            let resolver = fs_resolver(fetcher, &cache);
            let cache = cache.clone();
            let ssl = &ssl;
            async move {
                resolver
                    .resolve(cfg("https", &cache, "https://x", 3600, ssl))
                    .await
            }
        });
        let results = futures::future::join_all(resolves).await;

        assert!(results.iter().all(Result::is_ok), "all resolves succeed");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "concurrent stale resolves must share a single download"
        );
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            b"payload",
            "the shared fetch was persisted to the cache"
        );
    }

    #[tokio::test]
    async fn waiters_fail_fast_after_concurrent_failure() {
        // Two concurrent resolves against a down server: the leader eats the
        // full (slow) transport failure; the waiter, having waited through
        // that failure, must fail fast instead of serially repeating it —
        // one timeout for the group, not one per waiter.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let calls = Arc::new(AtomicU64::new(0));
        let ssl = SslVerify::Enabled;

        let resolves = (0..2).map(|_| {
            let fetcher = Box::new(SlowFailingFetcher {
                calls: calls.clone(),
            });
            let resolver = fs_resolver(fetcher, &cache);
            let cache = cache.clone();
            let ssl = &ssl;
            async move {
                resolver
                    .resolve(cfg("https", &cache, "https://x", 3600, ssl))
                    .await
            }
        });
        let results = futures::future::join_all(resolves).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the waiter must not repeat the failed download"
        );
        let errors: Vec<RefhostError> = results.into_iter().map(|r| r.unwrap_err()).collect();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, RefhostError::ResolveFailed)),
            "the leader surfaces the transport failure: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, RefhostError::RefreshJustFailed)),
            "the waiter adopts the failure without re-fetching: {errors:?}"
        );
    }

    #[tokio::test]
    async fn failed_refresh_is_retried_not_latched() {
        // A failed download must propagate to its caller and leave the next
        // caller free to retry (no poisoned/latched single-flight state).
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let calls = Arc::new(AtomicU64::new(0));
        let ssl = SslVerify::Enabled;

        let first = fs_resolver(
            Box::new(FailOnceFetcher {
                calls: calls.clone(),
            }),
            &cache,
        );
        let second = fs_resolver(
            Box::new(FailOnceFetcher {
                calls: calls.clone(),
            }),
            &cache,
        );

        first
            .resolve(cfg("https", &cache, "https://x", 3600, &ssl))
            .await
            .unwrap_err();
        second
            .resolve(cfg("https", &cache, "https://x", 3600, &ssl))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "second caller re-fetched");
    }

    fn refresh_probe(clock: u64, stat: ScriptedStat) -> HttpsResolver {
        https_resolver(
            clock,
            stat,
            RecordingFetcher::new(b""),
            RecordingWriter::default(),
            RecordingBuilder::new(),
            "/tmp/refhosts.yml",
        )
    }

    #[test]
    fn is_refresh_needed_missing_file() {
        let r = refresh_probe(1_000_000, ScriptedStat::NotFound);
        assert!(r.is_refresh_needed(3600).unwrap());
    }

    #[test]
    fn is_refresh_needed_other_error_raises() {
        let r = refresh_probe(1_000_000, ScriptedStat::Denied);
        let err = r.is_refresh_needed(3600).unwrap_err();
        assert!(matches!(err, RefhostError::Io { .. }));
    }

    #[test]
    fn is_refresh_needed_fresh_cache() {
        // delta = 1; expiration = 3600 → no refresh.
        let r = refresh_probe(1_000_000, ScriptedStat::Mtime(999_999));
        assert!(!r.is_refresh_needed(3600).unwrap());
    }

    #[test]
    fn is_refresh_needed_stale_cache() {
        let r = refresh_probe(1_000_000, ScriptedStat::Mtime(0));
        assert!(r.is_refresh_needed(3600).unwrap());
    }

    // --- HttpsResolver: refresh / refresh_if_needed ------------------------

    #[tokio::test]
    async fn refresh_writes_url_payload() {
        let fetcher = RecordingFetcher::new(b"yaml-bytes");
        let writer = RecordingWriter::default();
        let fcalls = fetcher.calls.clone();
        let writes = writer.writes.clone();
        let r = https_resolver(
            1,
            ScriptedStat::Mtime(0),
            fetcher,
            writer,
            RecordingBuilder::new(),
            "/dst",
        );
        r.refresh("https://x/refhosts.yml", VerifyPolicy::Default(true))
            .await
            .unwrap();
        assert_eq!(
            *fcalls.lock().unwrap(),
            vec![("https://x/refhosts.yml".to_string(), true)]
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(b"yaml-bytes".to_vec(), PathBuf::from("/dst"))]
        );
    }

    #[tokio::test]
    async fn refresh_if_needed_skips_when_fresh() {
        let fetcher = RecordingFetcher::new(b"");
        let fcalls = fetcher.calls.clone();
        let r = https_resolver(
            1_000_000,
            ScriptedStat::Mtime(999_999),
            fetcher,
            RecordingWriter::default(),
            RecordingBuilder::new(),
            "/x",
        );
        let ssl = SslVerify::Enabled;
        r.refresh_if_needed(cfg("https", Path::new("/x"), "https://e/r.yml", 3600, &ssl))
            .await
            .unwrap();
        assert!(fcalls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_if_needed_refreshes_when_stale() {
        let fetcher = RecordingFetcher::new(b"payload");
        let writer = RecordingWriter::default();
        let fcalls = fetcher.calls.clone();
        let writes = writer.writes.clone();
        let r = https_resolver(
            1_000_000,
            ScriptedStat::Mtime(0),
            fetcher,
            writer,
            RecordingBuilder::new(),
            "/x",
        );
        // ssl_verify unset (Enabled) → per-site default True flows to the opener.
        let ssl = SslVerify::Enabled;
        r.refresh_if_needed(cfg(
            "https",
            Path::new("/x"),
            "https://example.invalid/refhosts.yml",
            3600,
            &ssl,
        ))
        .await
        .unwrap();
        assert_eq!(
            *fcalls.lock().unwrap(),
            vec![("https://example.invalid/refhosts.yml".to_string(), true)]
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(b"payload".to_vec(), PathBuf::from("/x"))]
        );
    }

    #[tokio::test]
    async fn refresh_if_needed_honors_ssl_verify_override() {
        let fetcher = RecordingFetcher::new(b"payload");
        let fcalls = fetcher.calls.clone();
        let r = https_resolver(
            1_000_000,
            ScriptedStat::Mtime(0),
            fetcher,
            RecordingWriter::default(),
            RecordingBuilder::new(),
            "/x",
        );
        let ssl = SslVerify::Disabled;
        r.refresh_if_needed(cfg(
            "https",
            Path::new("/x"),
            "https://example.invalid/refhosts.yml",
            3600,
            &ssl,
        ))
        .await
        .unwrap();
        assert_eq!(
            *fcalls.lock().unwrap(),
            vec![("https://example.invalid/refhosts.yml".to_string(), false)]
        );
    }

    // --- RefhostsFactory ---------------------------------------------------

    #[tokio::test]
    async fn factory_returns_first_successful_resolver() {
        let (path_r, calls) = MockResolver::ok();
        let factory = RefhostsFactory::new(vec![("path".to_string(), Box::new(path_r))]);
        let ssl = SslVerify::Enabled;
        factory
            .resolve(cfg("path", Path::new("/x"), "https://x", 3600, &ssl))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn factory_falls_back_to_next_resolver_on_failure() {
        let (failing, fcalls) = MockResolver::failing();
        let (working, wcalls) = MockResolver::ok();
        let factory = RefhostsFactory::new(vec![
            ("https".to_string(), Box::new(failing)),
            ("path".to_string(), Box::new(working)),
        ]);
        let ssl = SslVerify::Enabled;
        factory
            .resolve(cfg("https,path", Path::new("/x"), "https://x", 3600, &ssl))
            .await
            .unwrap();
        assert_eq!(fcalls.load(Ordering::SeqCst), 1);
        assert_eq!(wcalls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn factory_raises_when_all_resolvers_fail() {
        let (f1, _) = MockResolver::failing();
        let (f2, _) = MockResolver::failing();
        let factory = RefhostsFactory::new(vec![
            ("https".to_string(), Box::new(f1)),
            ("path".to_string(), Box::new(f2)),
        ]);
        let ssl = SslVerify::Enabled;
        let err = factory
            .resolve(cfg("https,path", Path::new("/x"), "https://x", 3600, &ssl))
            .await
            .unwrap_err();
        assert!(matches!(err, RefhostError::ResolveFailed));
    }

    #[tokio::test]
    async fn factory_skips_unknown_resolver_and_continues() {
        let (working, calls) = MockResolver::ok();
        let factory = RefhostsFactory::new(vec![("path".to_string(), Box::new(working))]);
        let ssl = SslVerify::Enabled;
        factory
            .resolve(cfg(
                "nonexistent,path",
                Path::new("/x"),
                "https://x",
                3600,
                &ssl,
            ))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // --- production seams (smoke) ------------------------------------------

    #[test]
    fn atomic_writer_roundtrips_via_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("refhosts.yml");
        AtomicFileWriter.write(b"data", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
    }

    #[test]
    fn fs_stat_reports_not_found_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yml");
        assert!(matches!(FsStat.stat(&missing), StatResult::NotFound));
        let present = dir.path().join("there.yml");
        std::fs::write(&present, b"x").unwrap();
        assert!(matches!(FsStat.stat(&present), StatResult::Mtime(_)));
    }

    #[test]
    fn path_refhosts_builder_reads_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refhosts.yml");
        std::fs::write(&path, FIXTURE_HOSTS).unwrap();
        let store = PathRefhostsBuilder.build(&path).unwrap();
        assert_eq!(store.hosts().len(), 1);
    }

    #[test]
    fn system_clock_is_monotonicish() {
        // Smoke: the system clock returns a plausibly-recent epoch second.
        assert!(SystemClock.now_unix() > 1_600_000_000);
    }
}
