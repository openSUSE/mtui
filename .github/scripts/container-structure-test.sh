#!/usr/bin/env bash
# Static assertions over the root Containerfile: the image must serve as a
# non-root user and must not bake any operator credentials into its layers.
# No container runtime needed, so this runs in CI and on a plain checkout.
#
# Dockerfile keywords are case-insensitive, so every keyword match below is.
#
# Usage: container-structure-test.sh [Containerfile]
set -euo pipefail

file="${1:-Containerfile}"
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
if grep -Ei '^[[:space:]]*FROM[[:space:]]' "$file" | tail -1 | grep -qi 'scratch'; then
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
    deny "Containerfile COPYs credential material (see above)"
else
    echo "ok: no credential COPY/ADD"
fi
if grep -Ei '^[[:space:]]*ENV[[:space:]]' "$file" | grep -Ei '(token|password|secret)'; then
    deny "Containerfile bakes a secret into ENV (see above)"
else
    echo "ok: no secret ENV"
fi
if grep -Ei '^[[:space:]]*ARG[[:space:]][^#]*=' "$file" | grep -Ei '(key|private|credential|aws|token|password|secret)'; then
    deny "Containerfile bakes a secret into an ARG default (see above)"
else
    echo "ok: no secret ARG default"
fi

# Both shipped binaries must reach the runtime image at the documented
# destination. Root-owned and mode-pinned, so the serving user cannot rewrite
# its own binaries in place (see Containerfile).
ship="$(grep -Ei '^[[:space:]]*COPY[[:space:]][[:space:]]*--from=builder' "$file" || true)"
case "$ship" in
    *"/target/release/mtui "*) echo "ok: mtui shipped" ;;
    *) deny "mtui binary not COPYd from the builder stage" ;;
esac
case "$ship" in
    *"target/release/mtui-mcp "*) echo "ok: mtui-mcp shipped" ;;
    *) deny "mtui-mcp binary not COPYd from the builder stage" ;;
esac
case "$ship" in
    *"--chown=root:root"*) echo "ok: ship COPY is root-owned" ;;
    *) deny "ship COPY is not --chown=root:root" ;;
esac
case "$ship" in
    *"--chmod=755"*) echo "ok: ship COPY is mode-pinned" ;;
    *) deny "ship COPY is not --chmod=755" ;;
esac
case "$ship" in
    *"/usr/local/bin/"*) echo "ok: ships into /usr/local/bin" ;;
    *) deny "ship COPY destination is not /usr/local/bin" ;;
esac

# The MCP http transport listens on 8000 by default; the port must be exposed.
if grep -Eq '^[[:space:]]*EXPOSE[[:space:]][[:space:]]*8000' "$file"; then
    echo "ok: EXPOSE 8000"
else
    deny "no EXPOSE 8000 for mtui-mcp --transport http"
fi

exit "$fail"
