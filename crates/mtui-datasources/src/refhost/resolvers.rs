//! Strategies for producing a [`Refhosts`] from a YAML source.
//!
//! The [`RefhostsFactory`] tries the named resolvers in the order given by
//! `config.refhosts_resolvers` (a comma-separated string), returning the first
//! success and logging every failure along the way:
//! - `path` builds a [`Refhosts`] from a local file (`config.refhosts_path`);
//! - `https` downloads it from an HTTPS URL, caching the payload on disk and
//!   only re-fetching once the cache is older than
//!   `config.refhosts_https_expiration`.
//!
//! # Testability seams
//! The clock and the URL opener are injected as small object-safe traits
//! ([`Clock`], [`Fetcher`]) so the cache-refresh logic can be driven offline;
//! production wires [`SystemClock`] and [`HttpFetcher`]. The cache itself is a
//! real file, so tests point it at a temp directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use tracing::warn;

use super::store::Refhosts;
use crate::clock::unix_now;
use crate::error::RefhostError;
use crate::http::{HttpClient, VerifyPolicy, sanitize_url};

/// The subset of configuration a resolver needs.
///
/// A borrowed view rather than the whole `mtui_config::Config`, so resolvers
/// stay decoupled from the full config surface and are trivial to construct in
/// tests.
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
}

// ---------------------------------------------------------------------------
// Seams: clock / URL fetch
// ---------------------------------------------------------------------------

/// A source of the current wall-clock time.
pub trait Clock: Send + Sync {
    /// Current time as seconds since the Unix epoch.
    fn now_unix(&self) -> u64;
}

/// Fetches the raw bytes of a URL.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// GET `uri` and return the body bytes.
    ///
    /// # Errors
    /// Returns [`RefhostError`] on any transport failure or non-2xx status.
    async fn fetch(&self, uri: &str) -> Result<Vec<u8>, RefhostError>;
}

/// [`Clock`] backed by the real system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        u64::try_from(unix_now()).unwrap_or(0)
    }
}

/// [`Fetcher`] backed by the shared [`HttpClient`].
///
/// The client's TLS posture is fixed at build time (see [`HttpClient::new`]) by
/// `HttpFetcher::new` from the effective [`VerifyPolicy`].
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
    fn new(verify: VerifyPolicy) -> Result<Self, RefhostError> {
        let client = HttpClient::new(verify).map_err(|e| RefhostError::Io {
            path: "<http client>".to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, uri: &str) -> Result<Vec<u8>, RefhostError> {
        self.client
            .get_bytes(uri)
            .await
            .map_err(|e| RefhostError::Io {
                path: sanitize_url(uri),
                source: std::io::Error::other(e.to_string()),
            })
    }
}

// ---------------------------------------------------------------------------
// The https strategy
// ---------------------------------------------------------------------------

/// The result of stat-ing the cache file.
enum StatResult {
    /// The cache file does not exist (`ENOENT`) → force a refresh.
    NotFound,
    /// The cache file exists with this mtime (seconds since the Unix epoch).
    Mtime(u64),
    /// Stat-ing the file failed for a reason other than "not found"; the error
    /// propagates instead of silently forcing a refresh.
    Err(std::io::Error),
}

fn stat_cache(path: &Path) -> StatResult {
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

/// State shared by every refresh of one cache path, guarded by its
/// [`refresh_lock`].
///
/// `failed_at` is a monotonic [`std::time::Instant`], deliberately not the
/// scriptable [`Clock`] seam: it answers "did that failure happen while *I* was
/// waiting", an ordering question, not a cache-age computation.
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
/// by cache path so unrelated caches (in practice per-test temp paths —
/// production configures exactly one `config.refhosts_path`) never serialise
/// each other; entries are never removed, which is fine at that cardinality.
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
struct HttpsResolver {
    clock: Box<dyn Clock>,
    fetcher: Box<dyn Fetcher>,
    cache_path: PathBuf,
}

impl HttpsResolver {
    /// Whether the cache should be re-downloaded.
    ///
    /// A missing cache always refreshes; otherwise refresh iff it is older than
    /// `expiration` seconds.
    ///
    /// # Errors
    /// Returns [`RefhostError::Io`] if stat-ing the cache fails for a reason
    /// other than "not found".
    fn is_refresh_needed(&self, expiration: u64) -> Result<bool, RefhostError> {
        match stat_cache(&self.cache_path) {
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

    /// Download `uri` and return the bytes, best-effort
    /// persisting them to the cache path.
    ///
    /// A mirror-write failure (a read-only, package-managed `refhosts.path`) is
    /// logged but does not fail the download — the caller still gets the fetched
    /// bytes to build from in memory.
    async fn refresh(&self, uri: &str) -> Result<Vec<u8>, RefhostError> {
        let bytes = self.fetcher.fetch(uri).await?;
        if let Err(e) = mtui_config::atomic::write(&bytes, &self.cache_path) {
            warn!(
                "failed to write refhosts mirror to {}: {e}",
                self.cache_path.display()
            );
        }
        Ok(bytes)
    }

    /// Refresh the cache if it is missing or stale.
    ///
    /// Concurrent resolves **single-flight** the download: the first caller to
    /// see a stale cache takes the per-cache-path [`refresh_lock`] and fetches,
    /// and callers arriving meanwhile wait on it, then *re-check* freshness —
    /// the leader's write bumped the mtime, so they build from the fresh cache
    /// instead of stampeding the server. That holds on failure too: a waiter
    /// that waited through a *failed* download fails fast
    /// ([`RefhostError::RefreshJustFailed`]) rather than repeating an attempt
    /// that would almost certainly fail again, so a down server costs the group
    /// one transport timeout, not one per waiter. A caller that was not yet
    /// waiting when the failure happened retries normally.
    ///
    /// Returns `Some(bytes)` when this call performed the download — so
    /// [`resolve`](Self::resolve) builds from those rather than re-reading a
    /// cache that may not have been written — and `None` when no download
    /// happened.
    async fn refresh_if_needed(
        &self,
        config: ResolveConfig<'_>,
    ) -> Result<Option<Vec<u8>>, RefhostError> {
        // Unlocked fast path: the common case (fresh cache) takes no lock.
        if !self.is_refresh_needed(config.refhosts_https_expiration)? {
            return Ok(None);
        }
        let wait_started = std::time::Instant::now();
        let lock = refresh_lock(&self.cache_path);
        let mut slot = lock.lock().await;
        // Re-check under the lock: the leader may have refreshed meanwhile.
        if self.is_refresh_needed(config.refhosts_https_expiration)? {
            // A download that failed while this task waited is adopted rather
            // than serially re-fetched.
            if slot.failed_at.is_some_and(|failed| failed > wait_started) {
                return Err(RefhostError::RefreshJustFailed);
            }
            return match self.refresh(config.refhosts_https_uri).await {
                Ok(bytes) => {
                    slot.failed_at = None;
                    Ok(Some(bytes))
                }
                Err(e) => {
                    slot.failed_at = Some(std::time::Instant::now());
                    Err(e)
                }
            };
        }
        Ok(None)
    }

    async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
        match self.refresh_if_needed(config).await? {
            Some(bytes) => Refhosts::from_bytes(&bytes),
            None => Refhosts::from_path(&self.cache_path),
        }
    }
}

/// Best-effort check whether `refhosts_path`'s parent directory looks
/// writable, so a caller can forewarn that the https mirror write is likely
/// to fail — e.g. a read-only, package-managed `refhosts.path`.
///
/// `mtui_config::atomic::write` creates its temp file in that same directory,
/// so the directory — not the target file — is what must be writable. A missing
/// directory counts as "not writable" even though the write would try to create
/// it, since a directory missing at this level (e.g. `/usr/share/qam-metadata`)
/// is typically root-owned and uncreatable anyway. A plain permission-bit probe
/// (no ACLs/mounts), so it gates only a warning.
fn refhosts_mirror_dir_writable(refhosts_path: &Path) -> bool {
    let Some(parent) = refhosts_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return true;
    };
    std::fs::metadata(parent).is_ok_and(|meta| !meta.permissions().readonly())
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Builds a [`Refhosts`] from the configured resolvers.
///
/// Each name in `config.refhosts_resolvers` is tried in turn; an unknown name
/// or a resolver error is logged and the next is tried, and an exhausted list
/// is [`RefhostError::ResolveFailed`].
pub struct RefhostsFactory {
    https: HttpsResolver,
}

impl RefhostsFactory {
    /// The production factory, caching the https download at `cache_path`.
    ///
    /// # Errors
    /// Returns [`RefhostError`] if the HTTPS fetcher's client cannot be built
    /// (e.g. an unreadable configured CA bundle).
    pub fn production(cache_path: PathBuf, verify: VerifyPolicy) -> Result<Self, RefhostError> {
        Ok(Self {
            https: HttpsResolver {
                clock: Box::new(SystemClock),
                fetcher: Box::new(HttpFetcher::new(verify)?),
                cache_path,
            },
        })
    }

    /// Try each configured resolver in order; return the first success.
    ///
    /// # Errors
    /// Returns [`RefhostError::ResolveFailed`] if no resolver produces a usable
    /// database.
    pub async fn resolve(&self, config: ResolveConfig<'_>) -> Result<Refhosts, RefhostError> {
        for name in config.refhosts_resolvers.split(',').map(str::trim) {
            let result = match name {
                "https" => {
                    if !refhosts_mirror_dir_writable(config.refhosts_path) {
                        warn!(
                            "refhosts mirror directory for {} is missing or not writable; \
                             the https download will not be cached on disk",
                            config.refhosts_path.display()
                        );
                    }
                    self.https.resolve(config).await
                }
                "path" => Refhosts::from_path(config.refhosts_path),
                _ => {
                    warn!("Refhosts: invalid resolver: {name}");
                    continue;
                }
            };
            match result {
                Ok(refhosts) => return Ok(refhosts),
                Err(e) => warn!("Refhosts: resolver {name} failed: {e}"),
            }
        }
        Err(RefhostError::ResolveFailed)
    }
}

#[cfg(test)]
mod tests {
    //! The cache is a real file in a temp directory; only the clock and the
    //! fetch are scripted.

    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

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
    ) -> ResolveConfig<'a> {
        ResolveConfig {
            refhosts_resolvers: resolvers,
            refhosts_path: path,
            refhosts_https_uri: uri,
            refhosts_https_expiration: expiration,
        }
    }

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_unix(&self) -> u64 {
            self.0
        }
    }

    /// Serves fixed bytes (or a failure), recording each URI and optionally
    /// dwelling like a slow transport.
    struct ScriptedFetcher {
        calls: Arc<AtomicU64>,
        uris: Arc<StdMutex<Vec<String>>>,
        payload: Option<Vec<u8>>,
        dwell: std::time::Duration,
    }
    impl ScriptedFetcher {
        fn serving(payload: &[u8]) -> Self {
            Self {
                calls: Arc::default(),
                uris: Arc::default(),
                payload: Some(payload.to_vec()),
                dwell: std::time::Duration::ZERO,
            }
        }
        fn failing() -> Self {
            Self {
                payload: None,
                ..Self::serving(b"")
            }
        }
        fn slow(mut self) -> Self {
            self.dwell = std::time::Duration::from_millis(50);
            self
        }
        /// A fetcher sharing this one's counters, for several resolver
        /// instances over one cache.
        fn sharing(&self) -> Self {
            Self {
                calls: self.calls.clone(),
                uris: self.uris.clone(),
                payload: self.payload.clone(),
                dwell: self.dwell,
            }
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn fetch(&self, uri: &str) -> Result<Vec<u8>, RefhostError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.uris.lock().unwrap().push(uri.to_owned());
            tokio::time::sleep(self.dwell).await;
            self.payload.clone().ok_or(RefhostError::ResolveFailed)
        }
    }

    fn resolver(clock: Box<dyn Clock>, fetcher: ScriptedFetcher, cache: &Path) -> HttpsResolver {
        HttpsResolver {
            clock,
            fetcher: Box::new(fetcher),
            cache_path: cache.to_path_buf(),
        }
    }

    /// A resolver on the real clock, as every production call site builds its
    /// own instance.
    fn live(fetcher: ScriptedFetcher, cache: &Path) -> HttpsResolver {
        resolver(Box::new(SystemClock), fetcher, cache)
    }

    /// A cache file whose age, as the resolver sees it, is `age` seconds.
    fn aged_cache(age: u64) -> (tempfile::TempDir, PathBuf, Box<dyn Clock>) {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        std::fs::write(&cache, FIXTURE_HOSTS).unwrap();
        let mtime = match stat_cache(&cache) {
            StatResult::Mtime(m) => m,
            _ => panic!("fresh file must have an mtime"),
        };
        (dir, cache, Box::new(FixedClock(mtime + age)))
    }

    #[tokio::test]
    async fn a_failed_download_does_not_echo_the_uri_userinfo() {
        let fetcher = HttpFetcher::new(VerifyPolicy::Default(true)).unwrap();
        let err = fetcher
            .fetch("http://bot:s3cret@127.0.0.1:1/refhosts.yml")
            .await
            .expect_err("nothing listens on port 1");
        let rendered = err.to_string();
        assert!(rendered.contains("127.0.0.1:1"), "{rendered}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }

    // --- HttpsResolver: resolve --------------------------------------------

    #[tokio::test]
    async fn a_fresh_cache_is_built_from_without_fetching() {
        let (_dir, cache, clock) = aged_cache(10);
        let fetcher = ScriptedFetcher::serving(b"not yaml");
        let calls = fetcher.calls.clone();
        let store = resolver(clock, fetcher, &cache)
            .resolve(cfg("https", Path::new("/x"), "https://x", 3600))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "fresh cache: no fetch");
        assert_eq!(store.hosts().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_cache_is_refetched_persisted_and_built_from_the_download() {
        let (_dir, cache, clock) = aged_cache(7200);
        std::fs::write(&cache, "stale: [").unwrap();
        let fetcher = ScriptedFetcher::serving(FIXTURE_HOSTS.as_bytes());
        let uris = fetcher.uris.clone();
        let store = resolver(clock, fetcher, &cache)
            .resolve(cfg(
                "https",
                Path::new("/x"),
                "https://x/refhosts.yml",
                3600,
            ))
            .await
            .unwrap();
        assert_eq!(*uris.lock().unwrap(), vec!["https://x/refhosts.yml"]);
        assert_eq!(std::fs::read(&cache).unwrap(), FIXTURE_HOSTS.as_bytes());
        assert_eq!(store.hosts().len(), 1);
    }

    /// A mirror that cannot be written must not fail the resolve: the store is
    /// built from the downloaded bytes, never re-read from the unwritten path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mirror_write_failure_is_non_fatal() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let cache = dir.path().join("refhosts.yml");
        let result = resolver(
            Box::new(FixedClock(1_000_000)),
            ScriptedFetcher::serving(FIXTURE_HOSTS.as_bytes()),
            &cache,
        )
        .resolve(cfg(
            "https",
            Path::new("/x"),
            "https://x/refhosts.yml",
            3600,
        ))
        .await;
        let written = cache.exists();
        // Restore permissions so `tempdir` can clean up on drop.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = result.expect("a mirror-write failure must not fail the resolve");
        assert_eq!(store.hosts().len(), 1);
        assert!(!written, "premise: the mirror could not be written");
    }

    // --- HttpsResolver: is_refresh_needed ----------------------------------

    #[test]
    fn is_refresh_needed_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(
            Box::new(FixedClock(1_000_000)),
            ScriptedFetcher::failing(),
            &dir.path().join("absent.yml"),
        );
        assert!(r.is_refresh_needed(3600).unwrap());
    }

    #[test]
    fn is_refresh_needed_other_error_raises() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let r = resolver(
            Box::new(FixedClock(1_000_000)),
            ScriptedFetcher::failing(),
            &blocker.join("refhosts.yml"),
        );
        let err = r.is_refresh_needed(3600).unwrap_err();
        assert!(matches!(err, RefhostError::Io { .. }));
    }

    #[test]
    fn is_refresh_needed_fresh_cache() {
        let (_dir, cache, clock) = aged_cache(10);
        let r = resolver(clock, ScriptedFetcher::failing(), &cache);
        assert!(!r.is_refresh_needed(3600).unwrap());
    }

    #[test]
    fn is_refresh_needed_stale_cache() {
        let (_dir, cache, clock) = aged_cache(3601);
        let r = resolver(clock, ScriptedFetcher::failing(), &cache);
        assert!(r.is_refresh_needed(3600).unwrap());
    }

    #[test]
    fn is_refresh_needed_at_the_expiration_boundary_is_fresh() {
        let (_dir, cache, clock) = aged_cache(3600);
        let r = resolver(clock, ScriptedFetcher::failing(), &cache);
        assert!(!r.is_refresh_needed(3600).unwrap());
    }

    // --- HttpsResolver: refresh single-flight ------------------------------

    #[tokio::test]
    async fn concurrent_stale_resolves_share_one_fetch() {
        // Four resolver *instances* over one missing cache file: the shape of
        // four concurrent sessions each constructing their own factory.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let template = ScriptedFetcher::serving(FIXTURE_HOSTS.as_bytes()).slow();

        let resolves = (0..4).map(|_| {
            let r = live(template.sharing(), &cache);
            let cache = cache.clone();
            async move { r.resolve(cfg("https", &cache, "https://x", 3600)).await }
        });
        let results = futures::future::join_all(resolves).await;

        assert!(results.iter().all(Result::is_ok), "all resolves succeed");
        assert_eq!(
            template.calls(),
            1,
            "concurrent stale resolves must share a single download"
        );
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            FIXTURE_HOSTS.as_bytes(),
            "the shared fetch was persisted to the cache"
        );
    }

    #[tokio::test]
    async fn waiters_fail_fast_after_concurrent_failure() {
        // Against a down server the leader eats the full (slow) transport
        // failure; the waiter must fail fast rather than repeat it — one
        // timeout for the group, not one per waiter.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let template = ScriptedFetcher::failing().slow();

        let resolves = (0..2).map(|_| {
            let r = live(template.sharing(), &cache);
            let cache = cache.clone();
            async move { r.resolve(cfg("https", &cache, "https://x", 3600)).await }
        });
        let results = futures::future::join_all(resolves).await;

        assert_eq!(
            template.calls(),
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
        // No poisoned/latched single-flight state: the next caller may retry.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("refhosts.yml");
        let failing = ScriptedFetcher::failing();
        let serving = ScriptedFetcher::serving(FIXTURE_HOSTS.as_bytes());
        let (failed_calls, served_calls) = (failing.calls.clone(), serving.calls.clone());

        live(failing, &cache)
            .resolve(cfg("https", &cache, "https://x", 3600))
            .await
            .unwrap_err();
        live(serving, &cache)
            .resolve(cfg("https", &cache, "https://x", 3600))
            .await
            .unwrap();
        assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            served_calls.load(Ordering::SeqCst),
            1,
            "second caller re-fetched"
        );
    }

    // --- refhosts_mirror_dir_writable ---------------------------------------

    #[test]
    fn mirror_dir_writable_for_writable_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refhosts.yml");
        assert!(refhosts_mirror_dir_writable(&path));
    }

    #[test]
    fn mirror_dir_not_writable_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent-subdir").join("refhosts.yml");
        assert!(!refhosts_mirror_dir_writable(&path));
    }

    #[cfg(unix)]
    #[test]
    fn mirror_dir_not_writable_for_readonly_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let path = dir.path().join("refhosts.yml");
        let writable = refhosts_mirror_dir_writable(&path);
        // Restore permissions so `tempdir` can clean up on drop.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!writable);
    }

    // --- RefhostsFactory ---------------------------------------------------

    fn factory(fetcher: ScriptedFetcher, cache: &Path) -> RefhostsFactory {
        RefhostsFactory {
            https: live(fetcher, cache),
        }
    }

    fn fixture_file(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("local.yml");
        std::fs::write(&path, FIXTURE_HOSTS).unwrap();
        path
    }

    #[tokio::test]
    async fn path_resolver_reads_the_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let local = fixture_file(&dir);
        let fetcher = ScriptedFetcher::failing();
        let calls = fetcher.calls.clone();
        let store = factory(fetcher, &dir.path().join("cache.yml"))
            .resolve(cfg("path", &local, "https://x", 3600))
            .await
            .unwrap();
        assert_eq!(store.hosts().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "path never fetches");
    }

    #[tokio::test]
    async fn factory_returns_first_successful_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let local = fixture_file(&dir);
        let fetcher = ScriptedFetcher::serving(FIXTURE_HOSTS.as_bytes());
        let calls = fetcher.calls.clone();
        factory(fetcher, &dir.path().join("cache.yml"))
            .resolve(cfg("https,path", &local, "https://x", 3600))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "https answered first");
    }

    #[tokio::test]
    async fn factory_falls_back_to_next_resolver_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let local = fixture_file(&dir);
        let fetcher = ScriptedFetcher::failing();
        let calls = fetcher.calls.clone();
        let store = factory(fetcher, &dir.path().join("cache.yml"))
            .resolve(cfg("https,path", &local, "https://x", 3600))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.hosts().len(), 1);
    }

    #[tokio::test]
    async fn factory_raises_when_all_resolvers_fail() {
        let dir = tempfile::tempdir().unwrap();
        let err = factory(ScriptedFetcher::failing(), &dir.path().join("cache.yml"))
            .resolve(cfg(
                "https,path",
                &dir.path().join("missing.yml"),
                "https://x",
                3600,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, RefhostError::ResolveFailed));
    }

    #[tokio::test]
    async fn factory_skips_unknown_resolver_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let local = fixture_file(&dir);
        factory(ScriptedFetcher::failing(), &dir.path().join("cache.yml"))
            .resolve(cfg("nonexistent,path", &local, "https://x", 3600))
            .await
            .unwrap();
    }

    // --- production seams (smoke) ------------------------------------------

    #[test]
    fn stat_cache_reports_not_found_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yml");
        assert!(matches!(stat_cache(&missing), StatResult::NotFound));
        let present = dir.path().join("there.yml");
        std::fs::write(&present, b"x").unwrap();
        assert!(matches!(stat_cache(&present), StatResult::Mtime(_)));
    }

    #[test]
    fn system_clock_is_monotonicish() {
        assert!(SystemClock.now_unix() > 1_600_000_000);
    }
}
