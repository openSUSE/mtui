complete -c mtui-mcp -s t -l template-dir -d 'Override config `mtui.template_dir`' -r -F
complete -c mtui-mcp -s w -l connection-timeout -d 'Override config `connection.connection_timeout` (seconds)' -r
complete -c mtui-mcp -s c -l config -d 'Override the default config path' -r -F
complete -c mtui-mcp -l color -d 'Control coloured (log) output. Logs go to stderr; stdout is the transport' -r -f -a "auto\t'Colour iff stderr is a TTY and `NO_COLOR` is unset. The default'
always\t'Always emit colour escapes'
never\t'Never emit colour escapes'"
complete -c mtui-mcp -s g -l gitea-token -d 'Gitea access token' -r
complete -c mtui-mcp -l ssl-verify -d 'Override config `mtui.ssl_verify`: TLS certificate verification for all outbound HTTP. Accepts `true`/`false` (and the spellings `yes`/`no`/ `on`/`off`/`1`/`0`), or a path to a custom CA bundle/certificate' -r
complete -c mtui-mcp -l transport -d 'MCP transport to serve on: `stdio` (default, one client) or `http` (streamable HTTP, per-client session isolation)' -r -f -a "stdio\t'Serve over stdin/stdout (default). One process serves one client'
http\t'Serve over streamable HTTP. One process serves many isolated clients'"
complete -c mtui-mcp -l host -d 'Bind address for `--transport http` (default: 127.0.0.1). Ignored under stdio. The address is not restricted, but rmcp\'s DNS-rebinding guard answers 403 to any request whose `Host` header is not `localhost`, `127.0.0.1` or `::1`: bind loopback, or put the listener behind an authenticated boundary' -r
complete -c mtui-mcp -l port -d 'Bind port for `--transport http` (default: 8000). Ignored under stdio' -r
complete -c mtui-mcp -s d -l debug -d 'Enable debugging output'
complete -c mtui-mcp -s h -l help -d 'Print help (see more with \'--help\')'
complete -c mtui-mcp -s V -l version -d 'Print version'
