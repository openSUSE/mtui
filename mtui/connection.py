"""Handles SSH and SFTP connections using paramiko.

This module provides the `Connection` class, which encapsulates the
functionality for running commands, transferring files, and opening
remote shells on a remote host.
"""

import errno
import getpass
import logging
import select
import socket
import stat
import sys
import termios
import tty
from logging import getLogger
from pathlib import Path
from traceback import format_exc

import paramiko
from paramiko import Channel, SFTPClient, SFTPFile, SSHClient, SSHConfig

from .messages import ReConnectFailed
from .utils import termsize

logger = getLogger("mtui.connection")
RETRIES: int = 5


if not sys.warnoptions:
    import warnings

    warnings.simplefilter("ignore")


class CommandTimeout(Exception):
    """Exception raised when a remote command times out."""

    def __init__(self, command=None) -> None:
        """Initializes the exception.

        Args:
            command: The command that timed out.
        """
        self.command = command

    def __str__(self) -> str:
        """Returns the timed out remote command as a string."""
        return repr(self.command)


class Connection:
    """Manages SSH and SFTP connections to a remote host."""

    __slots__ = [
        "client",
        "command",
        "hostname",
        "port",
        "stderr",
        "stdin",
        "stdout",
        "timeout",
    ]

    def __init__(self, hostname: str, port: int | str, timeout: int) -> None:
        """Opens an SSH channel to the specified host.

        This method tries to authenticate using SSH keys and falls back to
        password authentication if key-based authentication fails.

        Args:
            hostname: The hostname or IP address of the remote host.
            port: The port number to connect to.
            timeout: The timeout for remote commands.
        """
        # uncomment to enable separate paramiko connection logging

        # paramiko.util.log_to_file("/tmp/paramiko.log")

        self.hostname = hostname

        try:
            self.port = int(port)
        except ValueError:
            self.port = 22

        self.timeout = timeout

        self.client = SSHClient()

        self.load_keys()

        # uncomment to combine stderr and stdout channel. In most cases,
        # mtui expects a separate stderr channel. Changing this may be
        # harmfull to error checking code.
        # self.client.set_combine_stderr(True)

        self.connect()

    def __repr__(self) -> str:
        return f"<{self.__class__.__name__} object hostname={self.hostname} port={self.port}>"

    def load_keys(self) -> None:
        """Loads system host keys."""
        self.client.load_system_host_keys()
        self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

    def connect(self) -> None:
        """Connects to the remote host using paramiko."""
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
            )

        except (paramiko.AuthenticationException, paramiko.BadHostKeyException):
            # if public key auth fails, fallback to a password prompt.
            # other than ssh, mtui asks only once for a password. this could
            # be changed if there is demand for it.
            logger.warning(
                "Authentication failed on %s: AuthKey missing. Make sure your system is set up correctly",
                self.hostname,
            )
            logger.warning("Trying manually, please enter the root password")
            password = getpass.getpass()

            try:
                # try again with password auth instead of public/private key
                self.client.connect(
                    hostname=(
                        opts.get("hostname", self.hostname)
                        if "proxycommand" not in opts
                        else self.hostname
                    ),
                    port=int(opts.get("port", self.port)),
                    username=opts.get("user", "root"),
                    password=password,
                    sock=(
                        paramiko.ProxyCommand(opts["proxycommand"])
                        if "proxycommand" in opts
                        else None
                    ),
                )
            except paramiko.AuthenticationException:
                # if a wrong password was set, don't connect to the host and
                # reraise the exception hoping it's catched somewhere in an
                # upper layer.
                logger.exception(
                    "Authentication failed on %s: wrong password", self.hostname
                )
                raise
        except paramiko.SSHException:
            # unspecified general SSHException. the host/sshd is probably not
            # available.
            logger.exception("SSHException while connecting to %s", self.hostname)
            raise

        except OSError:
            logger.error("No valid connection to %s:%s", self.hostname, self.port)
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
            self.connect()

        assert self.is_active()

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
            except BaseException:
                pass
            session = transport.open_session()

            # disable blocking and timeout to use the session in async mode
            session.setblocking(0)
            session.settimeout(0)
        except Exception:
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
            except BaseException:
                # pass all exceptions since the session is already closed or broken
                pass

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

        while True:
            buffer = b""

            # wait for data to be transmitted. if the timeout is hit,
            # ask the user on how to procceed
            if select.select([session], [], [], self.timeout) == ([], [], []):
                assert session

                # writing on stdout needs locking as all run threads could
                # write at the same time to stdout
                if lock:
                    lock.acquire()

                try:
                    if input(
                        f'command "{command}" timed out on {self.hostname}. wait? (Y/n) ',
                    ).lower() not in ("no", "n", "ne", "nein"):
                        continue
                    # if the user don't want to wait, raise CommandTimeout
                    # and procceed
                    raise CommandTimeout
                finally:
                    # release lock to allow other command threads to write to
                    # stdout
                    if lock:
                        lock.release()

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

            except socket.timeout:
                select.select([], [], [], 1)
        # save the exitcode of the last command and return it
        exitcode = session.recv_exit_status()

        self.close_session(session)
        self.stdout = stdout.decode("utf-8")
        self.stderr = stderr.decode("utf-8")
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

            while True:
                r, _, _ = select.select([session, sys.stdin], [], [])
                if session in r:
                    try:
                        x = session.recv(1024)
                        if len(x) == 0:
                            break
                        sys.stdout.write(x.decode())
                        sys.stdout.flush()
                    except socket.timeout:
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

    def sftp_put(self, local: Path, remote: Path) -> None:
        """Transfers a file to the remote host over SFTP.

        The file is made executable after transfer.

        Args:
            local: The local file to transfer.
            remote: The remote path to transfer the file to.
        """

        path = ""
        sftp = self.__sftp_reconnect()

        # create remote base directory and copy the file to that directory
        for subdir in str(remote).split("/")[:-1]:
            path += subdir + "/"
            created = False
            while not created:
                try:
                    sftp.mkdir(path)
                    created = True
                except (
                    AttributeError,
                    paramiko.ChannelException,
                    paramiko.SSHException,
                ):
                    created = False
                    sftp = self.__sftp_reconnect()
                except Exception:
                    created = True

        logger.debug(
            "transmitting %s to %s:%s:%s",
            local,
            self.hostname,
            self.port,
            remote,
        )
        # paramiko isn't prepared for proper pathlib objects
        sftp.put(str(local), str(remote))

        # make file executable since it's probably a script which needs to be
        # run
        sftp.chmod(str(remote), stat.S_IRWXG | stat.S_IRWXU)

        sftp.close()

    def sftp_get(self, remote: Path, local: Path) -> None:
        """Transfers a file from the remote host to the local host.

        Args:
            remote: The remote file to transfer.
            local: The local path to transfer the file to.
        """
        sftp = self.__sftp_reconnect()

        logger.debug(
            "transmitting %s:%s:%s to %s",
            self.hostname,
            self.port,
            remote,
            local,
        )
        sftp.get(str(remote), local)

        sftp.close()

    # Similar to 'get' but handles folders.
    def sftp_get_folder(self, remote: Path, local: Path) -> None:
        """Transfers a folder from the remote host to the local host.

        Args:
            remote: The remote folder to transfer.
            local: The local path to transfer the folder to.
        """
        sftp = self.__sftp_reconnect()
        logger.debug(
            "transmitting %s:%s:%s to %s",
            self.hostname,
            self.port,
            remote,
            local,
        )
        files = self.sftp_listdir(remote)
        for file in files:
            sftp.get(
                f"{remote}/{file}",
                f"{local}{file}.{self.hostname}",
            )

        sftp.close()

    def sftp_listdir(self, path: Path = Path(".")) -> list[str]:
        """Gets a directory listing of the remote host.

        Args:
            path: The remote directory path to list.

        Returns:
            A list of filenames in the directory.
        """
        logger.debug(
            f"getting {self.hostname!s}:{self.port!s}:{path!s} listing",
        )
        sftp = self.__sftp_reconnect()

        listdir = sftp.listdir(str(path))
        sftp.close()
        return listdir

    def sftp_open(self, filename: Path, mode: str = "r", bufsize=-1) -> SFTPFile:
        """Opens a remote file for reading.

        Args:
            filename: The remote file to open.
            mode: The mode to open the file in.
            bufsize: The buffer size.

        Returns:
            An SFTPFile object.
        """
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
        sftp = self.__sftp_reconnect()

        try:
            sftp.remove(str(path))
        except IOError:
            logger.exception("Can't remove %s from %s", path, self.hostname)

        sftp.close()

    def sftp_rmdir(self, path: Path) -> None:
        """Deletes a remote directory.

        Args:
            path: The path to the remote directory to delete.
        """
        logger.debug("deleting dir %s:%s:%s", self.hostname, self.port, path)
        sftp = self.__sftp_reconnect()
        items = self.sftp_listdir(path)

        for item in items:
            filename = path / item
            self.sftp_remove(filename)

        sftp.rmdir(str(path))
        sftp.close()

    def sftp_readlink(self, path: Path) -> str | None:
        """Returns the target of a symbolic link.

        Args:
            path: The path to the symbolic link.

        Returns:
            The target of the symbolic link.
        """
        logger.debug("read link %s:%s:%s", self.hostname, self.port, path)
        sftp = self.__sftp_reconnect()
        link = sftp.readlink(str(path))
        sftp.close()
        return link

    def is_active(self) -> bool:
        """Checks if the connection is active.

        Returns:
            True if the connection is active, False otherwise.
        """
        return self.client._transport.is_active()  # type: ignore

    def close(self) -> None:
        """Closes the SSH channel and disconnects."""
        logger.debug("closing connection to %s:%s", self.hostname, self.port)
        self.client.close()
