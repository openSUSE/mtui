"""Handles SSH and SFTP connections using paramiko.

This module provides the `Connection` class, which encapsulates the
functionality for running commands, transferring files, and opening
remote shells on a remote host.
"""

import codecs
import errno
import logging
import select
import stat
import sys
import termios
import tty
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from logging import getLogger
from pathlib import Path
from traceback import format_exc

import paramiko
from paramiko import Channel, SFTPClient, SFTPFile, SSHClient, SSHConfig

from ...cli.term import termsize
from ...support.messages import ReConnectFailed
from .timeout import CommandTimeoutError

logger = getLogger("mtui.connection")
RETRIES: int = 5


if not sys.warnoptions:
    import warnings

    # Suppress only paramiko/cryptography deprecation warnings, not all warnings
    warnings.filterwarnings("ignore", module="paramiko")
    warnings.filterwarnings("ignore", module="cryptography")


class Connection:
    """Manages SSH and SFTP connections to a remote host."""

    __slots__ = [
        "_interactive",
        "_policy",
        "_timeout_prompt",
        "client",
        "command",
        "hostname",
        "port",
        "stderr",
        "stdin",
        "stdout",
        "timeout",
    ]

    def __init__(
        self,
        hostname: str,
        port: int | str,
        timeout: int,
        missing_host_key_policy: paramiko.MissingHostKeyPolicy | None = None,
        timeout_prompt: Callable[[str], str] | None = None,
        interactive: bool = True,
    ) -> None:
        """Opens an SSH channel to the specified host.

        Authentication is SSH public-key only. If key auth fails the
        connection error propagates to the caller -- there is no password
        fallback (set up working SSH key auth to the target instead).

        Args:
            hostname: The hostname or IP address of the remote host.
            port: The port number to connect to.
            timeout: Timeout in seconds for both establishing the connection
                (TCP connect, SSH banner, auth) and remote command execution.
            missing_host_key_policy: paramiko policy applied to unknown
                host keys. ``None`` (the default) preserves the legacy
                behaviour of ``AutoAddPolicy``.
            timeout_prompt: Optional callable invoked with prompt text
                when a remote command times out. Returns the user's
                response (Enter / ``y`` to wait, ``n`` to abort). When
                ``None`` (the default) the timeout branch silently
                loops back to wait for the command — matching the
                long-standing Enter / Y default — and emits one WARNING
                log line so the silence is observable. Wire a
                :class:`mtui.cli.prompter.Prompter`'s ``ask`` method here
                to surface a serialised, race-free prompt to the user
                when multiple targets run a command in parallel.
            interactive: Whether a TTY-backed user is available to answer
                the command-timeout prompt raised by :meth:`run`. ``True``
                (the default) lets a timed-out command ask the user
                whether to keep waiting. ``False`` (e.g. under
                ``mtui-mcp``, which has no TTY) makes a silent command
                timeout abort the run instead of looping forever.

        """
        self._timeout_prompt = timeout_prompt
        self._interactive = interactive
        self.hostname = hostname

        try:
            self.port = int(port) if port else 22
        except ValueError:
            logger.warning(
                "invalid SSH port %r for %s; falling back to 22",
                port,
                hostname,
            )
            self.port = 22

        self.timeout = timeout

        self.client = SSHClient()
        self._policy = missing_host_key_policy

        self.load_keys()

        self.connect()

    def __repr__(self) -> str:
        return f"<{self.__class__.__name__} object hostname={self.hostname} port={self.port}>"

    def load_keys(self) -> None:
        """Loads system host keys and applies the missing-key policy."""
        self.client.load_system_host_keys()
        policy = self._policy if self._policy is not None else paramiko.AutoAddPolicy()
        self.client.set_missing_host_key_policy(policy)

    def connect(self, quiet: bool = False) -> None:
        """Connects to the remote host using paramiko.

        Args:
            quiet: When True, a failed connection (host unreachable) is
                logged at debug rather than error level. Used during
                reconnect retries -- e.g. while a host is rebooting -- where
                a refused/timed-out connection is expected and only the
                final give-up (a raised error) should surface to the user.

        """
        cfg = SSHConfig()
        try:
            with Path("~/.ssh/config").expanduser().open() as fd:
                cfg.parse(fd)
        except OSError as e:
            if e.errno != errno.ENOENT:
                logger.warning(e)
        opts = cfg.lookup(self.hostname)

        try:
            logger.debug("connecting to %s:%s", self.hostname, self.port)
            # if this fails, the user most likely has none or an outdated
            # hostkey for the specified host. checking back with a manual
            # "ssh root@..." invocation helps in most cases.
            self.client.connect(
                hostname=(
                    opts.get("hostname", self.hostname)
                    if "proxycommand" not in opts
                    else self.hostname
                ),
                port=int(opts.get("port", self.port)),
                username=opts.get("user", "root"),
                key_filename=opts.get("identityfile", None),
                sock=(
                    paramiko.ProxyCommand(opts["proxycommand"])
                    if "proxycommand" in opts
                    else None
                ),
                timeout=self.timeout,
                banner_timeout=self.timeout,
                auth_timeout=self.timeout,
            )

        except (paramiko.AuthenticationException, paramiko.BadHostKeyException):
            # Public-key authentication failed. There is no password
            # fallback: MTUI requires working SSH key auth to the target
            # (set it up and verify with "ssh root@<host>"). Re-raise so
            # the caller (Target.connect) reports the single user-facing
            # ConnectingTargetFailedMessage; keep the traceback at DEBUG
            # (visible with --debug) so only one line surfaces per failure.
            logger.warning(
                "Authentication failed on %s: SSH key authentication did not "
                'succeed. Set up working SSH key auth (verify with "ssh '
                'root@%s").',
                self.hostname,
                self.hostname,
            )
            logger.debug(
                "Authentication failure detail on %s",
                self.hostname,
                exc_info=True,
            )
            raise
        except paramiko.SSHException:
            # unspecified general SSHException. the host/sshd is probably not
            # available. As above: one-line error, traceback to DEBUG.
            logger.error("SSHException while connecting to %s", self.hostname)
            logger.debug("SSHException detail", exc_info=True)
            raise

        except OSError:
            if quiet:
                # Reconnect path (e.g. host rebooting): an unreachable host is
                # expected; swallow and let reconnect()'s is_active() check
                # decide when to give up.
                logger.debug(
                    "No valid connection to %s:%s (yet)", self.hostname, self.port
                )
            else:
                # Initial/explicit connect: re-raise so the caller
                # (Target.connect) reports ConnectingTargetFailedMessage instead
                # of proceeding with a dead transport and failing later in
                # is_locked()/parse_system() with an opaque error.
                logger.error("No valid connection to %s:%s", self.hostname, self.port)
                raise
        except Exception as e:
            # general Exception
            logger.debug("%s: %s", self.hostname, e)
            raise

    def reconnect(
        self, retry: int = 0, timeout: int = 10, backoff: bool = False
    ) -> None:
        """Tries to reconnect to the host.

        Args:
            retry: The number of times to retry the connection.
            timeout: The timeout for each connection attempt.
            backoff: Whether to use exponential backoff for retries.

        """
        count = 0
        rtimeout = timeout
        while not self.is_active() and count <= retry:
            count += 1
            logger.debug(
                "lost connection to %s:%s, reconnecting",
                self.hostname,
                self.port,
            )

            select.select([], [], [], rtimeout)
            if backoff:
                rtimeout = 2 * (timeout + 5 * count)
            # Retries are expected to fail while the host is still down
            # (e.g. mid-reboot); keep them quiet and let the final
            # ConnectionError below be the single surfaced failure.
            self.connect(quiet=True)

        if not self.is_active():
            raise ConnectionError(
                f"Failed to reconnect to {self.hostname}:{self.port} after {count} retries"
            )

    def fire_and_forget(self, command: str) -> None:
        """Dispatches a command without waiting for it to complete.

        Intended for commands that deliberately tear down the connection
        (e.g. a reboot): the command is sent on a new session and the
        local connection is then closed. No output or exit status is
        collected and a dropped link is expected -- callers should follow
        up with :meth:`reconnect`. This avoids the normal :meth:`run`
        recovery path, which would otherwise try (and fail) to reconnect
        to the still-rebooting host.

        Args:
            command: The command to dispatch.

        """
        logger.debug(
            "%s: dispatching fire-and-forget command: %s", self.hostname, command
        )
        self.__run_command(command)
        self.close()

    def new_session(self) -> Channel | None:
        """Opens a new session on the channel.

        All remote commands are run on a separate session to make sure
        that leftovers from the previous command do not interfere with
        the current command.

        Returns:
            A new session object, or None if a session could not be opened.

        """
        logger.debug("creating new session at %s:%s", self.hostname, self.port)
        try:
            if transport := self.client.get_transport():
                transport.set_keepalive(30)
            else:
                return None
            try:
                # add NullHandler to paramiko to get rid of
                # "paramiko: logging handler not found" messages
                sshlog = logging.getLogger(transport.get_log_channel())
                sshlog.addHandler(logging.NullHandler())
            except Exception:
                logger.debug(
                    "failed to attach NullHandler to paramiko log channel",
                    exc_info=True,
                )
            session = transport.open_session()

            # disable blocking and timeout to use the session in async mode
            session.setblocking(0)
            session.settimeout(0)
        except Exception:
            logger.debug(
                "failed to open new session on %s:%s",
                self.hostname,
                self.port,
                exc_info=True,
            )
            session = None

        return session

    @staticmethod
    def close_session(session: Channel | None = None) -> None:
        """Closes the current session.

        Args:
            session: The session to close.

        """
        if session:
            try:
                session.shutdown(2)
                session.close()
            except Exception:
                # session is already closed or broken; nothing to do beyond
                # leaving a debug breadcrumb for diagnosis.
                logger.debug(
                    "ignoring error while closing already-broken session",
                    exc_info=True,
                )

    def __run_command(self, command: str) -> Channel | None:
        """Opens a new session and runs a command in it.

        Args:
            command: The command to run.

        Returns:
            A session instance with the running command, or None on failure.

        """
        try:
            if session := self.new_session():
                session.exec_command(command)
                # run() never feeds stdin to the remote command, so close the
                # write half of the channel right away. This sends EOF to the
                # command's stdin: one that reads input (an interactive prompt,
                # `read`, `cat`, ...) gets EOF and proceeds/aborts instead of
                # blocking forever waiting for input that will never arrive.
                try:
                    session.shutdown_write()
                except Exception:
                    logger.debug(
                        "shutdown_write failed on %s; continuing",
                        self.hostname,
                        exc_info=True,
                    )
            else:
                return None
        except (paramiko.ChannelException, paramiko.SSHException):
            if "session" in locals() and isinstance(session, Channel):
                self.close_session(session)
            return None
        return session

    def run(self, command: str, lock=None) -> int:
        """Runs a command over the SSH channel.

        This method blocks until the command terminates and returns the
        exit code of the command. In case of errors, -1 is returned.

        Args:
            command: The command to run.
            lock: A lock object for writing to stdout.

        Returns:
            The exit code of the command.

        """
        self.stdin = command
        self.stdout = ""
        self.stderr = ""
        stdout = b""
        stderr = b""

        session = self.__run_command(command)
        counter = 0
        while not session:
            if counter == RETRIES:
                raise ReConnectFailed(self.hostname)

            self.reconnect()
            session = self.__run_command(command)
            counter += 1

        try:
            while True:
                buffer = b""

                # wait for data to be transmitted. if the timeout is hit,
                # ask the user on how to procceed
                if select.select([session], [], [], self.timeout) == ([], [], []):
                    if not session:
                        raise ConnectionError(
                            f"Session lost during command execution on {self.hostname}"
                        )

                    # Non-interactive (e.g. mtui-mcp): there is no human to ask
                    # whether to keep waiting, so a command that produced no output
                    # for the whole timeout window is treated as stuck and aborted,
                    # rather than looping forever (which previously wedged the run
                    # until the session was killed). The window is
                    # ``connection_timeout`` (default 300s); raise it in config for
                    # legitimately long, fully silent commands.
                    if not self._interactive:
                        logger.warning(
                            'command "%s" timed out on %s after %ss with no output; '
                            "aborting (non-interactive session)",
                            command,
                            self.hostname,
                            self.timeout,
                        )
                        raise CommandTimeoutError

                    # No prompt callback wired: silently wait. Matches the
                    # long-standing Enter / Y default but emits a WARNING
                    # so the silence is observable in logs.
                    if self._timeout_prompt is None:
                        logger.warning(
                            'command "%s" timed out on %s; no prompt callback wired, '
                            "waiting for completion",
                            command,
                            self.hostname,
                        )
                        continue

                    # Prompt callback wired: ask the user. The callback is
                    # responsible for any cross-thread serialisation (the
                    # production wiring uses ``Prompter.ask`` which holds a
                    # single lock so workers don't race for stdin).
                    answer = self._timeout_prompt(
                        f'command "{command}" timed out on {self.hostname}. wait? (Y/n) ',
                    )
                    if answer.lower() not in ("no", "n", "ne", "nein"):
                        continue
                    # If the user don't want to wait, raise CommandTimeoutError
                    # and procceed.
                    raise CommandTimeoutError

                try:
                    # wait for data on the session's stdout/stderr. if debug is enabled,
                    # print the received data
                    if session.recv_ready():
                        buffer = session.recv(1024)
                        stdout += buffer
                        for line in buffer.decode("utf-8", "ignore").split("\n"):
                            if line:
                                logger.debug(line)

                    if session.recv_stderr_ready():
                        buffer = session.recv_stderr(1024)
                        stderr += buffer
                        for line in buffer.decode("utf-8", "ignore").split("\n"):
                            if line:
                                logger.debug(line)

                    if not buffer:
                        break

                except TimeoutError:
                    select.select([], [], [], 1)
        except BaseException:
            # Abandoning the command must not leak its channel: without
            # this close the Channel stays registered on the paramiko
            # Transport (never reclaimed by GC) and the remote command
            # keeps running -- repeated timeouts accumulated orphaned
            # channels and remote processes until the connection died.
            # BaseException, not just CommandTimeoutError: the wrapped
            # region includes the interactive timeout prompt, so Ctrl-D
            # (EOFError) or Ctrl-C (KeyboardInterrupt) at the prompt
            # abandons the command the same way and must clean up too.
            self.close_session(session)
            raise
        # save the exitcode of the last command and return it
        exitcode = session.recv_exit_status()

        self.close_session(session)
        # "replace", not strict: command output is arbitrary bytes (binary
        # dumps, non-UTF-8 locales, odd rpm metadata). A strict decode would
        # raise here and turn a successful command into a phantom failure
        # with its output lost -- mirror the tolerant per-line decode above.
        self.stdout = stdout.decode("utf-8", "replace")
        self.stderr = stderr.decode("utf-8", "replace")
        return exitcode

    def __invoke_shell(self, width: int, height: int) -> Channel | None:
        """Invokes a shell on the remote host.

        Args:
            width: The width of the terminal.
            height: The height of the terminal.

        Returns:
            A session with an open shell, or None on failure.

        """
        try:
            if session := self.new_session():
                session.get_pty("xterm", width, height)
                session.invoke_shell()
            else:
                return None
        except (paramiko.ChannelException, paramiko.SSHException):
            if "session" in locals() and isinstance(session, Channel):
                self.close_session(session)
            return None

        return session

    def shell(self) -> None:
        """Spawns a root shell on the target host."""
        oldtty = termios.tcgetattr(sys.stdin)

        width, height = termsize()

        session = self.__invoke_shell(width, height)
        while not session:
            self.reconnect()
            session = self.__invoke_shell(width, height)

        try:
            tty.setraw(sys.stdin.fileno())
            tty.setcbreak(sys.stdin.fileno())

            # The PTY stream arrives in arbitrary 1024-byte chunks, so a
            # plain per-chunk strict decode would raise on any non-UTF-8
            # byte -- and even on a valid multibyte character straddling a
            # chunk boundary -- killing the interactive shell mid-use. The
            # incremental decoder buffers partial sequences across chunks
            # and replaces genuinely invalid bytes instead.
            decoder = codecs.getincrementaldecoder("utf-8")("replace")
            while True:
                r, _, _ = select.select([session, sys.stdin], [], [])
                if session in r:
                    try:
                        x = session.recv(1024)
                        if len(x) == 0:
                            break
                        sys.stdout.write(decoder.decode(x))
                        sys.stdout.flush()
                    except TimeoutError:
                        pass
                if sys.stdin in r:
                    y: str = sys.stdin.read(1)
                    if len(y) == 0:
                        break
                    session.send(y.encode())

        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, oldtty)

        self.close_session(session)

    def __sftp_open(self) -> SFTPClient | None:
        """Opens an SFTP session.

        Returns:
            An SFTP client object, or None on failure.

        """
        try:
            sftp = self.client.open_sftp()
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            if "sftp" in locals() and isinstance(sftp, SFTPClient):
                sftp.close()
            return None
        return sftp

    def __sftp_reconnect(self) -> SFTPClient:
        """Reconnects the SFTP session if it's not active.

        Returns:
            An active SFTP client object.

        """
        sftp = self.__sftp_open()
        counter = 0
        while not sftp:
            if counter == RETRIES:
                raise ReConnectFailed(self.hostname)
            self.reconnect()
            sftp = self.__sftp_open()
            counter += 1
        return sftp

    @contextmanager
    def _sftp(self) -> Iterator[SFTPClient]:
        """Open an SFTP client, yield it for one or more operations, then close.

        Centralises the open + close lifecycle so every public ``sftp_*``
        method (and external callers via :meth:`sftp_session`) reuses a
        single client across the with-block. Mid-block paramiko errors
        propagate to the caller; reconnect happens once at entry.
        """
        sftp = self.__sftp_reconnect()
        try:
            yield sftp
        finally:
            sftp.close()

    @contextmanager
    def sftp_session(self) -> Iterator[SFTPClient]:
        """Public CM for batching multi-step SFTP ops against this host.

        Yields a live :class:`paramiko.SFTPClient`. Use this when the
        caller wants to perform several reads/writes against the same
        host without paying the per-op handshake cost charged by the
        individual ``sftp_*`` helpers. Mid-session paramiko errors
        propagate out of the with-block; this CM does not auto-retry.
        """
        with self._sftp() as sftp:
            yield sftp

    def sftp_put(self, local: Path, remote: Path) -> None:
        """Transfers a file to the remote host over SFTP.

        The file is made executable after transfer.

        Args:
            local: The local file to transfer.
            remote: The remote path to transfer the file to.

        """
        with self._sftp() as sftp:
            path = ""
            # create remote base directory and copy the file to that directory.
            # Paramiko transport errors propagate per the ``_sftp`` contract;
            # don't rebind ``sftp`` mid-loop or the CM cannot close the
            # replacement client. ``OSError`` covers the common "directory
            # already exists" case (EEXIST) and is the only exception we
            # intentionally swallow.
            for subdir in str(remote).split("/")[:-1]:
                path += subdir + "/"
                try:
                    sftp.mkdir(path)
                except OSError:
                    # Most commonly: directory already exists. Treat as
                    # success but leave a breadcrumb so unexpected failures
                    # are diagnosable.
                    logger.debug(
                        "sftp mkdir %s treated as success",
                        path,
                        exc_info=True,
                    )

            logger.debug(
                "transmitting %s to %s:%s:%s",
                local,
                self.hostname,
                self.port,
                remote,
            )
            # paramiko isn't prepared for proper pathlib objects
            sftp.put(str(local), str(remote))

            # make file executable since it's probably a script which needs
            # to be run
            sftp.chmod(str(remote), stat.S_IRWXG | stat.S_IRWXU)

    def sftp_get(self, remote: Path, local: Path) -> None:
        """Transfers a file from the remote host to the local host.

        Args:
            remote: The remote file to transfer.
            local: The local path to transfer the file to.

        """
        with self._sftp() as sftp:
            logger.debug(
                "transmitting %s:%s:%s to %s",
                self.hostname,
                self.port,
                remote,
                local,
            )
            sftp.get(str(remote), local)

    # Similar to 'get' but handles folders.
    def sftp_get_folder(self, remote: Path, local: Path) -> None:
        """Transfers a folder from the remote host to the local host.

        Args:
            remote: The remote folder to transfer.
            local: The local path to transfer the folder to.

        """
        with self._sftp() as sftp:
            logger.debug(
                "transmitting %s:%s:%s to %s",
                self.hostname,
                self.port,
                remote,
                local,
            )
            # listdir directly on the live client so the whole batch
            # (1 listdir + N gets) shares one SFTP session; previously
            # listdir went through self.sftp_listdir, opening a second
            # client.
            files = sftp.listdir(str(remote))
            for file in files:
                sftp.get(
                    f"{remote}/{file}",
                    f"{local}{file}.{self.hostname}",
                )

    def sftp_listdir(self, path: Path = Path()) -> list[str]:
        """Gets a directory listing of the remote host.

        Args:
            path: The remote directory path to list.

        Returns:
            A list of filenames in the directory.

        """
        logger.debug(
            "getting %s:%s:%s listing",
            self.hostname,
            self.port,
            path,
        )
        with self._sftp() as sftp:
            return sftp.listdir(str(path))

    def sftp_open(self, filename: Path, mode: str = "r", bufsize=-1) -> SFTPFile:
        """Opens a remote file for reading.

        Args:
            filename: The remote file to open.
            mode: The mode to open the file in.
            bufsize: The buffer size.

        Returns:
            An SFTPFile object.

        """
        # C7: keep manual SFTPClient lifetime here. The returned
        # ``SFTPFile`` holds a strong reference to the SFTP channel, so
        # the file stays usable after the client object is GC'd. Routing
        # this through the ``_sftp`` context manager would call
        # ``client.close()`` on exit and break that behaviour.
        logger.debug("%s open(%s, %s)", repr(self), filename, mode)
        logger.debug("  -> self.client.open_sftp")
        sftp = self.__sftp_reconnect()
        logger.debug("  -> sftp.open")

        try:
            ofile = sftp.open(str(filename), mode, bufsize)
        except BaseException:
            # It often happens to me lately that mtui seems to freeze at
            # doing sftp.open() so let's log any other exception here,
            # just in case it gets eaten by some caller in mtui
            # bnc#880934
            logger.debug(format_exc())
            if "sftp" in locals() and isinstance(sftp, SFTPClient):
                sftp.close()
            raise

        return ofile

    def sftp_remove(self, path: Path) -> None:
        """Deletes a remote file.

        Args:
            path: The path to the remote file to delete.

        """
        logger.debug("deleting file %s:%s:%s", self.hostname, self.port, path)
        try:
            with self._sftp() as sftp:
                sftp.remove(str(path))
        except OSError:
            logger.exception("Can't remove %s from %s", path, self.hostname)

    def sftp_rmdir(self, path: Path) -> None:
        """Deletes a remote directory.

        Args:
            path: The path to the remote directory to delete.

        """
        logger.debug("deleting dir %s:%s:%s", self.hostname, self.port, path)
        with self._sftp() as sftp:
            # listdir + per-file remove + final rmdir all share one
            # SFTP session; previously each child op opened its own
            # client via self.sftp_listdir / self.sftp_remove.
            items = sftp.listdir(str(path))
            for item in items:
                filename = path / item
                try:
                    sftp.remove(str(filename))
                except OSError:
                    logger.exception("Can't remove %s from %s", filename, self.hostname)
            sftp.rmdir(str(path))

    def sftp_readlink(self, path: Path) -> str | None:
        """Returns the target of a symbolic link.

        Args:
            path: The path to the symbolic link.

        Returns:
            The target of the symbolic link.

        """
        logger.debug("read link %s:%s:%s", self.hostname, self.port, path)
        with self._sftp() as sftp:
            return sftp.readlink(str(path))

    def is_active(self) -> bool:
        """Checks if the connection is active.

        Returns:
            True if the connection is active, False otherwise.

        """
        transport = self.client._transport  # noqa: SLF001
        if transport is None:
            return False
        return transport.is_active()

    def close(self) -> None:
        """Closes the SSH channel and disconnects."""
        logger.debug("closing connection to %s:%s", self.hostname, self.port)
        self.client.close()
