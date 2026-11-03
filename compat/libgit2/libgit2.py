"""Pando compat helper built on pygit2 (libgit2 + libssh2).

    libgit2 clone <url> <dir>
    libgit2 fetch <dir>
    libgit2 push <dir> <refspec>...

http(s) URLs carry credentials in the userinfo (basic auth); ssh URLs use the
unencrypted private key at $PANDO_SSH_KEY with host key checking disabled.
"""

import os
import subprocess
import sys
import tempfile
from urllib.parse import unquote, urlsplit

import pygit2


def callbacks_for(url: str, keep: list) -> pygit2.RemoteCallbacks:
    """Credentials + accept-any-certificate callbacks for `url`. `keep` holds temp dirs alive."""
    parts = urlsplit(url)
    if parts.scheme in ("http", "https"):
        cred = pygit2.UserPass(unquote(parts.username or ""), unquote(parts.password or ""))
    elif parts.scheme == "ssh":
        key = os.environ["PANDO_SSH_KEY"]
        pub = key + ".pub"
        if not os.path.exists(pub):
            tmp = tempfile.TemporaryDirectory()
            keep.append(tmp)
            pub = os.path.join(tmp.name, "key.pub")
            with open(pub, "w") as f:
                f.write(subprocess.check_output(["ssh-keygen", "-y", "-f", key], text=True))
        cred = pygit2.Keypair(parts.username or "git", pub, key, "")
    else:
        raise SystemExit(f"libgit2: unsupported URL scheme {parts.scheme!r}")

    class Callbacks(pygit2.RemoteCallbacks):
        rejected: list[str] = []

        def credentials(self, url, username_from_url, allowed_types):
            return cred

        def certificate_check(self, certificate, valid, host):
            return True

        # Called per ref; a non-empty message means the server rejected that update.
        def push_update_reference(self, refname, message):
            if message:
                self.rejected.append(f"{refname}: {message}")

    return Callbacks()


def main(argv: list[str]) -> int:
    keep: list = []
    match argv:
        case ["clone", url, dest]:
            pygit2.clone_repository(url, dest, callbacks=callbacks_for(url, keep))
        case ["fetch", dest]:
            remote = pygit2.Repository(dest).remotes["origin"]
            remote.fetch(callbacks=callbacks_for(remote.url, keep))
        case ["push", dest, *refspecs] if refspecs:
            remote = pygit2.Repository(dest).remotes["origin"]
            cb = callbacks_for(remote.url, keep)
            remote.push(refspecs, callbacks=cb)
            if cb.rejected:
                print("libgit2: push rejected: " + "; ".join(cb.rejected), file=sys.stderr)
                return 1
        case _:
            print("usage: libgit2 clone <url> <dir> | fetch <dir> | push <dir> <refspec>...", file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (pygit2.GitError, OSError, subprocess.CalledProcessError, KeyError) as e:
        print(f"libgit2: {e}", file=sys.stderr)
        sys.exit(1)
