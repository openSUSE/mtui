//! Real-filesystem acceptance tests for remote uploads.
//!
//! The tests drive [`SshConnection`] against a disposable OpenSSH `sshd` run as
//! the current user on `127.0.0.1`, serving a scratch directory, so symlink,
//! hardlink and rename behaviour is the operating system's, not a fake's. They
//! are `#[ignore]`d and opt-in:
//!
//! ```sh
//! MTUI_TEST_REAL_SFTP=1 cargo test -p mtui-hosts --test it remote_upload_safety \
//!     -- --ignored --test-threads=1
//! ```
//!
//! Requested but unusable prerequisites (missing `sshd`, `sftp-server` or
//! `ssh-keygen`, or the daemon not coming up) panic instead of passing.
//!
//! Optional environment: `MTUI_TEST_REAL_SSHD` (default `/usr/sbin/sshd`),
//! `MTUI_TEST_REAL_SFTP_SERVER` (default `/usr/libexec/sftp-server`, then
//! `/usr/lib/ssh/sftp-server`), `MTUI_TEST_REAL_SFTP_SCRATCH` (parent of the
//! per-test scratch root, default the system temp dir).
//!
//! Everything lives under a fixture-owned scratch root and uses test-generated
//! keys and password-less public-key login. A same-UID run cannot show how a
//! sticky directory treats a *different* user's entry; that needs separate
//! privileged infrastructure and is not covered here.

use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mtui_hosts::{CommandTimeout, Connection, HostError, HostKeyPolicy, SshConnection};

const SSHD: &str = "/usr/sbin/sshd";
const SFTP_SERVER_CANDIDATES: [&str; 2] = ["/usr/libexec/sftp-server", "/usr/lib/ssh/sftp-server"];
const SENTINEL: &[u8] = b"ORIGINAL";

fn require_opt_in() {
    assert_eq!(
        std::env::var("MTUI_TEST_REAL_SFTP").as_deref(),
        Ok("1"),
        "these tests need a real sshd: set MTUI_TEST_REAL_SFTP=1"
    );
}

fn existing_tool(env: &str, candidates: &[&str]) -> PathBuf {
    if let Ok(p) = std::env::var(env) {
        let p = PathBuf::from(p);
        assert!(
            p.exists(),
            "{env} points at a missing file: {}",
            p.display()
        );
        return p;
    }
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!("MTUI_TEST_REAL_SFTP=1 but none of {candidates:?} exists; set {env}")
        })
}

fn current_user() -> String {
    let out = Command::new("id")
        .arg("-un")
        .output()
        .expect("run `id -un`");
    assert!(out.status.success(), "`id -un` failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn keygen(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
}

fn free_port() -> u16 {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind probe port");
    l.local_addr().expect("local addr").port()
}

/// A running disposable `sshd` plus the scratch tree it serves.
///
/// Dropping it kills the daemon and removes only the scratch root it created.
struct RealSshd {
    child: Child,
    port: u16,
    user: String,
    client_key: PathBuf,
    known_hosts: PathBuf,
    /// Canonical scratch root; the SFTP start directory and every test path.
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl RealSshd {
    fn start() -> Self {
        require_opt_in();
        let sshd = existing_tool("MTUI_TEST_REAL_SSHD", &[SSHD]);
        let sftp_server = existing_tool("MTUI_TEST_REAL_SFTP_SERVER", &SFTP_SERVER_CANDIDATES);

        let base = std::env::var_os("MTUI_TEST_REAL_SFTP_SCRATCH")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = tempfile::Builder::new()
            .prefix("mtui-real-sftp-")
            .tempdir_in(&base)
            .expect("create scratch root");
        // Canonical: a symlinked ancestor (macOS `/var`) is refused by design.
        let scratch = dir.path().canonicalize().expect("canonicalize scratch");
        let state = scratch.join("sshd");
        let root = scratch.join("files");
        fs::create_dir_all(&state).expect("mkdir sshd state");
        fs::create_dir_all(&root).expect("mkdir files");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("chmod files");

        let host_key = state.join("host_key");
        let client_key = state.join("client_key");
        keygen(&host_key);
        keygen(&client_key);
        let authorized = state.join("authorized_keys");
        fs::copy(client_key.with_extension("pub"), &authorized).expect("authorize client key");
        for secret in [&host_key, &client_key, &authorized] {
            fs::set_permissions(secret, fs::Permissions::from_mode(0o600)).expect("chmod key");
        }

        let user = current_user();
        let port = free_port();
        let config = state.join("sshd_config");
        fs::write(
            &config,
            format!(
                "ListenAddress 127.0.0.1\nPort {port}\nHostKey {host_key}\n\
                 AuthorizedKeysFile {authorized}\nPubkeyAuthentication yes\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 AuthenticationMethods publickey\nMaxAuthTries 20\nStrictModes no\n\
                 UsePAM no\nPidFile none\nLogLevel ERROR\nAllowUsers {user}\n\
                 Subsystem sftp {sftp} -d {root}\n",
                host_key = host_key.display(),
                authorized = authorized.display(),
                sftp = sftp_server.display(),
                root = root.display(),
            ),
        )
        .expect("write sshd_config");

        let child = Command::new(&sshd)
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", sshd.display()));
        let mut fixture = Self {
            child,
            port,
            user,
            known_hosts: state.join("known_hosts"),
            client_key,
            root,
            _dir: dir,
        };
        fixture.wait_ready();
        fixture
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll sshd") {
                panic!("sshd exited early: {status}");
            }
            if TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port)).is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "sshd did not start listening");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    async fn connect(&self) -> SshConnection {
        SshConnection::connect_as(
            "127.0.0.1",
            self.port,
            self.user.clone(),
            self.client_key.clone(),
            HostKeyPolicy::AutoAdd,
            CommandTimeout::from_secs(10),
            Some(self.known_hosts.clone()),
        )
        .await
        .expect("connect to the disposable sshd")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl Drop for RealSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn patterned(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + i / 251) % 251).expect("fits u8"))
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

fn leftover_stages(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".mtui-upload-"))
        .collect()
}

fn assert_refused(err: &HostError) {
    assert!(
        matches!(err, HostError::UploadRefused { .. }),
        "expected a refusal, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn fresh_upload_has_exact_bytes_and_mode_0770() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("upload.bin");
    let data = patterned(80 * 1024);

    conn.sftp_put_bytes(&data, &dest).await.expect("upload");

    assert_eq!(fs::read(&dest).expect("read"), data);
    assert_eq!(mode(&dest), 0o770);
    assert!(leftover_stages(&sshd.root).is_empty());
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn empty_upload_publishes_an_empty_file() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("empty.bin");

    conn.sftp_put_bytes(b"", &dest).await.expect("upload");

    assert_eq!(fs::metadata(&dest).expect("stat").len(), 0);
    assert_eq!(mode(&dest), 0o770);
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn existing_file_is_replaced() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("upload.bin");
    fs::write(&dest, b"OLD").expect("seed");

    conn.sftp_put_bytes(b"NEW", &dest).await.expect("upload");

    assert_eq!(fs::read(&dest).expect("read"), b"NEW");
    assert!(leftover_stages(&sshd.root).is_empty());
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn destination_symlink_is_replaced_and_its_target_left_alone() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let victim = sshd.path("victim");
    let dest = sshd.path("upload.bin");
    fs::write(&victim, SENTINEL).expect("seed victim");
    symlink(&victim, &dest).expect("plant symlink");

    conn.sftp_put_bytes(b"NEW", &dest).await.expect("upload");

    assert_eq!(fs::read(&victim).expect("read victim"), SENTINEL);
    let meta = fs::symlink_metadata(&dest).expect("stat dest");
    assert!(
        meta.file_type().is_file(),
        "destination must be a regular file"
    );
    assert_eq!(fs::read(&dest).expect("read dest"), b"NEW");
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn destination_hardlink_is_replaced_not_written_through() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let victim = sshd.path("victim");
    let dest = sshd.path("upload.bin");
    fs::write(&victim, SENTINEL).expect("seed victim");
    fs::hard_link(&victim, &dest).expect("plant hardlink");
    let victim_ino = fs::metadata(&victim).expect("stat victim").ino();

    conn.sftp_put_bytes(b"NEW", &dest).await.expect("upload");

    assert_eq!(fs::read(&victim).expect("read victim"), SENTINEL);
    assert_eq!(
        fs::metadata(&victim).expect("stat victim").ino(),
        victim_ino
    );
    assert_ne!(fs::metadata(&dest).expect("stat dest").ino(), victim_ino);
    assert_eq!(fs::read(&dest).expect("read dest"), b"NEW");
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn directory_destination_fails_and_stays_intact() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("upload.bin");
    fs::create_dir(&dest).expect("plant directory");
    fs::write(dest.join("keep"), b"keep").expect("seed child");

    let err = conn
        .sftp_put_bytes(b"NEW", &dest)
        .await
        .expect_err("a directory destination must fail");

    assert!(matches!(err, HostError::UploadFailed { .. }), "got {err:?}");
    assert_eq!(fs::read(dest.join("keep")).expect("read child"), b"keep");
    assert!(fs::metadata(&dest).expect("stat").is_dir());
    assert!(leftover_stages(&sshd.root).is_empty());
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn symlinked_parent_is_refused_and_outside_directory_untouched() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let outside = sshd.path("outside");
    fs::create_dir(&outside).expect("mkdir outside");
    fs::write(outside.join("sentinel"), SENTINEL).expect("seed sentinel");
    symlink(&outside, sshd.path("link")).expect("plant parent symlink");

    let err = conn
        .sftp_put_bytes(b"NEW", &sshd.path("link/upload.bin"))
        .await
        .expect_err("a symlinked parent must be refused");

    assert_refused(&err);
    assert_eq!(
        fs::read_dir(&outside).expect("list").count(),
        1,
        "nothing may land in the symlink's referent"
    );
    assert_eq!(fs::read(outside.join("sentinel")).expect("read"), SENTINEL);
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn group_and_world_writable_nonsticky_parent_is_refused_before_any_write() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    for (name, bits) in [("world", 0o777), ("group", 0o775)] {
        let parent = sshd.path(name);
        fs::create_dir(&parent).expect("mkdir parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(bits)).expect("chmod parent");

        let err = conn
            .sftp_put_bytes(b"NEW", &parent.join("upload.bin"))
            .await
            .expect_err("an unsafe parent must be refused");

        assert_refused(&err);
        assert_eq!(
            fs::read_dir(&parent).expect("list").count(),
            0,
            "{name}: refused before any stage or payload exists"
        );
    }
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn sticky_world_writable_parent_owned_by_the_uploader_is_accepted() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let parent = sshd.path("sticky");
    fs::create_dir(&parent).expect("mkdir parent");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).expect("chmod parent");

    conn.sftp_put_bytes(b"NEW", &parent.join("upload.bin"))
        .await
        .expect("sticky parent owned by the uploader is trusted");

    assert_eq!(fs::read(parent.join("upload.bin")).expect("read"), b"NEW");
    assert!(leftover_stages(&parent).is_empty());
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn missing_parents_are_created_without_group_or_world_write() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("a/b/upload.bin");

    conn.sftp_put_bytes(b"NEW", &dest).await.expect("upload");

    assert_eq!(fs::read(&dest).expect("read"), b"NEW");
    for dir in ["a", "a/b"] {
        assert_eq!(
            mode(&sshd.path(dir)) & 0o022,
            0,
            "{dir} must not be writable by others"
        );
    }
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn relative_destination_resolves_against_the_sftp_start_directory() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;

    conn.sftp_put_bytes(b"NEW", Path::new("rel/upload.bin"))
        .await
        .expect("upload");

    assert_eq!(fs::read(sshd.path("rel/upload.bin")).expect("read"), b"NEW");
}

#[tokio::test]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn parent_dot_dot_components_are_refused() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    fs::create_dir(sshd.path("sub")).expect("mkdir");

    let err = conn
        .sftp_put_bytes(b"NEW", &sshd.path("sub/../upload.bin"))
        .await
        .expect_err("`..` must be refused");

    assert_refused(&err);
    assert!(!sshd.path("upload.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real sshd: set MTUI_TEST_REAL_SFTP=1"]
async fn concurrent_reader_sees_only_whole_old_or_whole_new_content() {
    let sshd = RealSshd::start();
    let mut conn = sshd.connect().await;
    let dest = sshd.path("upload.bin");
    let old = vec![b'O'; 600 * 1024];
    let new = vec![b'N'; 600 * 1024];
    fs::write(&dest, &old).expect("seed");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = {
        let (dest, stop, old, new) = (dest.clone(), stop.clone(), old.clone(), new.clone());
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let got = fs::read(&dest).expect("final path must always be readable");
                assert!(
                    got == old || got == new,
                    "observed a partial file of {} bytes",
                    got.len()
                );
                seen.push(got == new);
            }
            seen
        })
    };

    conn.sftp_put_bytes(&new, &dest).await.expect("upload");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let seen = observer.join().expect("observer thread");

    assert!(!seen.is_empty());
    assert_eq!(fs::read(&dest).expect("read"), new);
}
