# Running Pando

## systemd

```sh
# 1. Binary (from a GitHub release tarball, or `cargo build --release`)
sudo install -m 755 pando /usr/local/bin/pando

# 2. Tokens: `<name> <token> <read|write|admin> [repo-globs]`
sudo install -d -m 755 /etc/pando
echo "ishaan $(openssl rand -hex 24) admin" | sudo tee /etc/pando/tokens
echo "ci ssh-ed25519 AAAA...your-public-key... write" | sudo tee -a /etc/pando/tokens
sudo chmod 600 /etc/pando/tokens

# 3. Secrets (optional): signed URLs and webhooks
printf 'PANDO_URL_SECRET=%s\n' "$(openssl rand -hex 32)" | sudo tee /etc/pando/env
sudo chmod 600 /etc/pando/env

# 4. Unit
sudo install -m 644 contrib/pando.service /etc/systemd/system/pando.service
sudo systemctl daemon-reload
sudo systemctl enable --now pando
curl localhost:8080/healthz   # ok
```

Data lives in `/var/lib/pando` (`StateDirectory`, owned by a transient user). Back it
up with `pando backup --data /var/lib/pando --out DIR` (run as root, or stop the
unit first for a quiet copy). Edit `ExecStart` to add `--public-url`, `--webhook`,
`--public-read`, `--gc-interval` and friends, then `systemctl daemon-reload && systemctl restart pando`.

## TLS

The unit listens on `127.0.0.1:8080`. Put Caddy in front for automatic HTTPS:

```sh
caddy reverse-proxy --from git.example.com --to 127.0.0.1:8080
```

Set `--public-url https://git.example.com` so generated URLs match.

## SSH

`--ssh-listen 0.0.0.0:2222` is on in the unit; open the port in your firewall. The
host key is generated in the data directory on first start. Clients use any user name:

```sh
git clone ssh://git@git.example.com:2222/demo.git
```

To serve SSH on port 22 instead, move the system sshd or put a TCP forwarder in front
(the unit has no capabilities, so it cannot bind privileged ports itself).

## Docker

```sh
docker run -d --name pando -p 8080:8080 -p 2222:2222 \
  -v pando-data:/data \
  -v "$PWD/tokens:/etc/pando/tokens:ro" \
  ghcr.io/ishaanko/pando \
  serve --data /data --listen 0.0.0.0:8080 --ssh-listen 0.0.0.0:2222 \
        --auth-file /etc/pando/tokens
```

The container runs as uid 10001. A named volume works as is; for a bind mount,
`chown 10001 DIR` first. Passing arguments replaces the default `serve` command.
