complete -c mtui -s t -l template-dir -d 'Override config `mtui.template_dir`' -r -F
complete -c mtui -s s -l sut -d 'Cumulatively override the default hosts from the template (format: `hostname,hostname2`). May be given more than once' -r
complete -c mtui -s w -l connection-timeout -d 'Override config `mtui.connection_timeout` (seconds)' -r
complete -c mtui -l reboot-timeout -d 'Override config `connection.reboot_timeout`: the backoff base (seconds) for post-reboot reconnect retries' -r
complete -c mtui -l reboot-retries -d 'Override config `connection.reboot_retries`: the number of post-reboot reconnect attempts beyond the first probe' -r
complete -c mtui -s c -l config -d 'Override the default config path' -r -F
complete -c mtui -l color -d 'Control coloured output' -r -f -a "auto\t'Colour iff stderr is a TTY and `NO_COLOR` is unset. The default'
always\t'Always emit colour escapes'
never\t'Never emit colour escapes'"
complete -c mtui -s g -l gitea-token -d 'Gitea access token' -r
complete -c mtui -l ssl-verify -d 'Override config `mtui.ssl_verify`: TLS certificate verification for all outbound HTTP. Accepts `true`/`false` (and the spellings `yes`/`no`/ `on`/`off`/`1`/`0`), or a path to a custom CA bundle/certificate' -r
complete -c mtui -s a -l auto-review-id -d 'OBS request review id, run under the automatic workflow (example: `SUSE:Maintenance:1:1`)' -r
complete -c mtui -s k -l kernel-review-id -d 'OBS kernel/live-patch request review id, run under the kernel workflow (example: `SUSE:Maintenance:1:1`)' -r
complete -c mtui -s d -l debug -d 'Enable debugging output'
complete -c mtui -s h -l help -d 'Print help (see more with \'--help\')'
complete -c mtui -s V -l version -d 'Print version'
