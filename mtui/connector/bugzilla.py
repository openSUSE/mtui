# -*- coding: utf-8 -*-
#
# manage connection to Bugzilla
#

try:
    import xmlrpc.client as xmlrpclib
except ImportError:
    import xmlrpclib


class Bugzilla(object):
    """Connector to the Bugzilla XMLRPC interface

    Interface to the Bugzilla XMLRPC API documented at
    http://www.bugzilla.org/docs/4.0/en/html/api/Bugzilla/WebService.html

    Interface to the Testopia XMLRPC API documented at
    https://wiki.mozilla.org/Testopia:Documentation:XMLRPC

    """

    def __init__(self, logger, interface, username="", password=""):
        """create xmlrpclib.ServerProxy object for communication

        creates a ServerProxy XMLRPC instance with Bugzilla credentials

        Keyword arguments:
        None

        """

        self.log = logger

        # just basic auth for the start
        self.url = interface.replace('://', '://%s:%s@' % (username, password))
        self.proxy = xmlrpclib.ServerProxy(self.url);

    def query_interface(self, service, *query):
        """generic XMLRPC interface query

        queries Bugzilla services and handles exceptions on the
        XMLRPC interface

        Keyword arguments:
        service  -- XMLRPC service to query object
        query    -- XMLRPC query

        """

        try:
            method = getattr(self.proxy, service)
            return method(*query)
        except AttributeError:
            self.log.critical('service "%s" does not exist.' % service)
            raise
        except xmlrpclib.ProtocolError as error:
            if error.errcode == 401:
                self.log.critical('failed to authorize with Bugzilla')
            raise
        except xmlrpclib.Fault as error:
            if error.faultCode == 32000:
                self.log.critical('testcase does not exist')
            raise


