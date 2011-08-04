#!/usr/bin/env python
# -*- coding: utf-8 -*-

import sys
import stat
import errno
import select
import socket
import getpass
import logging
import warnings

with warnings.catch_warnings():
	warnings.filterwarnings("ignore",category=DeprecationWarning)
	import paramiko

out = logging.getLogger('mtui')

class CommandTimeout(Exception):
	def __init__(self, command=None):
		self.command = command

	def __str__(self):
		return repr(self.command)

class Connection():
	"""manage SSH and SFTP connections"""
	def __init__(self, hostname, timeout):
		"""opens SSH channel to specified host

		Tries AuthKey Authentication and falls back to password mode in case of errors

		Keyword arguments:
		hostname -- host address to connect to

		"""
		# paramiko.util.log_to_file("/tmp/paramiko.log")
		self.hostname = hostname
		self.timeout = timeout

		self.client = paramiko.SSHClient()
		self.client.load_system_host_keys()
		self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

		#self.client.set_combine_stderr(True)

		self.connect()

	def connect(self):
		try:
			self.client.connect(self.hostname, username='root')
		except paramiko.AuthenticationException:
			out.warning("Authentication failed on %s: AuthKey missing. Make sure your system is set up correctly" % self.hostname)
			print "Trying manually, please specify root password"
			password = getpass.getpass()

			try:
				self.client.connect(self.hostname, username='root', password=password)
			except paramiko.AuthenticationException:
				out.error("Authentication failed on %s: wrong password" % self.hostname)
				raise

		except paramiko.BadHostKeyException:
			out.error("Authentication failed on %s: Hostkey did not match. Make sure your system is set up correctly" % self.hostname)
			raise

	def reconnect(self):
		if self.is_active():
			return

		out.debug("lost connection, reconnecting")
		select.select([], [], [], 20)
		self.connect()

		assert self.is_active()

	def run(self, command, lock=None):
		"""run command over SSH channel

		Blocks until command terminates. Return value of issued command is returned.
		In case of errors, -1 is returned.

		Keyword arguments:
		command -- the command to run

		"""
		self.stdin = command
		self.stdout = ''
		self.stderr = ''

		transport = self.client.get_transport()

		try:
			session = transport.open_session()
			session.setblocking(0)
		except AttributeError:
			self.reconnect()
			return self.run(command, lock)

		session.settimeout(0)
		session.exec_command(command)

		while True:
			buffer = ''

			if select.select([session], [], [], self.timeout) == ([],[],[]):
				if lock is not None:
					lock.acquire()

				try:
					if raw_input('command "%s" timed out on %s. wait? (y/N) ' % (command, self.hostname)).lower() in ["y", "yes"]:
						continue
					else:
						raise CommandTimeout
				finally:
					if lock is not None:
						lock.release()

			try:
				if session.recv_ready():
					buffer = session.recv(1024)
					self.stdout += buffer

					for line in buffer.split('\n'):
						if line: out.debug(line)

				if session.recv_stderr_ready():
					buffer = session.recv_stderr(1024)
					self.stderr += buffer

					for line in buffer.split('\n'):
						if line: out.debug(line)

				if not buffer:
					break

			except socket.timeout:
				select.select([], [], [], 1)

		exitcode = session.recv_exit_status()

		session.close()

		return exitcode

	def put(self, local, remote):
		"""transfers file to the host over SSH channel

		File is made executable

		Keyword arguments:
		local  -- local file name
		remote -- remote file name

		"""
		path = ""
		sftp = self.client.open_sftp()

		for subdir in remote.split('/')[:-1]:
			path += subdir + '/'
			try:
				sftp.mkdir(path)
			except AttributeError:
				self.reconnect()
				return self.put(local, remote)
			except Exception:
				pass

		sftp.put(local, remote)
		sftp.chmod(remote, stat.S_IEXEC)

		sftp.close()

	def get(self, remote, local):
		"""transfers file from the host over SSH channel 

		Keyword arguments:
		remote -- remote file name
		local  -- local file name

		"""
		sftp = self.client.open_sftp()
		try:
			sftp.get(remote, local)
		except AttributeError:
			self.reconnect()
			return self.get(remote, local)

		sftp.close()

	def open(self, filename, mode='r', bufsize=-1):
		sftp = self.client.open_sftp()
		try:
			return sftp.open(filename, mode, bufsize)
		except AttributeError:
			self.reconnect()
			return self.open(filename, mode, bufsize)

	def remove(self, path):
		sftp = self.client.open_sftp()
		try:
			return sftp.remove(path)
		except AttributeError:
			self.reconnect()
			return self.remove(path)

	def is_connected(self):
		"""check if connection to host is established

		Keyword arguments:
		None

		"""
		if not self.client:
			return False

		if self.client.get_transport():
			return True
		else:
			return False

	def is_active(self):
		"""check if connection to host is still active

		Keyword arguments:
		None

		"""
		if not self.is_connected():
			return False

		try:
			transport = self.client.get_transport()
			session = transport.open_session()
			session.setblocking(0)
			session.close()
		except Exception:
			return False

		return transport.is_active()

	def close(self):
		"""closes SSH channel to host and disconnects

		Keyword arguments:
		None

		"""

		self.client.close()
