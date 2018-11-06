# -*- coding: utf-8 -*-
#
# mtui ssh connection handling using paramiko.
# almost all exceptions here are passed to the upper layer.
#

import sys
import stat
import errno
import select
import socket
import termios
import tty
import getpass
import logging
from pathlib import Path
from logging import getLogger
from traceback import format_exc

from mtui.utils import termsize

import paramiko

logger = getLogger("mtui.connection")


class CommandTimeout(Exception):

    """remote command timeout exception

    returns timed out remote command as __str__

    """

    def __init__(self, command=None):
        self.command = command

    def __str__(self):
        return repr(self.command)


class Connection(object):

    """manage SSH and SFTP connections"""

    def __init__(self, hostname, port, timeout):
        """opens SSH channel to specified host

        Tries AuthKey Authentication and falls back to password mode in case of errors.
        If a connection can't be established (host not available, wrong password/key)
        exceptions are reraised from the ssh subsystem and need to be catched
        by the caller.

        Keyword arguments:
        hostname -- host address to connect to
        timeout  -- remote command timeout on this connection

        """

        # uncomment to enable separate paramiko connection logging

        # paramiko.util.log_to_file("/tmp/paramiko.log")

        self.hostname = hostname

        try:
            self.port = int(port)
        except Exception:
            self.port = 22

        self.timeout = timeout

        self.client = paramiko.SSHClient()

        self.load_keys()

        # uncomment to combine stderr and stdout channel. In most cases,
        # mtui expects a separate stderr channel. Changing this may be
        # harmfull to error checking code.

        # self.client.set_combine_stderr(True)
        self.connect()

    def __repr__(self):
        return "<{0} object hostname={1} port={2}>".format(
            self.__class__.__name__, self.hostname, self.port
        )

    def load_keys(self):
        self.client.load_system_host_keys()
        self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

    def connect(self):
        """connect to the remote host using paramiko as ssh subsystem"""
        cfg = paramiko.config.SSHConfig()
        try:
            with Path("~/.ssh/config").expanduser().open() as fd:
                cfg.parse(fd)
        except IOError as e:
            if e.errno != errno.ENOENT:
                logger.warning(e)
        opts = cfg.lookup(self.hostname)

        try:
            logger.debug("connecting to {!s}:{!s}".format(self.hostname, self.port))
            # if this fails, the user most likely has none or an outdated
            # hostkey for the specified host. checking back with a manual
            # "ssh root@..." invocation helps in most cases.
            self.client.connect(
                hostname=opts.get("hostname", self.hostname)
                if "proxycommand" not in opts
                else self.hostname,
                port=int(opts.get("port", self.port)),
                username=opts.get("user", "root"),
                key_filename=opts.get("identityfile", None),
                sock=paramiko.ProxyCommand(opts["proxycommand"])
                if "proxycommand" in opts
                else None,
            )

        except (paramiko.AuthenticationException, paramiko.BadHostKeyException):
            # if public key auth fails, fallback to a password prompt.
            # other than ssh, mtui asks only once for a password. this could
            # be changed if there is demand for it.
            logger.warning(
                "Authentication failed on {!s}: AuthKey missing. Make sure your system is set up correctly".format(
                    self.hostname
                )
            )
            logger.warning("Trying manually, please enter the root password")
            password = getpass.getpass()

            try:
                # try again with password auth instead of public/private key
                self.client.connect(
                    hostname=opts.get("hostname", self.hostname)
                    if "proxycommand" not in opts
                    else self.hostname,
                    port=int(opts.get("port", self.port)),
                    username=opts.get("user", "root"),
                    password=password,
                    sock=paramiko.ProxyCommand(opts["proxycommand"])
                    if "proxycommand" in opts
                    else None,
                )
            except paramiko.AuthenticationException:
                # if a wrong password was set, don't connect to the host and
                # reraise the exception hoping it's catched somewhere in an
                # upper layer.
                logger.error(
                    "Authentication failed on {!s}: wrong password".format(
                        self.hostname
                    )
                )
                raise
        except paramiko.SSHException:
            # unspecified general SSHException. the host/sshd is probably not
            # available.
            logger.error("SSHException while connecting to {!s}".format(self.hostname))
            raise
        except Exception as error:
            # general Exception
            logger.debug("{!s}: {!s}".format(self.hostname, error))
            raise

    def reconnect(self):
        """try to reconnect to the host

        currently, there's no reconnection limit. needs to be implemented
        since the current implementation could deadlock.

        """

        if not self.is_active():
            logger.debug(
                "lost connection to {!s}:{!s}, reconnecting".format(
                    self.hostname, self.port
                )
            )

            # wait 10s and try to reconnect
            select.select([], [], [], 10)
            self.connect()

        assert self.is_active()

    def new_session(self):
        """open new session on the channel

        all remote commands are run on a seperate session to make sure
        that leftovers/session errors from the previous command do not
        interfere with the current command.


        session = self.new_session()
        session.exec_command(command)
        self.close_session(session)
        """

        logger.debug(
            "creating new session at {!s}:{!s}".format(self.hostname, self.port)
        )
        try:
            transport = self.client.get_transport()

            transport.set_keepalive(30)
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
    def close_session(session=None):
        """close the current session"""
        if session:
            try:
                session.shutdown(2)
                session.close()
            except BaseException:
                # pass all exceptions since the session is already closed or broken
                pass

    def __run_command(self, command):
        """ open new session and run command in it

        parameter: command -> str
        result: Succes - session instance with running command
                Fail - False
        """

        try:
            session = self.new_session()
            session.exec_command(command)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            if "session" in locals():
                if isinstance(session, paramiko.channel.Channel):
                    self.close_session(session)
            return False
        return session

    def run(self, command, lock=None):
        """run command over SSH channel

        Blocks until command terminates. returncode of issued command is returned.
        In case of errors, -1 is returned.

        If the connection hits the timeout limit, the user is asked to wait or
        cancel the current command.

        Keyword arguments:
        command -- the command to run
        lock    -- lock object for write on stdout
        """

        self.stdin = command
        self.stdout = ""
        self.stderr = ""
        stdout = b""
        stderr = b""

        session = self.__run_command(command)

        while not session:
            self.reconnect()
            session = self.__run_command(command)

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
                        'command "{}" timed out on {}. wait? (Y/n) '.format(
                            command, self.hostname
                        )
                    ).lower() not in ("no", "n", "ne", "nein"):
                        continue
                    else:
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

    def __invoke_shell(self, width, height):
        """
        params: widh
        params: height
        returns: session with open shell on pass else False
        """

        try:
            session = self.new_session()
            session.get_pty("xterm", width, height)
            session.invoke_shell()
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            if "session" in locals():
                if isinstance(session, paramiko.channel.Channel):
                    self.close_session(session)
            return False

        return session

    def shell(self):
        """invoke remote shell

        Spawns a root shell on the target host.
        TTY attributes are re-set after leaving the remote shell.

        Keyword arguments:
        None

        """
        oldtty = termios.tcgetattr(sys.stdin)

        session = self.new_session()
        width, height = termsize()

        session = self.__invoke_shell(width, height)
        while not session:
            self.reconnect()
            session = self.__invoke_shell(width, height)

        try:
            tty.setraw(sys.stdin.fileno())
            tty.setcbreak(sys.stdin.fileno())

            while True:
                r, w, e = select.select([session, sys.stdin], [], [])
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
                    x = sys.stdin.read(1)
                    if len(x) == 0:
                        break
                    session.send(x)

        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, oldtty)

        self.close_session(session)

    def __sftp_open(self):
        try:
            sftp = self.client.open_sftp()
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            if "sftp" in locals():
                if isinstance(sftp, paramiko.sftp_client.SFTPClient):
                    sftp.close()
            return False
        return sftp

    def __sftp_reconnect(self):
        sftp = self.__sftp_open()
        while not sftp:
            self.reconnect()
            sftp = self.__sftp_open()
        return sftp

    def put(self, local, remote):
        """transfers a file to the remote host over SFTP

        File is made executable

        Keyword arguments:
        local  -- local file name
        remote -- remote file name

        """
        remote = str(remote)
        local = str(local)

        path = ""
        sftp = self.__sftp_reconnect()

        # create remote base directory and copy the file to that directory
        for subdir in remote.split("/")[:-1]:
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
            "transmitting {!s} to {!s}:{!s}:{!s}".format(
                local, self.hostname, self.port, remote
            )
        )
        sftp.put(local, remote)

        # make file executable since it's probably a script which needs to be
        # run
        sftp.chmod(remote, stat.S_IRWXG | stat.S_IRWXU)

        sftp.close()

    def get(self, remote, local):
        """transfers file from the remote host to the local host over SFTP

        local base directory needs to exist

        Keyword arguments:
        remote -- remote file name
        local  -- local file name

        """
        remote = str(remote)
        local = str(local)
        sftp = self.__sftp_reconnect()

        logger.debug(
            "transmitting {!s}:{!s}:{!s} to {!s}".format(
                self.hostname, self.port, remote, local
            )
        )
        sftp.get(remote, local)

        sftp.close()

    # Similar to 'get' but handles folders.
    def get_folder(self, remote_folder, local_folder):

        remote_folder = str(remote_folder)
        local_folder = str(local_folder)
        sftp = self.__sftp_reconnect()
        logger.debug(
            "transmitting {!s}:{!s}:{!s} to {!s}".format(
                self.hostname, self.port, remote_folder, local_folder
            )
        )
        files = self.listdir(remote_folder)
        for f in files:
            sftp.get(
                "{!s}/{!s}".format(remote_folder, f),
                "{!s}{!s}.{!s}".format(local_folder, f, self.hostname),
            )

        sftp.close()

    def listdir(self, path="."):
        """get directory listing of the remote host

        Keyword arguments:
        path   -- remote directory path to list

        """

        path = str(path)
        logger.debug(
            "getting {!s}:{!s}:{!s} listing".format(self.hostname, self.port, path)
        )
        sftp = self.__sftp_reconnect()

        listdir = sftp.listdir(path)
        sftp.close()
        return listdir

    # TODO: context manager
    def open(self, filename, mode="r", bufsize=-1):
        """open remote file for reading"""
        str(filename)

        logger.debug("{0} open({1}, {2})".format(repr(self), filename, mode))
        logger.debug("  -> self.client.open_sftp")
        sftp = self.__sftp_reconnect()
        logger.debug("  -> sftp.open")
        try:
            ofile = sftp.open(filename, mode, bufsize)
        except BaseException:
            # It often happens to me lately that mtui seems to freeze at
            # doing sftp.open() so let's log any other exception here,
            # just in case it gets eaten by some caller in mtui
            # bnc#880934
            logger.debug(format_exc())
            if "sftp" in locals():
                if isinstance(sftp, paramiko.sftp_client.SFTPClient):
                    sftp.close()
            raise
        return ofile

    def remove(self, path):
        """delete remote file"""
        path = str(path)
        logger.debug(
            "deleting file {!s}:{!s}:{!s}".format(self.hostname, self.port, path)
        )
        sftp = self.__sftp_reconnect()

        try:
            sftp.remove(path)
        except IOError:
            logger.error("Can't remove {} from {}".format(path, self.hostname))

        sftp.close()

    def rmdir(self, path):
        """delete remote directory"""
        logger.debug(
            "deleting dir {!s}:{!s}:{!s}".format(self.hostname, self.port, path)
        )
        sftp = self.__sftp_reconnect()
        items = self.listdir(path)
        for item in items:
            filename = path / item
            self.remove(filename)
        sftp.rmdir(str(path))
        sftp.close()

    def readlink(self, path):
        """ Return the target of a symbolic link (shortcut)."""
        logger.debug("read link {}:{}:{}".format(self.hostname, self.port, path))
        path = str(path)

        sftp = self.__sftp_reconnect()
        link = sftp.readlink(path)
        sftp.close()
        return link

    def is_active(self):
        return self.client._transport.is_active()

    def close(self):
        """closes SSH channel to host and disconnects

        Keyword arguments:
        None

        """

        logger.debug("closing connection to {!s}:{!s}".format(self.hostname, self.port))
        self.client.close()
