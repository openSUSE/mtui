#!/usr/bin/env python
# -*- coding: utf-8 -*-

import warnings
with warnings.catch_warnings():
	warnings.filterwarnings("ignore",category=DeprecationWarning)
	import paramiko

import stat
import errno
import getpass
import logging

out = logging.getLogger('mtui')

class Connection():
	"""manage SSH and SFTP connections"""
	def __init__(self, hostname):
		"""opens SSH channel to specified host

		Tries AuthKey Authentication and falls back to password mode in case of errors

		Keyword arguments:
		hostname -- host address to connect to

		"""
		self.hostname = hostname

		self.client = paramiko.SSHClient()
		self.client.load_system_host_keys()
		self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

		#self.client.set_combine_stderr(True)

		out.info("connecting to %s" % self.hostname)

		try:
			self.client.connect(self.hostname, username='root')
		except paramiko.AuthenticationException:
			print "AuthKey Authentication failed on %s. Make sure your system is set up correctly" % self.hostname
			print "Trying manually, please specify root password"
			password = getpass.getpass()

			try:
				self.client.connect(self.hostname, username='root', password=password)
			except Exception as error:
				out.error("connecting to %s failed: %s" % (self.hostname, str(error)))
				raise

		except Exception as error:
			out.error("connecting to %s failed: %s" % (self.hostname, str(error)))
			raise

	def run(self, command):
		"""run command over SSH channel

		Blocks until command terminates. Return value of issued command is returned.
		In case of errors, -1 is returned.

		Keyword arguments:
		command -- the command to run

		"""
		if self.is_active():
			try:
				self.sin, self.out, self.err = self.client.exec_command(command)

				exitcode = self.out.channel.recv_exit_status()

				while not self.out.channel.eof_received:
					exitcode = self.out.channel.recv_exit_status()

				return exitcode
			except:
				raise
		else:
			out.error("connection to %s is not active, can't send command" % self.hostname)
			return -1

	def stdin(self):
		"""return stdin from last command

		Keyword arguments:
		None

		"""
		try:
			return self.sin.read()
		except:
			return

	def stdout(self):
		"""return stout from last command

		Keyword arguments:
		None

		"""
		try:
			return self.out.read()
		except:
			return

	def stderr(self):
		"""return stderr from last command

		Keyword arguments:
		None

		"""
		try:
			return self.err.read()
		except:
			return

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
			except:
				pass

		try:
			sftp.put(local, remote)
			sftp.chmod(remote, stat.S_IEXEC)
		except Exception as error:
			out.error(str(error))
			raise

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
		except Exception as error:
			out.error(str(error))
			raise

		sftp.close()

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

		transport = self.client.get_transport()
		return transport.is_active()

	def close(self):
		"""closes SSH channel to host and disconnects

		Keyword arguments:
		None

		"""
		if self.is_connected():
			out.info("closing connection to %s" % self.hostname)
			self.client.close()