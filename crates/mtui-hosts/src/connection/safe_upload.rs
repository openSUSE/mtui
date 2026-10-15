//! Staged, atomically published uploads over a dedicated raw SFTP session.
//!
//! Opening the destination for truncation follows a final symlink and writes
//! through a hardlink's shared inode, and exposes a half-written file to
//! readers. An upload therefore never touches the destination until its bytes
//! are complete:
//!
//! 1. the destination's ancestors are checked entry by entry (`lstat`, never
//!    following links) and must belong to root or the upload user and not be
//!    writable by anyone else unless sticky;
//! 2. a private `0700` `.mtui-upload-<uuid>` directory is created beside the
//!    destination, and the payload is created in it exclusively;
//! 3. the payload gets mode `0770` through its handle and is verified;
//! 4. `posix-rename@openssh.com` replaces the destination's directory entry.
//!
//! Anything that cannot be guaranteed fails closed: there is no truncating,
//! remove-then-rename or ordinary-rename fallback. SFTP v3 has no directory
//! handles, so the ancestor check is a policy over trusted parents rather than
//! a race-free proof; another user of the *same* account, or root, is out of
//! scope. Atomic publication is not durability: nothing is `fsync`ed.

use std::collections::HashMap;
use std::path::Path;

use futures::future;
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode, Version};

use crate::error::{HostError, Result, UploadRefusal};

pub(super) const POSIX_RENAME: &str = "posix-rename@openssh.com";
const LIMITS: &str = "limits@openssh.com";
const STAGE_PREFIX: &str = ".mtui-upload-";
const PAYLOAD: &str = "payload";
const STAGE_ATTEMPTS: usize = 3;
const DEFAULT_CHUNK: usize = 32 * 1024;
const WRITE_WINDOW: usize = 8;
const PUBLISHED_MODE: u32 = 0o770;
const NEW_DIR_MODE: u32 = 0o755;
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const STICKY: u32 = 0o1000;
const OTHERS_WRITE: u32 = 0o022;

/// The raw SFTP verbs an upload needs. `RawSftpSession` is the one production
/// implementor; tests script another to observe the exact request order.
pub(super) trait SftpOps {
    async fn realpath(&self, path: &str) -> std::result::Result<String, SftpError>;
    async fn lstat(&self, path: &str) -> std::result::Result<FileAttributes, SftpError>;
    async fn mkdir(&self, path: &str, mode: u32) -> std::result::Result<(), SftpError>;
    async fn create_exclusive(
        &self,
        path: &str,
        mode: u32,
    ) -> std::result::Result<String, SftpError>;
    async fn write(
        &self,
        handle: &str,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<(), SftpError>;
    async fn fchmod(&self, handle: &str, mode: u32) -> std::result::Result<(), SftpError>;
    async fn fstat(&self, handle: &str) -> std::result::Result<FileAttributes, SftpError>;
    async fn close(&self, handle: &str) -> std::result::Result<(), SftpError>;
    async fn remove(&self, path: &str) -> std::result::Result<(), SftpError>;
    async fn rmdir(&self, path: &str) -> std::result::Result<(), SftpError>;
    async fn posix_rename(&self, from: &str, to: &str) -> std::result::Result<(), SftpError>;

    /// A name for a staging directory, unique per call.
    fn stage_name(&self) -> String {
        format!("{STAGE_PREFIX}{}", uuid::Uuid::new_v4().simple())
    }

    /// Largest write that fits the server's advertised limits.
    fn write_chunk(&self) -> usize {
        DEFAULT_CHUNK
    }
}

fn mode_attrs(mode: u32) -> FileAttributes {
    FileAttributes {
        permissions: Some(mode),
        ..FileAttributes::empty()
    }
}

fn expect_ok(packet: Packet) -> std::result::Result<(), SftpError> {
    match packet {
        Packet::Status(s) if s.status_code == StatusCode::Ok => Ok(()),
        Packet::Status(s) => Err(SftpError::Status(s)),
        _ => Err(SftpError::UnexpectedPacket),
    }
}

impl SftpOps for RawSftpSession {
    async fn realpath(&self, path: &str) -> std::result::Result<String, SftpError> {
        let name = RawSftpSession::realpath(self, path).await?;
        name.files
            .into_iter()
            .next()
            .map(|f| f.filename)
            .ok_or(SftpError::UnexpectedPacket)
    }

    async fn lstat(&self, path: &str) -> std::result::Result<FileAttributes, SftpError> {
        Ok(RawSftpSession::lstat(self, path).await?.attrs)
    }

    async fn mkdir(&self, path: &str, mode: u32) -> std::result::Result<(), SftpError> {
        RawSftpSession::mkdir(self, path, mode_attrs(mode))
            .await
            .map(drop)
    }

    async fn create_exclusive(
        &self,
        path: &str,
        mode: u32,
    ) -> std::result::Result<String, SftpError> {
        let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE;
        Ok(RawSftpSession::open(self, path, flags, mode_attrs(mode))
            .await?
            .handle)
    }

    async fn write(
        &self,
        handle: &str,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<(), SftpError> {
        RawSftpSession::write(self, handle, offset, data.to_vec())
            .await
            .map(drop)
    }

    async fn fchmod(&self, handle: &str, mode: u32) -> std::result::Result<(), SftpError> {
        RawSftpSession::fsetstat(self, handle, mode_attrs(mode))
            .await
            .map(drop)
    }

    async fn fstat(&self, handle: &str) -> std::result::Result<FileAttributes, SftpError> {
        Ok(RawSftpSession::fstat(self, handle).await?.attrs)
    }

    async fn close(&self, handle: &str) -> std::result::Result<(), SftpError> {
        RawSftpSession::close(self, handle).await.map(drop)
    }

    async fn remove(&self, path: &str) -> std::result::Result<(), SftpError> {
        RawSftpSession::remove(self, path).await.map(drop)
    }

    async fn rmdir(&self, path: &str) -> std::result::Result<(), SftpError> {
        RawSftpSession::rmdir(self, path).await.map(drop)
    }

    async fn posix_rename(&self, from: &str, to: &str) -> std::result::Result<(), SftpError> {
        let payload =
            rename_payload(from, to).map_err(|e| SftpError::UnexpectedBehavior(e.to_owned()))?;
        expect_ok(self.extended(POSIX_RENAME, payload).await?)
    }
}

/// Wire form of `posix-rename@openssh.com`: two SSH strings, source then
/// destination.
fn rename_payload(from: &str, to: &str) -> std::result::Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(8 + from.len() + to.len());
    for part in [from, to] {
        let len = u32::try_from(part.len()).map_err(|_| "path too long for SFTP")?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    Ok(out)
}

/// Refuses a server that cannot publish atomically, before anything is opened.
pub(super) fn require_posix_rename(
    extensions: &HashMap<String, String>,
) -> std::result::Result<(), UploadRefusal> {
    match extensions.get(POSIX_RENAME).map(String::as_str) {
        Some("1") => Ok(()),
        _ => Err(UploadRefusal::PosixRenameUnavailable),
    }
}

/// Completes a freshly initialised raw session: requires atomic rename and
/// adopts the server's write limit when it advertises one.
pub(super) async fn negotiate(
    raw: &mut RawSftpSession,
    host: &str,
    path: &Path,
    version: &Version,
) -> Result<()> {
    require_posix_rename(&version.extensions).map_err(|reason| HostError::UploadRefused {
        host: host.to_owned(),
        path: path.display().to_string(),
        reason,
    })?;
    if version.extensions.get(LIMITS).map(String::as_str) == Some("1")
        && let Ok(limits) = raw.limits().await
    {
        raw.set_limits(Limits::from(limits));
    }
    Ok(())
}

/// Parses `id -u` output: exit 0 and exactly one unsigned decimal.
pub(super) fn parse_uid(exitcode: i16, stdout: &str) -> std::result::Result<u32, UploadRefusal> {
    let unknown = |why: String| UploadRefusal::UnknownUser(why);
    if exitcode != 0 {
        return Err(unknown(format!("`id -u` exited with {exitcode}")));
    }
    let text = stdout.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(unknown(format!("unexpected `id -u` output {text:?}")));
    }
    text.parse()
        .map_err(|_| unknown(format!("unexpected `id -u` output {text:?}")))
}

/// A destination split into plain directory components and a file name.
#[derive(Debug, PartialEq, Eq)]
struct Dest {
    absolute: bool,
    dirs: Vec<String>,
    name: String,
}

impl Dest {
    fn parse(remote: &Path) -> std::result::Result<Self, UploadRefusal> {
        let bad = |why: &str| UploadRefusal::BadPath(format!("{}: {why}", remote.display()));
        let text = remote.to_str().ok_or_else(|| bad("not valid UTF-8"))?;
        if text.ends_with('/') {
            return Err(bad("names a directory"));
        }
        let mut parts = Vec::new();
        for part in text.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err(bad("contains `..`")),
                p if p.contains('\0') => return Err(bad("contains a NUL byte")),
                p => parts.push(p.to_owned()),
            }
        }
        let name = parts.pop().ok_or_else(|| bad("has no file name"))?;
        Ok(Self {
            absolute: text.starts_with('/'),
            dirs: parts,
            name,
        })
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn permission_bits(attrs: &FileAttributes) -> Option<u32> {
    attrs.permissions
}

/// Judges one ancestor (or the staging directory's parent chain entry).
fn check_ancestor(
    dir: &str,
    attrs: &FileAttributes,
    uid: u32,
) -> std::result::Result<(), UploadRefusal> {
    let (Some(perm), Some(owner)) = (permission_bits(attrs), attrs.uid) else {
        return Err(UploadRefusal::MissingAttributes {
            dir: dir.to_owned(),
        });
    };
    if perm & S_IFMT != S_IFDIR {
        return Err(UploadRefusal::NotDirectory {
            dir: dir.to_owned(),
        });
    }
    if owner != 0 && owner != uid {
        return Err(UploadRefusal::ForeignOwner {
            dir: dir.to_owned(),
            owner,
            uid,
        });
    }
    if perm & OTHERS_WRITE != 0 && perm & STICKY == 0 {
        return Err(UploadRefusal::WritableNotSticky {
            dir: dir.to_owned(),
            mode: perm & 0o7777,
        });
    }
    Ok(())
}

/// How an upload ended badly, before it is attributed to a host and path.
#[derive(Debug)]
enum Fail {
    Refused(UploadRefusal),
    Failed(Stop),
    Indeterminate(String),
}

/// A definite failure before publication. `lost`: the session is gone, so it
/// cannot carry cleanup either.
#[derive(Debug)]
struct Stop {
    reason: String,
    lost: bool,
}

impl Stop {
    fn plain(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            lost: false,
        }
    }
}

impl From<Stop> for Fail {
    fn from(stop: Stop) -> Self {
        Self::Failed(stop)
    }
}

impl Fail {
    fn plain(reason: impl Into<String>) -> Self {
        Stop::plain(reason).into()
    }
}

impl From<UploadRefusal> for Fail {
    fn from(r: UploadRefusal) -> Self {
        Self::Refused(r)
    }
}

/// A reply was lost or never arrived, so the request's effect is unknown and
/// the session cannot be trusted to carry cleanup either.
fn transport_lost(e: &SftpError) -> bool {
    match e {
        SftpError::Status(s) => {
            matches!(
                s.status_code,
                StatusCode::NoConnection | StatusCode::ConnectionLost
            )
        }
        SftpError::Limited(_) => false,
        _ => true,
    }
}

fn stopped(what: &str, e: &SftpError) -> Stop {
    Stop {
        reason: format!("{what}: {e}"),
        lost: transport_lost(e),
    }
}

fn failed(what: &str, e: &SftpError) -> Fail {
    stopped(what, e).into()
}

fn is_missing(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile)
}

/// Walks the destination's directories top-down, creating absent ones under the
/// already-verified chain, and returns the verified parent's path.
async fn verified_parent<O: SftpOps>(
    ops: &O,
    dest: &Dest,
    uid: u32,
) -> std::result::Result<String, Fail> {
    let mut dir = if dest.absolute {
        "/".to_owned()
    } else {
        ops.realpath(".")
            .await
            .map_err(|e| failed("resolve the SFTP start directory", &e))?
    };
    let mut chain = vec![dir.clone()];
    for part in &dest.dirs {
        dir = join(&dir, part);
        chain.push(dir.clone());
    }
    for (i, current) in chain.iter().enumerate() {
        let attrs = match ops.lstat(current).await {
            Ok(attrs) => attrs,
            Err(e) if is_missing(&e) && i > 0 => {
                let made = ops.mkdir(current, NEW_DIR_MODE).await;
                // A racing creator is fine: whatever now exists must pass the check.
                match ops.lstat(current).await {
                    Ok(attrs) => attrs,
                    Err(e) => {
                        return Err(match made {
                            Err(mkdir_err) => failed(&format!("create {current}"), &mkdir_err),
                            Ok(()) => failed(&format!("inspect {current}"), &e),
                        });
                    }
                }
            }
            Err(e) => return Err(failed(&format!("inspect {current}"), &e)),
        };
        check_ancestor(current, &attrs, uid)?;
    }
    chain
        .pop()
        .ok_or_else(|| Fail::plain("empty destination chain".to_owned()))
}

struct Staged {
    dir: String,
    payload: String,
}

/// Creates a private staging directory that this call alone made.
async fn create_stage<O: SftpOps>(
    ops: &O,
    parent: &str,
    uid: u32,
) -> std::result::Result<Staged, Fail> {
    let mut last = None;
    for _ in 0..STAGE_ATTEMPTS {
        let dir = join(parent, &ops.stage_name());
        match ops.mkdir(&dir, 0o700).await {
            Ok(()) => {}
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Failure => {
                last = Some(format!("{dir} already exists"));
                continue;
            }
            Err(e) => return Err(failed(&format!("create staging directory {dir}"), &e)),
        }
        let staged = Staged {
            payload: join(&dir, PAYLOAD),
            dir,
        };
        return match verify_stage(ops, &staged.dir, uid).await {
            Ok(()) => Ok(staged),
            Err(fail) => {
                // Only a directory this call created is safe to remove.
                let _ = ops.rmdir(&staged.dir).await;
                Err(fail)
            }
        };
    }
    Err(Fail::plain(format!(
        "could not create a fresh staging directory: {}",
        last.unwrap_or_default()
    )))
}

async fn verify_stage<O: SftpOps>(ops: &O, dir: &str, uid: u32) -> std::result::Result<(), Fail> {
    let attrs = ops
        .lstat(dir)
        .await
        .map_err(|e| failed(&format!("inspect staging directory {dir}"), &e))?;
    let (Some(perm), Some(owner)) = (permission_bits(&attrs), attrs.uid) else {
        return Err(UploadRefusal::MissingAttributes {
            dir: dir.to_owned(),
        }
        .into());
    };
    if perm & S_IFMT != S_IFDIR || owner != uid || perm & 0o777 != 0o700 {
        return Err(Fail::plain(format!(
            "staging directory {dir} is not private to the upload user"
        )));
    }
    Ok(())
}

/// Writes and seals the payload, returning on success with its handle closed.
async fn write_payload<O: SftpOps>(
    ops: &O,
    staged: &Staged,
    data: &[u8],
    uid: u32,
    open_handle: &mut Option<String>,
) -> std::result::Result<(), Stop> {
    let handle = ops
        .create_exclusive(&staged.payload, 0o600)
        .await
        .map_err(|e| stopped("create the staged payload", &e))?;
    let handle = open_handle.insert(handle).clone();

    let chunk = ops.write_chunk().max(1);
    let mut writes = Vec::new();
    let mut offset: u64 = 0;
    for piece in data.chunks(chunk) {
        writes.push((offset, piece));
        offset = offset
            .checked_add(piece.len() as u64)
            .ok_or_else(|| Stop::plain("payload offset overflow".to_owned()))?;
    }
    for batch in writes.chunks(WRITE_WINDOW) {
        let mut inflight = Vec::with_capacity(batch.len());
        for &(at, piece) in batch {
            let handle = handle.as_str();
            inflight.push(async move { ops.write(handle, at, piece).await.map_err(|e| (at, e)) });
        }
        future::try_join_all(inflight)
            .await
            .map_err(|(at, e)| stopped(&format!("write at offset {at}"), &e))?;
    }

    ops.fchmod(&handle, PUBLISHED_MODE)
        .await
        .map_err(|e| stopped("set mode 0770 on the staged payload", &e))?;
    let attrs = ops
        .fstat(&handle)
        .await
        .map_err(|e| stopped("inspect the staged payload", &e))?;
    let sealed = attrs
        .permissions
        .is_some_and(|p| p & S_IFMT == S_IFREG && p & 0o7777 == PUBLISHED_MODE)
        && attrs.uid == Some(uid)
        && attrs.size == Some(offset);
    if !sealed {
        return Err(Stop::plain(
            "the staged payload does not have the expected owner, mode and size".to_owned(),
        ));
    }
    ops.close(&handle)
        .await
        .map_err(|e| stopped("close the staged payload", &e))?;
    *open_handle = None;
    Ok(())
}

/// Best-effort removal of this call's own staging, naming what was left.
async fn clean_up<O: SftpOps>(ops: &O, staged: &Staged, handle: Option<&str>) -> Option<String> {
    let mut left = Vec::new();
    if let Some(h) = handle
        && let Err(e) = ops.close(h).await
    {
        left.push(format!("close handle: {e}"));
    }
    match ops.remove(&staged.payload).await {
        Ok(()) => {}
        Err(e) if is_missing(&e) => {}
        Err(e) => left.push(format!("remove {}: {e}", staged.payload)),
    }
    if let Err(e) = ops.rmdir(&staged.dir).await {
        left.push(format!("remove {}: {e}", staged.dir));
    }
    (!left.is_empty()).then(|| format!("staging left behind ({})", left.join("; ")))
}

fn attribute(host: &str, path: &Path, fail: Fail) -> HostError {
    let (host, path) = (host.to_owned(), path.display().to_string());
    match fail {
        Fail::Refused(reason) => HostError::UploadRefused { host, path, reason },
        Fail::Failed(Stop { reason, .. }) => HostError::UploadFailed { host, path, reason },
        Fail::Indeterminate(reason) => HostError::UploadIndeterminate { host, path, reason },
    }
}

/// Uploads `data` so that `remote` is replaced atomically or left untouched.
///
/// `uid` is the upload login's numeric user id.
pub(super) async fn publish<O: SftpOps>(
    ops: &O,
    host: &str,
    uid: u32,
    data: &[u8],
    remote: &Path,
) -> Result<()> {
    run(ops, uid, data, remote)
        .await
        .map_err(|fail| attribute(host, remote, fail))
}

async fn run<O: SftpOps>(
    ops: &O,
    uid: u32,
    data: &[u8],
    remote: &Path,
) -> std::result::Result<(), Fail> {
    let dest = Dest::parse(remote)?;
    let parent = verified_parent(ops, &dest, uid).await?;
    let target = join(&parent, &dest.name);
    let staged = create_stage(ops, &parent, uid).await?;

    let mut open_handle = None;
    if let Err(stop) = write_payload(ops, &staged, data, uid, &mut open_handle).await {
        return Err(abandon(ops, &staged, open_handle.as_deref(), stop).await);
    }

    match ops.posix_rename(&staged.payload, &target).await {
        Ok(()) => {
            if let Err(e) = ops.rmdir(&staged.dir).await {
                tracing::warn!(dir = %staged.dir, "uploaded, but the empty staging directory remains: {e}");
            }
            Ok(())
        }
        Err(e) if transport_lost(&e) => Err(Fail::Indeterminate(format!(
            "publishing rename got no verdict ({e}); staging {} may remain",
            staged.dir
        ))),
        Err(e) => Err(abandon(ops, &staged, None, stopped("publish the upload", &e)).await),
    }
}

/// Cleans up after a pre-publication failure, keeping the original cause.
async fn abandon<O: SftpOps>(ops: &O, staged: &Staged, handle: Option<&str>, stop: Stop) -> Fail {
    let Stop { reason, lost } = stop;
    let note = if lost {
        Some(format!("staging {} left behind (session lost)", staged.dir))
    } else {
        clean_up(ops, staged, handle).await
    };
    Fail::Failed(Stop {
        reason: match note {
            Some(note) => format!("{reason}; {note}"),
            None => reason,
        },
        lost,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    use russh_sftp::protocol::Status;

    use super::*;

    const UID: u32 = 1000;
    const DIR: u32 = S_IFDIR;
    const REG: u32 = S_IFREG;
    const LNK: u32 = 0o120_000;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Node {
        Dir { mode: u32, uid: u32 },
        File { mode: u32, uid: u32, data: Vec<u8> },
        Link,
    }

    fn status(code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 0,
            status_code: code,
            error_message: code.to_string(),
            language_tag: String::new(),
        })
    }

    struct Inject {
        op: &'static str,
        path_has: &'static str,
        err: SftpError,
    }

    /// An independent, scripted model of a server: it records every request and
    /// applies it to a small in-memory tree, without using the code under test.
    struct Fake {
        nodes: Mutex<BTreeMap<String, Node>>,
        handles: Mutex<BTreeMap<String, String>>,
        log: Mutex<Vec<String>>,
        inject: Mutex<Vec<Inject>>,
        names: Mutex<VecDeque<String>>,
        chunk: usize,
        reported_size_delta: u64,
        mkdir_ignores_mode: bool,
        bare_stage_attrs: bool,
    }

    impl Fake {
        fn new() -> Self {
            let mut nodes = BTreeMap::new();
            for dir in ["/", "/srv", "/home", "/home/u"] {
                nodes.insert(
                    dir.to_owned(),
                    Node::Dir {
                        mode: 0o755,
                        uid: 0,
                    },
                );
            }
            nodes.insert(
                "/srv".to_owned(),
                Node::Dir {
                    mode: 0o755,
                    uid: UID,
                },
            );
            Self {
                nodes: Mutex::new(nodes),
                handles: Mutex::default(),
                log: Mutex::default(),
                inject: Mutex::default(),
                names: Mutex::new((1..=9).map(|i| format!("{STAGE_PREFIX}n{i}")).collect()),
                chunk: 4,
                reported_size_delta: 0,
                mkdir_ignores_mode: false,
                bare_stage_attrs: false,
            }
        }

        fn put(&self, path: &str, node: Node) {
            self.nodes.lock().unwrap().insert(path.to_owned(), node);
        }

        fn fail(&self, op: &'static str, path_has: &'static str, err: SftpError) {
            self.inject
                .lock()
                .unwrap()
                .push(Inject { op, path_has, err });
        }

        fn node(&self, path: &str) -> Option<Node> {
            self.nodes.lock().unwrap().get(path).cloned()
        }

        fn ops(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn count(&self, op: &str) -> usize {
            self.ops()
                .iter()
                .filter(|l| l.starts_with(&format!("{op} ")))
                .count()
        }

        fn pos(&self, op: &str) -> usize {
            self.ops()
                .iter()
                .position(|l| l.starts_with(&format!("{op} ")))
                .unwrap_or_else(|| panic!("no {op} in {:?}", self.ops()))
        }

        fn record(&self, op: &str, path: &str) -> std::result::Result<(), SftpError> {
            self.log.lock().unwrap().push(format!("{op} {path}"));
            let mut inject = self.inject.lock().unwrap();
            match inject
                .iter()
                .position(|i| i.op == op && path.contains(i.path_has))
            {
                Some(i) => Err(inject.remove(i).err),
                None => Ok(()),
            }
        }

        fn children(&self, dir: &str) -> Vec<String> {
            let prefix = if dir == "/" {
                "/".to_owned()
            } else {
                format!("{dir}/")
            };
            self.nodes
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(&prefix) && k.as_str() != dir)
                .cloned()
                .collect()
        }

        fn attrs(node: &Node) -> FileAttributes {
            let (type_bits, mode, uid, size) = match node {
                Node::Dir { mode, uid } => (DIR, *mode, *uid, 0),
                Node::File { mode, uid, data } => (REG, *mode, *uid, data.len() as u64),
                Node::Link => (LNK, 0o777, UID, 0),
            };
            FileAttributes {
                permissions: Some(type_bits | mode),
                uid: Some(uid),
                size: Some(size),
                ..FileAttributes::empty()
            }
        }

        fn path_of(&self, handle: &str) -> String {
            self.handles
                .lock()
                .unwrap()
                .get(handle)
                .cloned()
                .expect("known handle")
        }
    }

    impl SftpOps for Fake {
        async fn realpath(&self, path: &str) -> std::result::Result<String, SftpError> {
            self.record("realpath", path)?;
            Ok("/home/u".to_owned())
        }

        async fn lstat(&self, path: &str) -> std::result::Result<FileAttributes, SftpError> {
            self.record("lstat", path)?;
            if self.bare_stage_attrs && path.contains(STAGE_PREFIX) {
                return Ok(FileAttributes::empty());
            }
            self.node(path)
                .map(|n| Self::attrs(&n))
                .ok_or_else(|| status(StatusCode::NoSuchFile))
        }

        async fn mkdir(&self, path: &str, mode: u32) -> std::result::Result<(), SftpError> {
            self.record("mkdir", path)?;
            let parent = path
                .rsplit_once('/')
                .map_or("/", |(p, _)| if p.is_empty() { "/" } else { p });
            if self.node(parent).is_none() {
                return Err(status(StatusCode::NoSuchFile));
            }
            if self.node(path).is_some() {
                return Err(status(StatusCode::Failure));
            }
            let mode = if self.mkdir_ignores_mode {
                0o777
            } else {
                mode & !0o022
            };
            self.put(path, Node::Dir { mode, uid: UID });
            Ok(())
        }

        async fn create_exclusive(
            &self,
            path: &str,
            mode: u32,
        ) -> std::result::Result<String, SftpError> {
            self.record("open", path)?;
            if self.node(path).is_some() {
                return Err(status(StatusCode::Failure));
            }
            self.put(
                path,
                Node::File {
                    mode: mode & !0o022,
                    uid: UID,
                    data: Vec::new(),
                },
            );
            let handle = format!("h{}", self.handles.lock().unwrap().len() + 1);
            self.handles
                .lock()
                .unwrap()
                .insert(handle.clone(), path.to_owned());
            Ok(handle)
        }

        async fn write(
            &self,
            handle: &str,
            offset: u64,
            data: &[u8],
        ) -> std::result::Result<(), SftpError> {
            let path = self.path_of(handle);
            self.record("write", &format!("{path}@{offset}+{}", data.len()))?;
            let mut nodes = self.nodes.lock().unwrap();
            let Some(Node::File { data: stored, .. }) = nodes.get_mut(&path) else {
                return Err(status(StatusCode::Failure));
            };
            let end = usize::try_from(offset).unwrap() + data.len();
            if stored.len() < end {
                stored.resize(end, 0);
            }
            stored[usize::try_from(offset).unwrap()..end].copy_from_slice(data);
            Ok(())
        }

        async fn fchmod(&self, handle: &str, mode: u32) -> std::result::Result<(), SftpError> {
            let path = self.path_of(handle);
            self.record("fchmod", &path)?;
            if let Some(Node::File { mode: m, .. }) = self.nodes.lock().unwrap().get_mut(&path) {
                *m = mode;
            }
            Ok(())
        }

        async fn fstat(&self, handle: &str) -> std::result::Result<FileAttributes, SftpError> {
            let path = self.path_of(handle);
            self.record("fstat", &path)?;
            let mut attrs = Self::attrs(&self.node(&path).expect("open file"));
            attrs.size = attrs.size.map(|s| s - self.reported_size_delta);
            Ok(attrs)
        }

        async fn close(&self, handle: &str) -> std::result::Result<(), SftpError> {
            let path = self.path_of(handle);
            self.record("close", &path)
        }

        async fn remove(&self, path: &str) -> std::result::Result<(), SftpError> {
            self.record("remove", path)?;
            match self.nodes.lock().unwrap().remove(path) {
                Some(_) => Ok(()),
                None => Err(status(StatusCode::NoSuchFile)),
            }
        }

        async fn rmdir(&self, path: &str) -> std::result::Result<(), SftpError> {
            self.record("rmdir", path)?;
            if !self.children(path).is_empty() {
                return Err(status(StatusCode::Failure));
            }
            self.nodes.lock().unwrap().remove(path);
            Ok(())
        }

        async fn posix_rename(&self, from: &str, to: &str) -> std::result::Result<(), SftpError> {
            self.record("posix_rename", &format!("{from} -> {to}"))?;
            if matches!(self.node(to), Some(Node::Dir { .. })) {
                return Err(status(StatusCode::Failure));
            }
            let moved = self
                .nodes
                .lock()
                .unwrap()
                .remove(from)
                .ok_or_else(|| status(StatusCode::NoSuchFile))?;
            self.put(to, moved);
            Ok(())
        }

        fn stage_name(&self) -> String {
            self.names
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted stage names")
        }

        fn write_chunk(&self) -> usize {
            self.chunk
        }
    }

    fn old_file() -> Node {
        Node::File {
            mode: 0o644,
            uid: UID,
            data: b"OLD".to_vec(),
        }
    }

    async fn upload(fake: &Fake, data: &[u8], path: &str) -> Result<()> {
        publish(fake, "h1", UID, data, Path::new(path)).await
    }

    fn file_data(fake: &Fake, path: &str) -> Vec<u8> {
        match fake.node(path) {
            Some(Node::File { data, .. }) => data,
            other => panic!("{path} is {other:?}"),
        }
    }

    fn assert_no_stage_left(fake: &Fake, parent: &str) {
        let stray: Vec<_> = fake
            .children(parent)
            .into_iter()
            .filter(|c| c.contains(STAGE_PREFIX))
            .collect();
        assert!(stray.is_empty(), "staging left behind: {stray:?}");
    }

    #[tokio::test]
    async fn publishes_after_mode_and_close_and_leaves_no_stage() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());

        upload(&fake, b"0123456789", "/srv/upload.bin")
            .await
            .expect("upload");

        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"0123456789");
        let Some(Node::File { mode, .. }) = fake.node("/srv/upload.bin") else {
            panic!("not a file");
        };
        assert_eq!(mode, 0o770);
        assert!(
            fake.pos("fchmod") < fake.pos("posix_rename"),
            "{:?}",
            fake.ops()
        );
        assert!(
            fake.pos("close") < fake.pos("posix_rename"),
            "{:?}",
            fake.ops()
        );
        assert!(fake.pos("write") < fake.pos("fchmod"));
        assert_eq!(fake.count("posix_rename"), 1);
        assert_no_stage_left(&fake, "/srv");
    }

    #[tokio::test]
    async fn never_opens_or_removes_the_final_path() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());

        upload(&fake, b"NEW", "/srv/upload.bin")
            .await
            .expect("upload");

        for line in fake.ops() {
            assert!(
                !(line.starts_with("open ") || line.starts_with("remove "))
                    || line.contains(STAGE_PREFIX),
                "touched the destination directly: {line}"
            );
        }
    }

    #[tokio::test]
    async fn chunks_cover_every_byte_and_empty_payload_writes_nothing() {
        let fake = Fake::new();
        upload(&fake, b"abcdefghij", "/srv/a")
            .await
            .expect("upload");
        let writes: Vec<_> = fake
            .ops()
            .into_iter()
            .filter(|l| l.starts_with("write "))
            .collect();
        assert_eq!(writes.len(), 3, "{writes:?}");
        assert!(
            writes[2].ends_with("@8+2"),
            "final partial chunk: {writes:?}"
        );
        assert_eq!(file_data(&fake, "/srv/a"), b"abcdefghij");

        let fake = Fake::new();
        upload(&fake, b"", "/srv/e").await.expect("upload");
        assert_eq!(fake.count("write"), 0);
        assert_eq!(file_data(&fake, "/srv/e"), b"");
    }

    #[tokio::test]
    async fn write_failure_keeps_old_destination_and_cleans_stage() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());
        fake.fail("write", "@4+", status(StatusCode::Failure));

        let err = upload(&fake, b"0123456789", "/srv/upload.bin")
            .await
            .unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { host, reason, .. }
            if host == "h1" && reason.contains("write at offset")),
            "{err:?}"
        );
        assert_eq!(fake.count("posix_rename"), 0);
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"OLD");
        assert_no_stage_left(&fake, "/srv");
    }

    #[tokio::test]
    async fn chmod_failure_is_fatal_and_precedes_any_publication() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());
        fake.fail("fchmod", "", status(StatusCode::PermissionDenied));

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert_eq!(fake.count("posix_rename"), 0);
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"OLD");
        assert_no_stage_left(&fake, "/srv");
    }

    #[tokio::test]
    async fn close_failure_is_fatal_and_precedes_any_publication() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());
        fake.fail("close", "", status(StatusCode::Failure));

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert_eq!(fake.count("posix_rename"), 0);
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"OLD");
    }

    #[tokio::test]
    async fn short_or_mismatched_payload_is_not_published() {
        let mut fake = Fake::new();
        fake.reported_size_delta = 1;
        fake.put("/srv/upload.bin", old_file());

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert_eq!(fake.count("posix_rename"), 0);
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"OLD");
    }

    #[tokio::test]
    async fn unsupported_rename_fails_without_any_fallback() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());
        fake.fail("posix_rename", "", status(StatusCode::OpUnsupported));

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert_eq!(fake.count("posix_rename"), 1);
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"OLD");
        assert!(
            fake.ops()
                .iter()
                .all(|l| !l.contains("/srv/upload.bin") || l.starts_with("posix_rename")),
            "fallback touched the destination: {:?}",
            fake.ops()
        );
        assert_no_stage_left(&fake, "/srv");
    }

    #[tokio::test]
    async fn rename_without_verdict_is_indeterminate_and_never_repeated() {
        let fake = Fake::new();
        fake.put("/srv/upload.bin", old_file());
        fake.fail("posix_rename", "", SftpError::Timeout);

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadIndeterminate { path, .. }
            if path == "/srv/upload.bin"),
            "{err:?}"
        );
        assert_eq!(fake.count("posix_rename"), 1);
        let after = &fake.ops()[fake.pos("posix_rename") + 1..];
        assert!(
            after.is_empty(),
            "no cleanup guesses after an unknown outcome: {after:?}"
        );
    }

    #[tokio::test]
    async fn lost_session_before_publication_attempts_no_cleanup() {
        let fake = Fake::new();
        fake.fail("write", "", SftpError::Timeout);

        let err = upload(&fake, b"0123456789", "/srv/a").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("left behind")),
            "{err:?}"
        );
        assert_eq!(fake.count("remove"), 0);
        assert_eq!(fake.count("rmdir"), 0);
    }

    #[tokio::test]
    async fn failed_cleanup_is_named_next_to_the_original_cause() {
        let fake = Fake::new();
        fake.fail("write", "", status(StatusCode::Failure));
        fake.fail("remove", "", status(StatusCode::PermissionDenied));

        let err = upload(&fake, b"0123456789", "/srv/a").await.unwrap_err();

        let HostError::UploadFailed { reason, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(reason.contains("write at offset"), "{reason}");
        assert!(reason.contains("staging left behind"), "{reason}");
    }

    #[tokio::test]
    async fn a_planted_stage_is_never_adopted_and_a_fresh_name_is_used() {
        let fake = Fake::new();
        let planted = format!("/srv/{STAGE_PREFIX}n1");
        fake.put(
            &planted,
            Node::Dir {
                mode: 0o777,
                uid: 4242,
            },
        );
        let decoy = format!("{planted}/{PAYLOAD}");
        fake.put(
            &decoy,
            Node::File {
                mode: 0o666,
                uid: 4242,
                data: b"MINE".to_vec(),
            },
        );

        upload(&fake, b"NEW", "/srv/upload.bin")
            .await
            .expect("retries a fresh name");

        assert_eq!(file_data(&fake, &decoy), b"MINE");
        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"NEW");
        assert!(
            fake.ops()
                .iter()
                .all(|l| !l.starts_with("open ") || !l.contains("n1/"))
        );
    }

    #[tokio::test]
    async fn persistent_stage_collisions_fail_without_touching_the_planted_one() {
        let fake = Fake::new();
        for i in 1..=3 {
            let dir = format!("/srv/{STAGE_PREFIX}n{i}");
            fake.put(
                &dir,
                Node::Dir {
                    mode: 0o777,
                    uid: 4242,
                },
            );
        }

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert_eq!(fake.count("mkdir"), 3, "bounded retries");
        assert_eq!(fake.count("open"), 0);
        assert_eq!(
            fake.count("rmdir"),
            0,
            "planted directories are not ours to remove"
        );
        assert!(fake.node("/srv/upload.bin").is_none());
    }

    #[tokio::test]
    async fn a_stage_that_is_not_private_is_removed_and_the_upload_fails() {
        let mut fake = Fake::new();
        fake.mkdir_ignores_mode = true;

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("not private")),
            "{err:?}"
        );
        assert_eq!(
            fake.count("open"),
            0,
            "no payload in a stage others can enter"
        );
        assert_eq!(fake.count("rmdir"), 1, "own stage removed");
        assert_no_stage_left(&fake, "/srv");
    }

    #[tokio::test]
    async fn a_stage_without_attributes_cannot_be_judged_and_is_removed() {
        let mut fake = Fake::new();
        fake.bare_stage_attrs = true;

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(
            matches!(
                err,
                HostError::UploadRefused {
                    reason: UploadRefusal::MissingAttributes { .. },
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(fake.count("open"), 0);
        assert_eq!(fake.count("rmdir"), 1);
    }

    #[tokio::test]
    async fn stage_creation_failure_is_reported_with_the_directory() {
        let fake = Fake::new();
        fake.fail("mkdir", STAGE_PREFIX, status(StatusCode::PermissionDenied));

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("create staging directory") && reason.contains("Permission denied")),
            "{err:?}"
        );
        assert_eq!(fake.count("open"), 0);
    }

    #[tokio::test]
    async fn an_unreadable_ancestor_fails_the_upload_with_its_name() {
        let fake = Fake::new();
        fake.fail("lstat", "/srv", status(StatusCode::PermissionDenied));

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("inspect /srv")),
            "{err:?}"
        );
        assert_eq!(fake.count("mkdir"), 0);
    }

    #[tokio::test]
    async fn a_created_ancestor_that_cannot_be_inspected_fails_the_upload() {
        let fake = Fake::new();
        fake.fail("lstat", "/srv/a", status(StatusCode::NoSuchFile));
        fake.fail("lstat", "/srv/a", status(StatusCode::Failure));

        let err = upload(&fake, b"NEW", "/srv/a/upload.bin")
            .await
            .unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("inspect /srv/a")),
            "{err:?}"
        );
        assert_eq!(fake.count("open"), 0);
    }

    #[tokio::test]
    async fn cleanup_also_closes_a_payload_handle_the_failed_step_left_open() {
        let fake = Fake::new();
        fake.fail("write", "", status(StatusCode::Failure));
        fake.fail("close", "", status(StatusCode::Failure));

        let err = upload(&fake, b"0123456789", "/srv/a").await.unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("close handle")),
            "{err:?}"
        );
        assert_eq!(fake.count("close"), 1);
    }

    #[tokio::test]
    async fn an_empty_stage_that_cannot_be_removed_after_publication_is_not_a_failure() {
        let fake = Fake::new();
        fake.fail("rmdir", STAGE_PREFIX, status(StatusCode::PermissionDenied));

        upload(&fake, b"NEW", "/srv/upload.bin")
            .await
            .expect("published");

        assert_eq!(file_data(&fake, "/srv/upload.bin"), b"NEW");
    }

    #[tokio::test]
    async fn unsafe_ancestors_are_refused_before_any_stage_exists() {
        type Expect = fn(&UploadRefusal) -> bool;
        let cases: [(&str, Node, Expect); 5] = [
            ("link", Node::Link, |r| {
                matches!(r, UploadRefusal::NotDirectory { .. })
            }),
            ("file", old_file(), |r| {
                matches!(r, UploadRefusal::NotDirectory { .. })
            }),
            (
                "foreign",
                Node::Dir {
                    mode: 0o755,
                    uid: 4242,
                },
                |r| matches!(r, UploadRefusal::ForeignOwner { owner: 4242, .. }),
            ),
            (
                "world",
                Node::Dir {
                    mode: 0o777,
                    uid: UID,
                },
                |r| matches!(r, UploadRefusal::WritableNotSticky { mode: 0o777, .. }),
            ),
            (
                "group",
                Node::Dir {
                    mode: 0o775,
                    uid: 0,
                },
                |r| matches!(r, UploadRefusal::WritableNotSticky { mode: 0o775, .. }),
            ),
        ];
        for (name, node, expected) in cases {
            let fake = Fake::new();
            fake.put(&format!("/srv/{name}"), node);

            let err = upload(&fake, b"NEW", &format!("/srv/{name}/upload.bin"))
                .await
                .unwrap_err();

            let HostError::UploadRefused { reason, host, .. } = &err else {
                panic!("{name}: {err:?}");
            };
            assert_eq!(host, "h1");
            assert!(expected(reason), "{name}: {reason:?}");
            assert_eq!(fake.count("mkdir"), 0, "{name}: refused before staging");
            assert_eq!(fake.count("open"), 0, "{name}");
        }
    }

    #[test]
    fn ancestor_policy_accepts_trusted_directories() {
        let attrs = |perm: u32, uid: u32| FileAttributes {
            permissions: Some(DIR | perm),
            uid: Some(uid),
            ..FileAttributes::empty()
        };
        for (perm, uid) in [
            (0o755, 0),
            (0o700, UID),
            (0o1777, 0),
            (0o1777, UID),
            (0o750, UID),
        ] {
            assert_eq!(
                check_ancestor("/d", &attrs(perm, uid), UID),
                Ok(()),
                "mode {perm:o}"
            );
        }
        assert!(matches!(
            check_ancestor("/d", &attrs(0o1777, 4242), UID),
            Err(UploadRefusal::ForeignOwner { .. })
        ));
        assert!(matches!(
            check_ancestor(
                "/d",
                &FileAttributes {
                    uid: Some(0),
                    ..FileAttributes::empty()
                },
                UID
            ),
            Err(UploadRefusal::MissingAttributes { .. })
        ));
        assert!(matches!(
            check_ancestor(
                "/d",
                &FileAttributes {
                    permissions: Some(DIR | 0o755),
                    ..FileAttributes::empty()
                },
                UID
            ),
            Err(UploadRefusal::MissingAttributes { .. })
        ));
    }

    #[tokio::test]
    async fn missing_ancestors_are_created_then_verified() {
        let fake = Fake::new();

        upload(&fake, b"NEW", "/srv/a/b/upload.bin")
            .await
            .expect("upload");

        assert_eq!(file_data(&fake, "/srv/a/b/upload.bin"), b"NEW");
        let Some(Node::Dir { mode, .. }) = fake.node("/srv/a") else {
            panic!("a is not a dir");
        };
        assert_eq!(mode & 0o022, 0);
    }

    #[tokio::test]
    async fn a_failed_parent_creation_reports_the_creation_error() {
        let fake = Fake::new();
        fake.fail("mkdir", "/srv/a", status(StatusCode::PermissionDenied));

        let err = upload(&fake, b"NEW", "/srv/a/upload.bin")
            .await
            .unwrap_err();

        assert!(
            matches!(&err, HostError::UploadFailed { reason, .. }
            if reason.contains("create /srv/a") && reason.contains("Permission denied")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn relative_destination_resolves_against_the_start_directory() {
        let fake = Fake::new();

        upload(&fake, b"NEW", "up.bin").await.expect("upload");

        assert_eq!(file_data(&fake, "/home/u/up.bin"), b"NEW");
        assert_eq!(fake.count("realpath"), 1);
    }

    #[tokio::test]
    async fn directory_destination_fails_and_is_left_intact() {
        let fake = Fake::new();
        fake.put(
            "/srv/upload.bin",
            Node::Dir {
                mode: 0o755,
                uid: UID,
            },
        );

        let err = upload(&fake, b"NEW", "/srv/upload.bin").await.unwrap_err();

        assert!(matches!(err, HostError::UploadFailed { .. }), "{err:?}");
        assert!(matches!(
            fake.node("/srv/upload.bin"),
            Some(Node::Dir { .. })
        ));
        assert_no_stage_left(&fake, "/srv");
    }

    #[test]
    fn destination_parsing_keeps_plain_components_and_rejects_the_rest() {
        let ok = Dest::parse(Path::new("/tmp//a/./b")).unwrap();
        assert_eq!(
            ok,
            Dest {
                absolute: true,
                dirs: vec!["tmp".into(), "a".into()],
                name: "b".into()
            }
        );
        let rel = Dest::parse(Path::new("x/y")).unwrap();
        assert!(!rel.absolute);
        for bad in ["/a/../b", "/a/", "/", "", ".", "/a/\0b"] {
            assert!(
                matches!(Dest::parse(Path::new(bad)), Err(UploadRefusal::BadPath(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rename_payload_is_two_length_prefixed_strings() {
        assert_eq!(
            rename_payload("/a/p", "/a/f").unwrap(),
            b"\x00\x00\x00\x04/a/p\x00\x00\x00\x04/a/f".to_vec()
        );
        assert_eq!(
            rename_payload("", "x").unwrap(),
            b"\x00\x00\x00\x00\x00\x00\x00\x01x".to_vec()
        );
    }

    #[test]
    fn only_posix_rename_version_one_is_accepted() {
        let ext = |v: Option<&str>| {
            v.map(|v| (POSIX_RENAME.to_owned(), v.to_owned()))
                .into_iter()
                .collect()
        };
        assert_eq!(require_posix_rename(&ext(Some("1"))), Ok(()));
        for other in [None, Some("2"), Some("")] {
            assert_eq!(
                require_posix_rename(&ext(other)),
                Err(UploadRefusal::PosixRenameUnavailable)
            );
        }
    }

    #[test]
    fn uid_parsing_is_strict() {
        assert_eq!(parse_uid(0, "501\n"), Ok(501));
        assert_eq!(parse_uid(0, "0"), Ok(0));
        for (code, out) in [
            (1, "501"),
            (0, ""),
            (0, "root"),
            (0, "-1"),
            (0, "501 20"),
            (0, "4294967296"),
            (-1, "501"),
        ] {
            assert!(
                matches!(parse_uid(code, out), Err(UploadRefusal::UnknownUser(_))),
                "{code} {out:?}"
            );
        }
    }

    #[tokio::test]
    async fn non_utf8_style_and_missing_name_destinations_are_refused_without_requests() {
        let fake = Fake::new();
        let err = upload(&fake, b"x", "/srv/../etc/x").await.unwrap_err();
        assert!(matches!(
            err,
            HostError::UploadRefused {
                reason: UploadRefusal::BadPath(_),
                ..
            }
        ));
        assert!(fake.ops().is_empty());
    }
}
