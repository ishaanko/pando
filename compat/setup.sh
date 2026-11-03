#!/usr/bin/env bash
# Download/build every client the compat matrix needs into a cache dir. Idempotent.
#
#   compat/setup.sh [cache-dir]     # default: compat/.cache (or $PANDO_COMPAT_CACHE)
#
# Prints `export PANDO_COMPAT_*=...` lines on stdout (everything else goes to stderr), so
#   eval "$(compat/setup.sh)"
# works. Linux only (x86_64 / aarch64).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "${1:-${PANDO_COMPAT_CACHE:-$here/.cache}}"
cache="$(cd "${1:-${PANDO_COMPAT_CACHE:-$here/.cache}}" && pwd)"

GO_VERSION="${GO_VERSION:-1.27.3}"
JGIT_VERSION="${JGIT_VERSION:-7.8.0.202609011348-r}"
GIX_VERSION="${GIX_VERSION:-0.61.0}"
OLD_GIT_VERSION="${OLD_GIT_VERSION:-2.26.0}"

case "$(uname -m)" in
  x86_64) go_arch=amd64; gix_arch=x86_64 ;;
  aarch64 | arm64) go_arch=arm64; gix_arch=aarch64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

log() { echo "setup: $*" >&2; }
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }

# Download+extract an archive into $2 once; $3 is a marker file proving it worked.
unpack() { # url dir marker
  [ -e "$2/$3" ] && return 0
  rm -rf "$2.tmp" && mkdir -p "$2.tmp"
  fetch "$1" "$2.tmp/archive"
  tar -xf "$2.tmp/archive" -C "$2.tmp" --no-same-owner
  rm "$2.tmp/archive"
  rm -rf "$2" && mv "$2.tmp" "$2"
}

exports=("PANDO_COMPAT_CACHE=$cache")

# --- go-git (Go toolchain lives in the cache; no system Go needed) ---
log "go $GO_VERSION + gogit"
unpack "https://go.dev/dl/go${GO_VERSION}.linux-${go_arch}.tar.gz" "$cache/go-${GO_VERSION}" go/bin/go
mkdir -p "$cache/gogit"
(
  cd "$here/gogit"
  export GOPATH="$cache/gopath" GOCACHE="$cache/gocache" GOTOOLCHAIN=local
  "$cache/go-${GO_VERSION}/go/bin/go" build -o "$cache/gogit/gogit" .
) >&2
exports+=("PANDO_COMPAT_GOGIT=$cache/gogit/gogit")

# --- libgit2 via pygit2 wheels (bundle libgit2 + libssh2) ---
log "pygit2"
if [ ! -x "$cache/venv/bin/python" ] || ! "$cache/venv/bin/python" -c 'import pygit2' 2>/dev/null; then
  rm -rf "$cache/venv"
  python3 -m venv "$cache/venv"
  "$cache/venv/bin/pip" install --quiet --only-binary=:all: pygit2 >&2
fi
exports+=("PANDO_COMPAT_LIBGIT2=$here/libgit2/libgit2")

# --- JGit command line ---
log "jgit $JGIT_VERSION"
if [ ! -x "$cache/jgit/jgit.sh" ]; then
  mkdir -p "$cache/jgit"
  fetch "https://repo1.maven.org/maven2/org/eclipse/jgit/org.eclipse.jgit.pgm/${JGIT_VERSION}/org.eclipse.jgit.pgm-${JGIT_VERSION}.sh" "$cache/jgit/jgit.sh.tmp"
  chmod +x "$cache/jgit/jgit.sh.tmp" && mv "$cache/jgit/jgit.sh.tmp" "$cache/jgit/jgit.sh"
fi
exports+=("PANDO_COMPAT_JGIT=$here/jgit/jgit")

# --- gitoxide (prebuilt, pure-Rust build) ---
log "gix $GIX_VERSION"
gix_dir="gitoxide-max-pure-v${GIX_VERSION}-${gix_arch}-unknown-linux-gnu"
unpack "https://github.com/GitoxideLabs/gitoxide/releases/download/v${GIX_VERSION}/${gix_dir}.tar.gz" "$cache/gix-${GIX_VERSION}" "$gix_dir/gix"
mkdir -p "$cache/gix" && ln -sf "$cache/gix-${GIX_VERSION}/$gix_dir/gix" "$cache/gix/gix"
exports+=("PANDO_COMPAT_GIX=$here/gix/gix")

# --- oldest git that supports protocol v2 ---
log "git $OLD_GIT_VERSION"
if [ -x "$cache/git-2.26/bin/git" ]; then
  exports+=("PANDO_COMPAT_OLD_GIT=$cache/git-2.26/bin/git")
elif ! command -v curl-config >/dev/null; then
  log "SKIPPING old git build: curl-config not found (install libcurl4-openssl-dev for http support)"
else
  src="$cache/git-src-${OLD_GIT_VERSION}"
  unpack "https://mirrors.edge.kernel.org/pub/software/scm/git/git-${OLD_GIT_VERSION}.tar.xz" "$src" "git-${OLD_GIT_VERSION}/Makefile"
  # Modern curl declares CURLUSESSL_TRY as an enum; git 2.26's fallback #define for
  # ancient curl then expands to itself.
  sed -i 's|^#define CURLUSESSL_TRY CURLFTPSSL_TRY|/* CURLUSESSL_TRY: see setup.sh */|' "$src/git-${OLD_GIT_VERSION}/http.h"
  make -C "$src/git-${OLD_GIT_VERSION}" -j"$(nproc)" prefix="$cache/git-2.26" \
    CFLAGS="-g -O2 -Wall -std=gnu99" NO_GETTEXT=1 NO_TCLTK=1 NO_PERL=1 NO_PYTHON=1 NO_EXPAT=1 NO_OPENSSL=1 install >&2
  exports+=("PANDO_COMPAT_OLD_GIT=$cache/git-2.26/bin/git")
fi

printf 'export %s\n' "${exports[@]}"
