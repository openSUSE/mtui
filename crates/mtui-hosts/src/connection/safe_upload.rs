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
    /// `lost`: the session is gone, so it cannot carry cleanup either.
    Failed {
        reason: String,
        lost: bool,
    },
    Indeterminate(String),
}

impl Fail {
    fn plain(reason: impl Into<String>) -> Self {
        Self::Failed {
            reason: reason.into(),
            lost: false,
        }
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

fn failed(what: &str, e: &SftpError) -> Fail {
    Fail::Failed {
        reason: format!("{what}: {e}"),
        lost: transport_lost(e),
    }
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
) -> std::result::Result<(), Fail> {
    let handle = ops
        .create_exclusive(&staged.payload, 0o600)
        .await
        .map_err(|e| failed("create the staged payload", &e))?;
    let handle = open_handle.insert(handle).clone();

    let chunk = ops.write_chunk().max(1);
    let mut writes = Vec::new();
    let mut offset: u64 = 0;
    for piece in data.chunks(chunk) {
        writes.push((offset, piece));
        offset = offset
            .checked_add(piece.len() as u64)
            .ok_or_else(|| Fail::plain("payload offset overflow".to_owned()))?;
    }
    for batch in writes.chunks(WRITE_WINDOW) {
        let mut inflight = Vec::with_capacity(batch.len());
        for &(at, piece) in batch {
            let handle = handle.as_str();
            inflight.push(async move { ops.write(handle, at, piece).await.map_err(|e| (at, e)) });
        }
        future::try_join_all(inflight)
            .await
            .map_err(|(at, e)| failed(&format!("write at offset {at}"), &e))?;
    }

    ops.fchmod(&handle, PUBLISHED_MODE)
        .await
        .map_err(|e| failed("set mode 0770 on the staged payload", &e))?;
    let attrs = ops
        .fstat(&handle)
        .await
        .map_err(|e| failed("inspect the staged payload", &e))?;
    let sealed = attrs
        .permissions
        .is_some_and(|p| p & S_IFMT == S_IFREG && p & 0o7777 == PUBLISHED_MODE)
        && attrs.uid == Some(uid)
        && attrs.size == Some(offset);
    if !sealed {
        return Err(Fail::plain(
            "the staged payload does not have the expected owner, mode and size".to_owned(),
        ));
    }
    ops.close(&handle)
        .await
        .map_err(|e| failed("close the staged payload", &e))?;
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
        Fail::Failed { reason, .. } => HostError::UploadFailed { host, path, reason },
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
    if let Err(fail) = write_payload(ops, &staged, data, uid, &mut open_handle).await {
        return Err(abandon(ops, &staged, open_handle.as_deref(), fail).await);
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
        Err(e) => Err(abandon(ops, &staged, None, failed("publish the upload", &e)).await),
    }
}

/// Cleans up after a pre-publication failure, keeping the original cause.
async fn abandon<O: SftpOps>(ops: &O, staged: &Staged, handle: Option<&str>, fail: Fail) -> Fail {
    let Fail::Failed { reason, lost } = fail else {
        return fail;
    };
    let note = if lost {
        Some(format!("staging {} left behind (session lost)", staged.dir))
    } else {
        clean_up(ops, staged, handle).await
    };
    Fail::Failed {
        reason: match note {
            Some(note) => format!("{reason}; {note}"),
            None => reason,
        },
        lost,
    }
}
