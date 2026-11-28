//! Read-only check of a published rpm-md update repository.
//!
//! [`check_repository`] fetches `repodata/repomd.xml`, follows its `primary`
//! entry and compares the package entries there with the binaries the report
//! lists for that repository. Every failure to fetch or read the metadata is
//! [`RepoCheck::NoAnswer`], never [`Empty`](RepoCheck::Empty) or
//! [`Missing`](RepoCheck::Missing): those two are claims about what the
//! repository serves, and only fully parsed metadata may make them.
//!
//! Requests go to the repository URL and paths below it only: redirects are
//! followed by hand, and a `location` or redirect that leaves the repository's
//! origin or path is refused rather than fetched.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use reqwest::Url;
use thiserror::Error;

use crate::error::HttpError;
use crate::http::{HttpClient, read_body_capped};

/// `(name, version-release.arch)` pairs: the binaries listed for a repository.
pub type Listed = BTreeSet<(String, String)>;

/// Redirect hops followed for one document.
const MAX_REDIRECTS: usize = 5;
/// Longest package name, arch, version or release accepted.
const MAX_FIELD: usize = 256;
/// Largest zstd window the decoder allocates.
const MAX_ZSTD_WINDOW: u64 = 32 * 1024 * 1024;
/// Deepest element nesting accepted; rpm-md needs about five, and the
/// parser keeps every open tag.
const MAX_DEPTH: usize = 16;
/// `BufReader`'s read-ahead, which the per-event bound must allow for.
const READ_AHEAD: usize = 8 * 1024;

/// The verdict of a [`RepoCheck`], as printed and serialised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStatus {
    /// See [`RepoCheck::Ok`].
    Ok,
    /// See [`RepoCheck::Missing`].
    Missing,
    /// See [`RepoCheck::Empty`].
    Empty,
    /// See [`RepoCheck::NotPublished`].
    NotPublished,
    /// See [`RepoCheck::NoAnswer`].
    NoAnswer,
}

impl RepoStatus {
    /// The status word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Empty => "empty",
            Self::NotPublished => "not published",
            Self::NoAnswer => "no answer",
        }
    }
}

/// What [`check_repository`] found; its `Display` is the one-line reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoCheck {
    /// Published with `packages` > 0 and all `listed` binaries present;
    /// `listed == 0` means no binaries were listed for this repository.
    Ok {
        /// Package entries in the primary metadata.
        packages: usize,
        /// Distinct listed binaries, all present.
        listed: usize,
    },
    /// Published with `packages` > 0, but the `missing` binary names are not
    /// at their listed `version-release.arch`.
    Missing {
        /// Package entries in the primary metadata.
        packages: usize,
        /// The listed names not published at their version, sorted.
        missing: Vec<String>,
    },
    /// Published with zero packages.
    Empty,
    /// `repodata/repomd.xml` answered 404.
    NotPublished,
    /// The metadata could not be fetched or fully read.
    NoAnswer(NoAnswer),
}

impl RepoCheck {
    /// The verdict.
    #[must_use]
    pub fn status(&self) -> RepoStatus {
        match self {
            Self::Ok { .. } => RepoStatus::Ok,
            Self::Missing { .. } => RepoStatus::Missing,
            Self::Empty => RepoStatus::Empty,
            Self::NotPublished => RepoStatus::NotPublished,
            Self::NoAnswer(_) => RepoStatus::NoAnswer,
        }
    }

    /// Packages in the primary metadata, when it was read.
    #[must_use]
    pub fn packages(&self) -> Option<usize> {
        match self {
            Self::Ok { packages, .. } | Self::Missing { packages, .. } => Some(*packages),
            Self::Empty => Some(0),
            Self::NotPublished | Self::NoAnswer(_) => None,
        }
    }

    /// The listed binary names not published, sorted.
    #[must_use]
    pub fn missing(&self) -> &[String] {
        match self {
            Self::Missing { missing, .. } => missing,
            _ => &[],
        }
    }
}

impl fmt::Display for RepoCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok {
                packages,
                listed: 0,
            } => write!(
                f,
                "{packages} packages; no install target lists binaries for this repository, \
                 so only checked that it is published and non-empty"
            ),
            Self::Ok { packages, listed } => {
                write!(
                    f,
                    "{packages} packages, all {listed} listed binaries present"
                )
            }
            Self::Missing { packages, missing } => write!(
                f,
                "{packages} packages; not published at the listed version-release.arch: {}",
                missing.join(", ")
            ),
            Self::Empty => f.write_str("published with 0 packages"),
            Self::NotPublished => f.write_str("repodata/repomd.xml answered 404"),
            Self::NoAnswer(why) => write!(f, "{why}"),
        }
    }
}

/// Which document a fetch was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Document {
    /// `repodata/repomd.xml`.
    Repomd,
    /// The primary metadata `repomd.xml` names.
    Primary,
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Repomd => "repodata/repomd.xml",
            Self::Primary => "the primary metadata",
        })
    }
}

/// Why a repository gave no answer. No variant carries a URL.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NoAnswer {
    /// The repository is not an absolute http(s) URL.
    #[error("the repository is not a valid http(s) URL")]
    InvalidUrl,
    /// The HTTP client could not be built.
    #[error("the HTTP client could not be built: {0}")]
    Client(String),
    /// A non-2xx answer (other than a 404 on `repomd.xml`).
    #[error("{0} answered HTTP {1}")]
    Status(Document, u16),
    /// The per-request budget ran out.
    #[error("{0} did not answer in time")]
    Timeout(Document),
    /// The whole check's budget ran out.
    #[error("the check's {0:?} budget ran out first")]
    OutOfTime(Duration),
    /// A transport failure; the detail is [`HttpError`]'s URL-free rendering.
    #[error("fetching {0} failed: {1}")]
    Transport(Document, String),
    /// The body exceeded its cap.
    #[error("{0} exceeds the {1}-byte limit")]
    TooLarge(Document, usize),
    /// A redirect left the repository, or named no usable location.
    #[error("{0} redirected outside the repository")]
    RedirectOutside(Document),
    /// More redirect hops than the check follows.
    #[error("{} redirected more than {} times", .0, MAX_REDIRECTS)]
    TooManyRedirects(Document),
    /// `repomd.xml` gave no usable primary location.
    #[error(transparent)]
    Repomd(#[from] RepomdError),
    /// The primary metadata could not be read.
    #[error(transparent)]
    Primary(#[from] PrimaryError),
}

/// Why `repomd.xml` gave no usable primary location.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RepomdError {
    /// Not complete, well-formed rpm-md repository metadata.
    #[error("repomd.xml is not readable rpm-md repository metadata")]
    Unreadable,
    /// No `<data type="primary">` with a `location`.
    #[error("repomd.xml lists no primary metadata")]
    NoPrimary,
    /// The primary `location` carries `xml:base`, which names another server.
    #[error("repomd.xml points the primary metadata at another location base")]
    ForeignBase,
    /// The primary `location` resolves outside the repository.
    #[error("repomd.xml points the primary metadata outside the repository")]
    Outside,
}

/// Why the primary metadata could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PrimaryError {
    /// Compressed with a format this build cannot decompress.
    #[error(
        "the primary metadata is {0}-compressed, which this build cannot decompress \
         (gzip, zstd and uncompressed only)"
    )]
    Unsupported(&'static str),
    /// Larger than the cap once decompressed.
    #[error("the primary metadata exceeds {0} bytes once decompressed")]
    TooLarge(usize),
    /// One XML event larger than the per-event cap.
    #[error("the primary metadata holds an element or text over {0} bytes")]
    EventTooLarge(usize),
    /// Not complete, well-formed primary metadata.
    #[error("the primary metadata is not readable rpm-md metadata")]
    Unreadable,
    /// The `packages` count disagrees with the entries listed.
    #[error("the primary metadata declares {declared} packages but lists {listed}")]
    CountMismatch {
        /// The `packages` attribute.
        declared: usize,
        /// The `<package>` entries read.
        listed: usize,
    },
    /// The blocking parse task failed.
    #[error("reading the primary metadata failed")]
    Aborted,
}

/// Bounds for one check.
#[derive(Debug, Clone)]
pub(crate) struct CheckLimits {
    /// Whole-request budget for each GET, body and redirects included.
    pub(crate) request_timeout: Duration,
    /// Largest `repomd.xml` accepted.
    pub(crate) max_repomd: usize,
    /// Largest primary metadata file accepted, as served.
    pub(crate) max_primary: usize,
    /// Largest primary metadata accepted once decompressed.
    pub(crate) max_unpacked: usize,
    /// Largest single XML event accepted in the primary metadata.
    pub(crate) max_event: usize,
}

impl Default for CheckLimits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(60),
            max_repomd: 1024 * 1024,
            max_primary: 64 * 1024 * 1024,
            max_unpacked: 512 * 1024 * 1024,
            max_event: 1024 * 1024,
        }
    }
}

/// Checks the rpm-md repository at `repository` against `listed`. An empty
/// `listed` checks only that the repository is published and non-empty.
///
/// The decompressing parse runs on a blocking thread; dropping this future
/// (a cancel) leaves that parse to finish on its own, bounded by the caps.
pub async fn check_repository(http: &HttpClient, repository: &str, listed: &Listed) -> RepoCheck {
    check_with(http, repository, listed, &CheckLimits::default()).await
}

pub(crate) async fn check_with(
    http: &HttpClient,
    repository: &str,
    listed: &Listed,
    limits: &CheckLimits,
) -> RepoCheck {
    match probe(http, repository, listed, limits).await {
        Ok(check) => check,
        Err(why) => RepoCheck::NoAnswer(why),
    }
}

async fn probe(
    http: &HttpClient,
    repository: &str,
    listed: &Listed,
    limits: &CheckLimits,
) -> Result<RepoCheck, NoAnswer> {
    let base = repository_base(repository).ok_or(NoAnswer::InvalidUrl)?;
    let client = http
        .unredirected()
        .map_err(|e| NoAnswer::Client(e.to_string()))?;
    let repomd_url = base
        .join("repodata/repomd.xml")
        .map_err(|_| NoAnswer::InvalidUrl)?;

    let repomd = match fetch(client, &base, repomd_url, Document::Repomd, limits).await {
        Err(NoAnswer::Status(Document::Repomd, 404)) => return Ok(RepoCheck::NotPublished),
        other => other?,
    };
    let href = parse_repomd(&repomd)?;
    let primary_url = base
        .join(&href)
        .ok()
        .and_then(|u| within(&base, &u))
        .ok_or(RepomdError::Outside)?;
    let primary = fetch(client, &base, primary_url, Document::Primary, limits).await?;

    let wanted = listed.clone();
    let (cap, max_event) = (limits.max_unpacked, limits.max_event);
    let scan =
        tokio::task::spawn_blocking(move || parse_primary(&primary, &wanted, cap, max_event))
            .await
            .unwrap_or(Err(PrimaryError::Aborted))?;
    Ok(verdict(&scan, listed))
}

/// The verdict for a fully read primary metadata.
fn verdict(scan: &PrimaryScan, listed: &Listed) -> RepoCheck {
    if scan.count == 0 {
        return RepoCheck::Empty;
    }
    let missing: BTreeSet<&String> = listed
        .iter()
        .filter(|pair| !scan.present.contains(*pair))
        .map(|(name, _)| name)
        .collect();
    if missing.is_empty() {
        RepoCheck::Ok {
            packages: scan.count,
            listed: listed.len(),
        }
    } else {
        RepoCheck::Missing {
            packages: scan.count,
            missing: missing.into_iter().cloned().collect(),
        }
    }
}

/// `repository` as a directory URL (trailing `/`), or `None` when it is not
/// an absolute http(s) URL.
fn repository_base(repository: &str) -> Option<Url> {
    let mut url = Url::parse(repository).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    plain_path(url.path()).then_some(url)
}

/// Whether `path` carries no percent-encoded `/`, `\` or `.`, which a server
/// may decode into a traversal the normalised path does not show.
fn plain_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !["%2f", "%5c", "%2e"].iter().any(|enc| lower.contains(enc))
}

/// `url` when it stays inside the repository at `base`: same scheme, host and
/// port, a path strictly below `base`'s, and no userinfo other than `base`'s.
/// The result carries `base`'s userinfo, so credentials go to this origin only.
fn within(base: &Url, url: &Url) -> Option<Url> {
    let same_origin = url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default();
    let own_userinfo = (url.username().is_empty() && url.password().is_none())
        || (url.username() == base.username() && url.password() == base.password());
    let below = url.path().starts_with(base.path()) && url.path() != base.path();
    if !(same_origin && own_userinfo && below && plain_path(url.path())) {
        return None;
    }
    let mut out = url.clone();
    out.set_username(base.username()).ok()?;
    out.set_password(base.password()).ok()?;
    out.set_fragment(None);
    Some(out)
}

/// GETs `url`, following up to [`MAX_REDIRECTS`] redirects that stay
/// [`within`] the repository, with the per-request budget and the body cap for
/// `what`.
async fn fetch(
    client: &reqwest::Client,
    base: &Url,
    url: Url,
    what: Document,
    limits: &CheckLimits,
) -> Result<Vec<u8>, NoAnswer> {
    let max = match what {
        Document::Repomd => limits.max_repomd,
        Document::Primary => limits.max_primary,
    };
    let transport = |e: HttpError| match e {
        HttpError::BodyTooLarge { limit, .. } => NoAnswer::TooLarge(what, limit),
        other => NoAnswer::Transport(what, other.to_string()),
    };
    let request = async {
        let mut url = url;
        for _ in 0..=MAX_REDIRECTS {
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| transport(e.into()))?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response.headers().get(reqwest::header::LOCATION)
            {
                url = location
                    .to_str()
                    .ok()
                    .and_then(|l| url.join(l).ok())
                    .and_then(|next| within(base, &next))
                    .ok_or(NoAnswer::RedirectOutside(what))?;
                continue;
            }
            if !status.is_success() {
                return Err(NoAnswer::Status(what, status.as_u16()));
            }
            return read_body_capped(response, max).await.map_err(transport);
        }
        Err(NoAnswer::TooManyRedirects(what))
    };
    tokio::time::timeout(limits.request_timeout, request)
        .await
        .unwrap_or(Err(NoAnswer::Timeout(what)))
}

/// A malformed attribute list.
struct BadAttribute;

/// Reads attribute `key` off `e`; a malformed attribute list is an error,
/// never an absent attribute.
fn attr(e: &BytesStart<'_>, key: &str) -> Result<Option<String>, BadAttribute> {
    for a in e.attributes() {
        let a = a.map_err(|_| BadAttribute)?;
        if a.key.as_ref() == key {
            let value = a
                .normalized_value(XmlVersion::default())
                .map_err(|_| BadAttribute)?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

impl From<BadAttribute> for RepomdError {
    fn from(_: BadAttribute) -> Self {
        Self::Unreadable
    }
}

impl From<BadAttribute> for PrimaryError {
    fn from(_: BadAttribute) -> Self {
        Self::Unreadable
    }
}

/// The `href` of `repomd.xml`'s primary entry. The document must be complete:
/// a `<repomd>` root that closes, with no DTD.
fn parse_repomd(xml: &[u8]) -> Result<String, RepomdError> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut closed = false;
    let mut in_primary = false;
    let mut href: Option<Result<String, RepomdError>> = None;
    loop {
        buf.clear();
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|_| RepomdError::Unreadable)?;
        match event {
            Event::DocType(_) => return Err(RepomdError::Unreadable),
            Event::Start(e) => {
                if closed || (depth == 0 && e.local_name().as_ref() != "repomd") {
                    return Err(RepomdError::Unreadable);
                }
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(RepomdError::Unreadable);
                }
                if depth == 2 && e.local_name().as_ref() == "data" {
                    in_primary = attr(&e, "type")?.as_deref() == Some("primary");
                } else if depth == 3 && in_primary && e.local_name().as_ref() == "location" {
                    let found = location(&e)?;
                    href.get_or_insert(found);
                }
            }
            Event::Empty(e) => {
                if closed || depth == 0 {
                    return Err(RepomdError::Unreadable);
                }
                if depth == 2 && in_primary && e.local_name().as_ref() == "location" {
                    let found = location(&e)?;
                    href.get_or_insert(found);
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(RepomdError::Unreadable)?;
                if depth == 1 {
                    in_primary = false;
                } else if depth == 0 {
                    closed = true;
                }
            }
            Event::Text(t) if depth == 0 && !t.trim().is_empty() => {
                return Err(RepomdError::Unreadable);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !closed {
        return Err(RepomdError::Unreadable);
    }
    href.unwrap_or(Err(RepomdError::NoPrimary))
}

/// The `href` of a primary `<location>`: the outer `Err` for an unreadable
/// tag, the inner one for a well-formed but unusable location.
fn location(e: &BytesStart<'_>) -> Result<Result<String, RepomdError>, RepomdError> {
    if attr(e, "xml:base")?.is_some() {
        return Ok(Err(RepomdError::ForeignBase));
    }
    Ok(attr(e, "href")?
        .filter(|h| !h.is_empty())
        .ok_or(RepomdError::NoPrimary))
}

/// What the primary metadata holds.
#[derive(Debug, Default, PartialEq, Eq)]
struct PrimaryScan {
    /// `<package>` entries.
    count: usize,
    /// The listed pairs that are present.
    present: Listed,
}

/// Sniffs the compression of `body` from its leading bytes.
fn decoder<'a>(body: &'a [u8]) -> Result<Box<dyn Read + 'a>, PrimaryError> {
    match body {
        [0x1f, 0x8b, ..] => Ok(Box::new(flate2::read::MultiGzDecoder::new(body))),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Ok(Box::new(ZstdFrames {
            rest: body,
            frame: None,
        })),
        [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => Err(PrimaryError::Unsupported("xz")),
        [b'B', b'Z', b'h', ..] => Err(PrimaryError::Unsupported("bzip2")),
        _ => Ok(Box::new(body)),
    }
}

/// Decodes every zstd frame in `rest` in turn, as the reference decoder does;
/// a frame that does not decode is an I/O error, never an early end.
struct ZstdFrames<'a> {
    rest: &'a [u8],
    frame: Option<ruzstd::decoding::StreamingDecoder<&'a [u8], ruzstd::decoding::FrameDecoder>>,
}

impl Read for ZstdFrames<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(frame) = self.frame.as_mut() {
                let n = frame.read(out)?;
                if n > 0 || out.is_empty() {
                    return Ok(n);
                }
                self.rest = self.frame.take().map_or(self.rest, |f| f.into_inner());
            }
            if self.rest.is_empty() {
                return Ok(0);
            }
            let frame = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
                self.rest,
                MAX_ZSTD_WINDOW,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            self.frame = Some(frame);
        }
    }
}

/// Which bound a [`Metered`] read tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trip {
    Total,
    Event,
}

/// A reader that fails once more than `cap` bytes have come through in all,
/// or more than `max_event` since `mark`, which the parser moves to the
/// current count after every event: one event is buffered whole, so this
/// bounds the parser's memory, not only the bytes streamed.
struct Metered<R> {
    inner: R,
    seen: Rc<Cell<usize>>,
    mark: Rc<Cell<usize>>,
    tripped: Rc<Cell<Option<Trip>>>,
    cap: usize,
    max_event: usize,
}

impl<R: Read> Read for Metered<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        let seen = self.seen.get() + n;
        let trip = if seen > self.cap {
            Some(Trip::Total)
        } else if seen.saturating_sub(self.mark.get()) > self.max_event + READ_AHEAD {
            Some(Trip::Event)
        } else {
            None
        };
        if let Some(trip) = trip {
            self.tripped.set(Some(trip));
            return Err(std::io::Error::other(
                "primary metadata size bound exceeded",
            ));
        }
        self.seen.set(seen);
        Ok(n)
    }
}

/// Counts `<package>` entries and records which `listed` pairs are present.
/// The document must be complete: a `<metadata>` root that closes, and a
/// `packages` count, when given, that matches.
fn parse_primary(
    body: &[u8],
    listed: &Listed,
    cap: usize,
    max_event: usize,
) -> Result<PrimaryScan, PrimaryError> {
    let seen = Rc::new(Cell::new(0));
    let mark = Rc::new(Cell::new(0));
    let tripped = Rc::new(Cell::new(None));
    let metered = Metered {
        inner: decoder(body)?,
        seen: Rc::clone(&seen),
        mark: Rc::clone(&mark),
        tripped: Rc::clone(&tripped),
        cap,
        max_event,
    };
    let mut reader = Reader::from_reader(std::io::BufReader::with_capacity(READ_AHEAD, metered));
    let result = scan_primary(&mut reader, listed, &|| mark.set(seen.get()));
    match tripped.get() {
        Some(Trip::Total) => Err(PrimaryError::TooLarge(cap)),
        Some(Trip::Event) => Err(PrimaryError::EventTooLarge(max_event)),
        None => result,
    }
}

/// One `<package>` being read.
#[derive(Default)]
struct Entry {
    name: String,
    arch: String,
    ver: String,
    rel: String,
}

/// Which child text of a `<package>` is being read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    None,
    Name,
    Arch,
}

/// Appends `text` to `field`, refusing a field over [`MAX_FIELD`].
fn push_field(field: &mut String, text: &str) -> Result<(), PrimaryError> {
    if field.len() + text.len() > MAX_FIELD {
        return Err(PrimaryError::Unreadable);
    }
    field.push_str(text);
    Ok(())
}

fn scan_primary<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    listed: &Listed,
    event_done: &dyn Fn(),
) -> Result<PrimaryScan, PrimaryError> {
    let mut scan = PrimaryScan::default();
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut closed = false;
    let mut declared: Option<usize> = None;
    let mut entry: Option<Entry> = None;
    let mut field = Field::None;

    let mut open_root = |e: &BytesStart<'_>| -> Result<(), PrimaryError> {
        if e.local_name().as_ref() != "metadata" {
            return Err(PrimaryError::Unreadable);
        }
        declared = match attr(e, "packages")? {
            Some(n) => Some(n.trim().parse().map_err(|_| PrimaryError::Unreadable)?),
            None => None,
        };
        Ok(())
    };

    loop {
        buf.clear();
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|_| PrimaryError::Unreadable)?;
        event_done();
        match event {
            Event::DocType(_) => return Err(PrimaryError::Unreadable),
            Event::Start(e) => {
                if closed {
                    return Err(PrimaryError::Unreadable);
                }
                if depth == 0 {
                    open_root(&e)?;
                } else if depth == 1 && e.local_name().as_ref() == "package" {
                    entry = Some(Entry::default());
                } else if depth == 2 && entry.is_some() {
                    field = match e.local_name().as_ref() {
                        "name" => Field::Name,
                        "arch" => Field::Arch,
                        _ => Field::None,
                    };
                    if e.local_name().as_ref() == "version"
                        && let Some(entry) = entry.as_mut()
                    {
                        version(&e, entry)?;
                    }
                }
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(PrimaryError::Unreadable);
                }
            }
            Event::Empty(e) => {
                if closed {
                    return Err(PrimaryError::Unreadable);
                }
                if depth == 0 {
                    open_root(&e)?;
                    closed = true;
                } else if depth == 1 && e.local_name().as_ref() == "package" {
                    scan.count += 1;
                } else if depth == 2
                    && e.local_name().as_ref() == "version"
                    && let Some(entry) = entry.as_mut()
                {
                    version(&e, entry)?;
                }
            }
            Event::Text(t) if depth == 3 && field != Field::None => {
                if let Some(entry) = entry.as_mut() {
                    let target = if field == Field::Name {
                        &mut entry.name
                    } else {
                        &mut entry.arch
                    };
                    push_field(target, &t)?;
                }
            }
            // A tracked field is plain text; anything else could hide its value.
            Event::CData(_) | Event::GeneralRef(_) if depth == 3 && field != Field::None => {
                return Err(PrimaryError::Unreadable);
            }
            Event::Text(t) if depth == 0 && !t.trim().is_empty() => {
                return Err(PrimaryError::Unreadable);
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(PrimaryError::Unreadable)?;
                field = Field::None;
                if depth == 1
                    && let Some(done) = entry.take()
                {
                    scan.count += 1;
                    let pair = (
                        done.name.trim().to_owned(),
                        format!("{}-{}.{}", done.ver, done.rel, done.arch.trim()),
                    );
                    if listed.contains(&pair) {
                        scan.present.insert(pair);
                    }
                } else if depth == 0 {
                    closed = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !closed {
        return Err(PrimaryError::Unreadable);
    }
    if let Some(declared) = declared
        && declared != scan.count
    {
        return Err(PrimaryError::CountMismatch {
            declared,
            listed: scan.count,
        });
    }
    Ok(scan)
}

/// Copies `<version ver= rel=>` onto `entry`.
fn version(e: &BytesStart<'_>, entry: &mut Entry) -> Result<(), PrimaryError> {
    let read = |key| -> Result<String, PrimaryError> {
        let value = attr(e, key)?.unwrap_or_default();
        if value.len() > MAX_FIELD {
            return Err(PrimaryError::Unreadable);
        }
        Ok(value)
    };
    entry.ver = read("ver")?;
    entry.rel = read("rel")?;
    Ok(())
}

/// The parsers, for the detached `fuzz/` crate only.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    /// Forwards to `parse_repomd`.
    #[must_use]
    pub fn parse_repomd(xml: &[u8]) -> bool {
        super::parse_repomd(xml).is_ok()
    }

    /// Forwards to `parse_primary` with small caps, wanting `listed`.
    #[must_use]
    pub fn parse_primary(body: &[u8], listed: &[(String, String)]) -> bool {
        let listed = listed.iter().cloned().collect();
        super::parse_primary(body, &listed, 8 * 1024 * 1024, 64 * 1024).is_ok()
    }
}

#[cfg(test)]
#[path = "repo_check_tests.rs"]
mod tests;
