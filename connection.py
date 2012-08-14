#!/usr/bin/python
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
import getpass
import logging
import warnings
import logging

with warnings.catch_warnings():
    warnings.filterwarnings('ignore', category=DeprecationWarning)
    import paramiko

out = logging.getLogger('mtui')


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

        self.session = None
        self.client = paramiko.SSHClient()
        self.client.load_system_host_keys()
        self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

        # uncomment to combine stderr and stdout channel. In most cases,
        # mtui expects a separate stderr channel. Changing this may be
        # harmfull to error checking code.

        # self.client.set_combine_stderr(True)

        self.connect()

    def connect(self):
        """connect to the remote host using paramiko as ssh subsystem"""

        try:
            out.debug('connecting to %s:%s' % (self.hostname, self.port))
            # if this fails, the user most likely has none or an outdated
            # hostkey for the specified host. checking back with a manual
            # "ssh root@..." invocation helps in most cases.
            self.client.connect(self.hostname, self.port, username='root')
        except (paramiko.AuthenticationException, paramiko.BadHostKeyException):
            # if public key auth fails, fallback to a password prompt.
            # other than ssh, mtui asks only once for a password. this could
            # be changed if there is demand for it.
            out.warning('Authentication failed on %s: AuthKey missing. Make sure your system is set up correctly' % self.hostname)
            print 'Trying manually, please enter the root password'
            password = getpass.getpass()

            try:
                # try again with password auth instead of public/private key
                self.client.connect(self.hostname, self.port, username='root', password=password)
            except paramiko.AuthenticationException:
                # if a wrong password was set, don't connect to the host and
                # reraise the exception hoping it's catched somewhere in an
                # upper layer.
                out.error('Authentication failed on %s: wrong password' % self.hostname)
                raise
        except paramiko.SSHException:
            # unspecified general SSHException. the host/sshd is probably not available.
            out.error('SSHException while connecting to %s' % self.hostname)
            raise

    def reconnect(self):
        """try to reconnect to the host

        currently, there's no reconnection limit. needs to be implemented
        since the current implementation could deadlock.

        """

        # if self.is_active():
        #    return

        out.debug('lost connection to %s:%s, reconnecting' % (self.hostname, self.port))

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
        self.close_session()

        or

        self.new_session()
        self.session.exec_command(command)
        self.close_session()
        """

        out.debug('creating new session at %s:%s' % (self.hostname, self.port))
        try:
            transport = self.client.get_transport()

            # enable compression to reduce transmission size. could be
            # helpful on bad lines/huge latencies.
            transport.use_compression()
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

    def close_session(self):
        """close the current session"""

        out.debug('closing session at %s:%s' % (self.hostname, self.port))
        try:
            self.session.shutdown(2)
            self.session.close()
            self.session = None
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
            buffer = ''

            # wait for data to be transmitted. if the timeout is hit,
            # ask the user on how to procceed
            if select.select([session], [], [], self.timeout) == ([], [], []):
                assert self.session

                # writing on stdout needs locking as all run threads could
                # write at the same time to stdout
                if lock is not None:
                    lock.acquire()

                try:
                    if raw_input('command "%s" timed out on %s. wait? (y/N) ' % (command, self.hostname)).lower() in ['y', 'yes']:
                        continue
                    else:
                        # if the user don't want to wait, raise CommandTimeout and procceed
                        raise CommandTimeout
                finally:
                    # release lock to allow other command threads to write to stdout
                    if lock is not None:
                        lock.release()

            try:
                # wait for data on the session's stdout/stderr. if debug is enabled,
                # print the received data
                if session.recv_ready():
                    buffer = session.recv(1024)
                    self.stdout += buffer

                    for line in buffer.split('\n'):
                        if line:
                            out.debug(line)

                if session.recv_stderr_ready():
                    buffer = session.recv_stderr(1024)
                    self.stderr += buffer

                    for line in buffer.split('\n'):
                        if line:
                            out.debug(line)

                if not buffer:
                    break
            except socket.timeout:
                select.select([], [], [], 1)

        # save the exitcode of the last command and return it
        exitcode = session.recv_exit_status()

        self.close_session()

        return exitcode

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
            # return on all SSH exceptions. need to implement some exceptions
            # to let the upper layer know that the file transfer has failed.
            return

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

        out.debug('transmitting %s to %s:%s:%s' % (local, self.hostname, self.port, remote))
        sftp.put(local, remote)

        # make file executable since it's probably a script which needs to be run
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
            out.debug('transmitting %s:%s:%s to %s' % (self.hostname, self.port, remote, local))
            sftp.get(remote, local)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently resending a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.get(remote, local)

        sftp.close()

    def listdir(self, path='.'):
        """get directory listing of the remote host

        Keyword arguments:
        path   -- remote directory path to list

        """

        out.debug('getting %s:%s:%s listing' % (self.hostname, self.port, path))
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

        out.debug('opening file %s:%s:%s (%s)' % (self.hostname, self.port, filename, mode))
        try:
            sftp = self.client.open_sftp()
            return sftp.open(filename, mode, bufsize)
        except (AttributeError, paramiko.ChannelException, paramiko.SSHException):
            self.reconnect()
            # currently opening a file after reconnection is implemented
            # as recursion. this is a really bad idea and needs fixing.
            return self.open(filename, mode, bufsize)

    def remove(self, path):
        """delete remote file"""

        out.debug('deleting file %s:%s:%s' % (self.hostname, self.port, path))
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

        out.debug('deleting dir %s:%s:%s' % (self.hostname, self.port, path))
        try:
            sftp = self.client.open_sftp()
            items = self.listdir(path)
            for item in items:
                self.remove("%s/%s" % (path, item))
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
            assert(self.new_session())
            self.close_session()
        except Exception:
            return False

        return True

    def close(self):
        """closes SSH channel to host and disconnects

        Keyword arguments:
        None

        """

        out.debug('closing connection to %s:%s' % (self.hostname, self.port))
        self.client.close()


