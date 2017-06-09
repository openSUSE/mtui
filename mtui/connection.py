# -*- coding: utf-8 -*-
#
# mtui ssh connection handling using paramiko.
# almost all exceptions here are passed to the upper layer.
#

import os
import sys
import stat
import errno
import select
import socket
import termios
import tty
import getpass
import warnings
import logging
from traceback import format_exc

from mtui.utils import termsize

with warnings.catch_warnings():
    warnings.filterwarnings('ignore', category=DeprecationWarning)
    import paramiko


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

    def __init__(self, logger, hostname, port, timeout):
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

        self.log = logger

        self.hostname = hostname

        try:
            self.port = int(port)
        except Exception:
            self.port = 22

        self.timeout = timeout

        self.session = None
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
            with open(os.path.expanduser("~/.ssh/config")) as fd:
                cfg.parse(fd)
        except IOError as e:
            if e.errno != errno.ENOENT:
                self.log.warning(e)
        opts = cfg.lookup(self.hostname)

        try:
            self.log.debug(
                'connecting to {!s}:{!s}'.format(
                    self.hostname, self.port))
            # if this fails, the user most likely has none or an outdated
            # hostkey for the specified host. checking back with a manual
            # "ssh root@..." invocation helps in most cases.
            self.client.connect(
                hostname=opts.get(
                    'hostname', self.hostname), port=int(
                    opts.get(
                        'port', self.port)), username=opts.get(
                    'user', 'root'), key_filename=opts.get(
                    'identityfile', None), sock=paramiko.ProxyCommand(
                    opts['proxycommand']) if 'proxycommand' in opts else None, )

        except (paramiko.AuthenticationException, paramiko.BadHostKeyException):
            # if public key auth fails, fallback to a password prompt.
            # other than ssh, mtui asks only once for a password. this could
            # be changed if there is demand for it.
            self.log.warning(
                'Authentication failed on {!s}: AuthKey missing. Make sure your system is set up correctly'.format(
                    self.hostname))
            self.log.warning('Trying manually, please enter the root password')
            password = getpass.getpass()

            try:
                # try again with password auth instead of public/private key
                self.client.connect(
                    hostname=opts.get(
                        'hostname', self.hostname), port=int(
                        opts.get(
                            'port', self.port)), username=opts.get(
                        'user', 'root'), password=password, sock=paramiko.ProxyCommand(
                        opts['proxycommand']) if 'proxycommand' in opts else None, )
            except paramiko.AuthenticationException:
                # if a wrong password was set, don't connect to the host and
                # reraise the exception hoping it's catched somewhere in an
                # upper layer.
                self.log.error(
                    'Authentication failed on {!s}: wrong password'.format(
                        self.hostname))
                raise
        except paramiko.SSHException:
            # unspecified general SSHException. the host/sshd is probably not
            # available.
            self.log.error(
                'SSHException while connecting to {!s}'.format(self.hostname))
            raise
        except Exception as error:
            # general Exception
            self.log.error('{!s}: {!s}'.format(self.hostname, error))
            raise

    def reconnect(self):
        """try to reconnect to the host

        currently, there's no reconnection limit. needs to be implemented
        since the current implementation could deadlock.

        """

        # if self.is_active():
        #    return

        self.log.debug(
            'lost connection to {!s}:{!s}, reconnecting'.format(
                self.hostname, self.port))

        # wait 10s and try to reconnect
        select.select([], [], [], 10)
        self.connect()

        assert self.is_active()

    def new_session(self):
        """open new session on the channel

        all remote commands are run on a seperate session to make sure
        that leftovers/session errors from the previous command do not
        interfere with the current command.

        the current session is saved as "session" attribute of the object

        session = self.new_session()
        session.exec_command(command)
        self.close_session(session)

        or

        self.new_session()
        self.session.exec_command(command)
        self.close_session()
        """

        self.log.debug(
            'creating new session at {!s}:{!s}'.format(
                self.hostname, self.port))
        try:
            transport = self.client.get_transport()

            transport.set_keepalive(30)
            try:
                # add NullHandler to paramiko to get rid of
                # "paramiko: logging handler not found" messages
                sshlog = logging.getLogger(transport.get_log_channel())
                sshlog.addHandler(logging.NullHandler())
            except:
                pass
            session = transport.open_session()

            # disable blocking and timeout to use the session in async mode
            session.setblocking(0)
            session.settimeout(0)
            self.session = session
        except Exception:
            self.session = None

        return self.session

    def close_session(self, session=None):
        """close the current session"""
        # TODO: looks as wrong code , very wrong
        self.log.debug(
            'closing session at {!s}:{!s}'.format(
                self.hostname, self.port))
        try:
            self.session.shutdown(2)
            self.session.close()
            self.session = None
            session = None
        except:
            # pass all exceptions since the session is already closed or broken
            pass

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
        self.stdout = ''
        self.stderr = ''
        stdout = b''
        stderr = b''
        session = self.new_session()

        try:
            session.exec_command(command)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            # reconnect if the channel is lost
            self.reconnect()
            # currently rerunning a command after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.run(command, lock)

        while True:
            buffer = b''

            # wait for data to be transmitted. if the timeout is hit,
            # ask the user on how to procceed
            if select.select([session], [], [], self.timeout) == ([], [], []):
                assert self.session

                # writing on stdout needs locking as all run threads could
                # write at the same time to stdout
                if lock is not None:
                    lock.acquire()

                try:
                    if input(
                            'command "%s" timed out on %s. wait? (y/N) ' %
                            (command, self.hostname)).lower() in [
                            'y', 'yes']:
                        continue
                    else:
                        # if the user don't want to wait, raise CommandTimeout
                        # and procceed
                        raise CommandTimeout
                finally:
                    # release lock to allow other command threads to write to
                    # stdout
                    if lock is not None:
                        lock.release()

            try:
                # wait for data on the session's stdout/stderr. if debug is enabled,
                # print the received data
                if session.recv_ready():
                    buffer = session.recv(1024)
                    stdout += buffer
                    for line in buffer.decode('utf-8', 'ignore').split('\n'):
                        if line:
                            self.log.debug(line)

                if session.recv_stderr_ready():
                    buffer = session.recv_stderr(1024)
                    stderr += buffer
                    for line in buffer.decode('utf-8', 'ignore').split('\n'):
                        if line:
                            self.log.debug(line)

                if not buffer:
                    break

            except socket.timeout:
                select.select([], [], [], 1)

        # save the exitcode of the last command and return it
        exitcode = session.recv_exit_status()

        self.close_session(session)
        self.stdout = stdout.decode('utf-8')
        self.stderr = stderr.decode('utf-8')
        return exitcode

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

        try:
            session.get_pty('xterm', width, height)
            session.invoke_shell()
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            # reconnect if the channel is lost
            self.reconnect()
            # currently rerunning a command after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.shell()

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

    def put(self, local, remote):
        """transfers a file to the remote host over SFTP

        File is made executable

        Keyword arguments:
        local  -- local file name
        remote -- remote file name

        """

        path = ''
        try:
            sftp = self.client.open_sftp()
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently resending a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.put(local, remote)

        # create remote base directory and copy the file to that directory
        for subdir in remote.split('/')[:-1]:
            path += subdir + '/'
            try:
                sftp.mkdir(path)
            except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
                self.reconnect()
                # currently resending a file after reconnection is implemented
                # as recursion. this is a really bad idea and needs fixing.
                return self.put(local, remote)
            except Exception:
                pass

        self.log.debug('transmitting {!s} to {!s}:{!s}:{!s}'.format(
                           local, self.hostname, self.port, remote))
        sftp.put(local, remote)

        # make file executable since it's probably a script which needs to be
        # run
        sftp.chmod(remote, stat.S_IEXEC)

        sftp.close()

    def get(self, remote, local):
        """transfers file from the remote host to the local host over SFTP

        local base directory needs to exist

        Keyword arguments:
        remote -- remote file name
        local  -- local file name

        """

        try:
            sftp = self.client.open_sftp()
            self.log.debug('transmitting {!s}:{!s}:{!s} to {!s}'.format(
                               self.hostname, self.port, remote, local))
            sftp.get(remote, local)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently resending a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.get(remote, local)

        sftp.close()

    # Similar to 'get' but handles folders.
    def get_folder(self, remote_folder, local_folder):

        sftp = self.client.open_sftp()
        self.log.debug('transmitting {!s}:{!s}:{!s} to {!s}'.format(
            self.hostname, self.port, remote_folder, local_folder))
        files = self.listdir(remote_folder)
        for f in files:
            sftp.get("{!s}/{!s}".format(remote_folder, f),
                     "{!s}{!s}.{!s}".format(local_folder, f, self.hostname))

        sftp.close()

    def listdir(self, path='.'):
        """get directory listing of the remote host

        Keyword arguments:
        path   -- remote directory path to list

        """

        self.log.debug(
            'getting {!s}:{!s}:{!s} listing'.format(
                self.hostname, self.port, path))
        try:
            sftp = self.client.open_sftp()
            return sftp.listdir(path)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently resending a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.listdir(path)

    def open(self, filename, mode='r', bufsize=-1):
        """open remote file for reading"""

        self.log.debug('{0} open({1}, {2})'.format(
            repr(self), filename, mode
        ))
        try:
            self.log.debug("  -> self.client.open_sftp")
            sftp = self.client.open_sftp()
            self.log.debug("  -> sftp.open")
            return sftp.open(filename, mode, bufsize)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently opening a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.open(filename, mode, bufsize)
        except:
            # It often happens to me lately that mtui seems to freeze at
            # doing sftp.open() so let's log any other exception here,
            # just in case it gets eaten by some caller in mtui
            # bnc#880934
            self.log.debug(format_exc())
            raise

    def remove(self, path):
        """delete remote file"""

        self.log.debug(
            'deleting file {!s}:{!s}:{!s}'.format(
                self.hostname, self.port, path))
        try:
            sftp = self.client.open_sftp()
            return sftp.remove(path)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently removing a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.remove(path)

    def rmdir(self, path):
        """delete remote directory"""

        self.log.debug(
            'deleting dir {!s}:{!s}:{!s}'.format(
                self.hostname, self.port, path))
        try:
            sftp = self.client.open_sftp()
            items = self.listdir(path)
            for item in items:
                filename = os.path.join(path, item)
                self.remove(filename)
            return sftp.rmdir(path)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently removing a directory after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.rmdir(path)

    def is_active(self):
        """check if connection to host is still active

        Keyword arguments:
        None

        """

        try:
            # if the channel is active, we should get a new session.
            # if not, the channel is probable not active.
            session = self.new_session()
            assert(session)
            self.close_session(session)
        except Exception:
            return False

        return True

    def close(self):
        """closes SSH channel to host and disconnects

        Keyword arguments:
        None

        """

        self.log.debug(
            'closing connection to {!s}:{!s}'.format(self.hostname, self.port))
        self.client.close()
