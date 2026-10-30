#!/usr/bin/env bash
# Static assertions over the root Dockerfile: the image must serve as a
# non-root user and must not bake any operator credentials into its layers.
# No container runtime needed, so this runs in CI and on a plain checkout.
#
# Dockerfile keywords are case-insensitive, so every keyword match below is.
#
# Usage: container-structure-test.sh [Dockerfile]
set -euo pipefail

file="${1:-Dockerfile}"
fail=0

[ -r "$file" ] || { echo "FAIL: cannot read $file" >&2; exit 1; }

deny() {
    echo "FAIL: $1" >&2
    fail=1
}

# The serving stage's `USER` must be non-root: only lines after the last FROM
# count, so a builder USER can neither satisfy nor spoil the check. Compared
# lowercased with any `:group` stripped, so `USER root:wheel` and `user ROOT`
# fail like `USER root` does. Leading whitespace is allowed (Dockerfile
# permits it), so indented instructions count like flush ones.
serving="$(awk 'BEGIN{u=""} /^[[:space:]]*[Ff][Rr][Oo][Mm][[:space:]]/{u=""} /^[[:space:]]*[Uu][Ss][Ee][Rr][[:space:]]+/{u=$2} END{print u}' "$file")"
user="$(printf '%s' "$serving" | tr '[:upper:]' '[:lower:]')"
user="${user%%:*}"
case "$user" in
    "" | root | 0) deny "serving USER is '${serving:-<none>}', want a non-root user" ;;
    *) echo "ok: serving USER is '$serving'" ;;
esac

# The runtime stage must be a real distro base, never scratch (there is no
# shell, no useradd, no CA store in scratch to debug from).
if grep -Ei '^[[:space:]]*FROM[[:space:]]' "$file" | tail -1 | awk '{for (i = 2; i <= NF; i++) if ($i !~ /^--/) {print $i; exit}}' | grep -qi '^scratch$'; then
    deny "runtime stage is FROM scratch"
else
    echo "ok: runtime stage is not scratch"
fi

# No credential material may enter any layer: no key/oscrc/cloud-config COPY,
# no secret-bearing ENV or ARG default (a defaulted ARG value is visible in
# `image history`). A bare `ARG NAME` with no default bakes nothing, so only
# `NAME=value` forms are checked.
if grep -Ei '^[[:space:]]*(copy|add)[[:space:]]' "$file" \
    | grep -Ei '(\.ssh|oscrc|\.aws/|\.gnupg|id_rsa|id_ed25519|id_ecdsa|\.pem|\.key|credentials\.toml)'; then
    deny "Dockerfile COPYs credential material (see above)"
else
    echo "ok: no credential COPY/ADD"
fi
if grep -Ei '^[[:space:]]*ENV[[:space:]]' "$file" | grep -Ei '(token|password|secret)'; then
    deny "Dockerfile bakes a secret into ENV (see above)"
else
    echo "ok: no secret ENV"
fi
if grep -Ei '^[[:space:]]*ARG[[:space:]][^#]*=' "$file" | grep -Ei '(key|private|credential|aws|token|password|secret)'; then
    deny "Dockerfile bakes a secret into an ARG default (see above)"
else
    echo "ok: no secret ARG default"
fi
# RUN and LABEL persist in `image history` and the image config like ENV does.
if grep -Ei '^[[:space:]]*(RUN|LABEL)[[:space:]]' "$file" | grep -Ei '(token|password|secret|BEGIN .*PRIVATE KEY)'; then
    deny "Dockerfile bakes a secret into RUN/LABEL (see above)"
else
    echo "ok: no secret RUN/LABEL"
fi

# Both shipped binaries must reach the runtime image at the documented
# destination. Root-owned and mode-pinned, so the serving user cannot rewrite
# its own binaries in place (see Dockerfile).
ship="$(grep -Ei '^[[:space:]]*COPY[[:space:]].*--from=builder([[:space:]]|$)' "$file" || true)"
# Each builder COPY line stands alone: joining the lines would let one line's
# flags vouch for another line's files.
nship=0
while IFS= read -r line; do
    [ -n "$line" ] || continue
    nship=$((nship + 1))
    case "$line" in *"/target/release/mtui "*) echo "ok: mtui shipped" ;; *) deny "builder COPY ships no mtui binary: $line" ;; esac
    case "$line" in *"target/release/mtui-mcp "*) echo "ok: mtui-mcp shipped" ;; *) deny "builder COPY ships no mtui-mcp binary: $line" ;; esac
    case "$line" in *"--chown=root:root"*) echo "ok: ship COPY is root-owned" ;; *) deny "ship COPY is not --chown=root:root: $line" ;; esac
    case "$line" in *"--chmod=755"*) echo "ok: ship COPY is mode-pinned" ;; *) deny "ship COPY is not --chmod=755: $line" ;; esac
    case "$line" in *"/usr/local/bin/"*) echo "ok: ships into /usr/local/bin" ;; *) deny "ship COPY destination is not /usr/local/bin: $line" ;; esac
done <<< "$ship"
[ "$nship" -gt 0 ] || deny "no COPY --from=builder line ships the binaries"

# The MCP http transport listens on 8000 by default; the port must be exposed
# by the serving stage: only lines after the last FROM count, as with USER
# above. The trailing boundary keeps `EXPOSE 18000` from false-passing.
serving_stage="$(awk '/^[[:space:]]*[Ff][Rr][Oo][Mm][[:space:]]/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$file")"
if printf '%s' "$serving_stage" | grep -Ei '^[[:space:]]*EXPOSE[[:space:]]+8000([[:space:]/,]|$)'; then
    echo "ok: EXPOSE 8000"
else
    deny "no EXPOSE 8000 for mtui-mcp --transport http"
fi

exit "$fail"
