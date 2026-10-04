# pairly-relay

The relay lets your phone reach your PC when they aren't on the same network: mobile data,
another Wi-Fi, or a router that isolates clients.

- **Blind:** it only pairs two encrypted streams that present the same room ID and copies bytes
  between them. Rooms are derived from each pairing's secret, and everything inside is a Noise
  session, so the relay never learns device names or IDs, or what is sent.
- **Pinned:** its certificate is self-signed. Devices trust it by the certificate's hash (the
  *pin*) in the relay address, so no domain name is needed.
- **Private:** an access token keeps strangers from using your server's bandwidth.
- **Small:** one binary, a few MB of RAM, UDP port 47200.

You configure the relay **only on the PC**. A paired phone learns it automatically the next time
they connect.

---

## Deploy on a VPS (step by step)

### 1. Get a server

Any small Linux VPS with a public IPv4 address works. 1 vCPU and 1 GB RAM is plenty. The
examples assume **Ubuntu 24.04 or Debian 12** and that you can `ssh` in as a user with `sudo`.
Note the server's public IP; it's written `SERVER_IP` below.

Traffic: each relayed byte counts twice (in and out). Notifications are tiny; large file
transfers over the relay use real bandwidth, so check your plan's allowance. Pairly goes back to
the direct LAN link as soon as both devices are home.

### 2. Open UDP port 47200

On the server:

```sh
sudo ufw allow OpenSSH
sudo ufw allow 47200/udp
sudo ufw enable
```

Many providers also have a firewall in their web console, separate from the server's own. Add
an inbound rule there for **UDP 47200**:

- **Oracle Cloud:** the subnet's *Security List*. Oracle's Ubuntu images also have iptables
  rules, so run `sudo iptables -I INPUT -p udp --dport 47200 -j ACCEPT` and
  `sudo netfilter-persistent save`.
- **AWS:** the *Security Group*.
- **Hetzner** and **DigitalOcean:** *Firewalls*.

### 3. Install Docker

```sh
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker $USER      # then log out and back in
```

With 1 GB of RAM, add swap so the one-time build doesn't run out of memory:

```sh
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

### 4. Copy the code to the server

From your PC, in the `pairly` folder (this skips the build output, which is many GB):

```sh
rsync -a --delete --exclude target pairly-core/ USER@SERVER_IP:~/pairly-core/
```

(Once the repository is on GitHub or similar, `git clone` on the server works instead.)

### 5. Configure

On the server:

```sh
cd ~/pairly-core/crates/pairly-relay/deploy
cp env.example .env
sed -i "s/^PAIRLY_RELAY_TOKENS=.*/PAIRLY_RELAY_TOKENS=$(openssl rand -hex 16)/" .env
sed -i "s/^PAIRLY_RELAY_PUBLIC_HOST=.*/PAIRLY_RELAY_PUBLIC_HOST=SERVER_IP/" .env
cat .env         # check: a random token and your server's IP
```

### 6. Build and start

```sh
docker compose up -d --build      # the first build takes 5–15 minutes on a small VPS
docker compose logs relay | grep -A1 "Relay address"
```

It prints something like:

```text
Relay address (put it in the PC's config.toml under [relay]):
  pairly-relay://3f9c…@203.0.113.7:47200/n3k2…
```

Copy that whole `pairly-relay://…` line. It contains the access token, so treat it like a
password.

The container restarts automatically after crashes and reboots.

### 7. Point your PC at it

Add the address to `~/.config/pairly/config.toml` on the PC:

```toml
[relay]
address = "pairly-relay://3f9c…@203.0.113.7:47200/n3k2…"
```

Then restart the daemon:

```sh
systemctl --user restart pairlyd
journalctl --user -u pairlyd -n 20 | grep -i relay     # "connected to relay"
```

### 8. Let the phone learn it, then test

1. Open Pairly on the phone while it's connected to the PC normally (same Wi-Fi or USB
   tethering). The PC passes the relay address over the encrypted link.
2. Turn off the phone's Wi-Fi and use mobile data. Within about a minute the PC's card shows
   **Connected · Internet**, and notifications, clipboard and files keep working.
3. Turn Wi-Fi back on: the link moves back to **Local network** on its own.

---

## Running it without Docker

Build on any Linux machine with Rust (`rustup`). The binary has no runtime dependencies besides
glibc, so build on the same distribution as the server, or on the server itself:

```sh
cd pairly-core
cargo build --release -p pairly-relay
scp target/release/pairly-relay crates/pairly-relay/deploy/pairly-relay.service USER@SERVER_IP:
```

On the server:

```sh
sudo install -m755 pairly-relay /usr/local/bin/
sudo install -m644 pairly-relay.service /etc/systemd/system/
echo "PAIRLY_RELAY_TOKENS=$(openssl rand -hex 16)" | sudo tee /etc/pairly-relay.env >/dev/null
echo "PAIRLY_RELAY_PUBLIC_HOST=SERVER_IP" | sudo tee -a /etc/pairly-relay.env >/dev/null
sudo chmod 600 /etc/pairly-relay.env
sudo systemctl enable --now pairly-relay
journalctl -u pairly-relay | grep -A1 "Relay address"
```

## Operating it

| Task | Docker | systemd |
|---|---|---|
| Logs and stats (a line every 5 min) | `docker compose logs -f relay` | `journalctl -fu pairly-relay` |
| Update | `rsync` the code again, then `docker compose up -d --build` | rebuild, `install`, `systemctl restart pairly-relay` |
| Stop | `docker compose down` | `systemctl stop pairly-relay` |

- **Keep the certificate.** It lives in the `relay-data` volume (Docker) or
  `/var/lib/private/pairly-relay` (systemd). If it's lost, the relay makes a new one with a new
  pin, and you must update the address in the PC's config. Paired phones then learn the new one
  on their next LAN connection.
- **Rotate the token:** change `PAIRLY_RELAY_TOKENS`, restart, and update the PC's config.
  Several comma-separated tokens are accepted during a switch-over.
- **Limits:** `PAIRLY_RELAY_RATE_LIMIT_MBPS` caps each pair's bandwidth. Connections per IP
  (32) and rooms per device (64) are flags; see `pairly-relay --help`.

## Settings

Every flag has an environment variable:

| Flag | Variable | Default |
|---|---|---|
| `--listen` | `PAIRLY_RELAY_LISTEN` | `[::]:47200` (IPv4 and IPv6) |
| `--data-dir` | `PAIRLY_RELAY_DATA` | `/var/lib/pairly-relay` |
| `--token` | `PAIRLY_RELAY_TOKENS` (comma separated) | none (open relay) |
| `--public-host` | `PAIRLY_RELAY_PUBLIC_HOST` | used only to print the address |
| `--public-port` | `PAIRLY_RELAY_PUBLIC_PORT` | the listen port |
| `--rate-limit-mbps` | `PAIRLY_RELAY_RATE_LIMIT_MBPS` | `0` (none) |
| `--max-conns-per-ip` | `PAIRLY_RELAY_MAX_CONNS_PER_IP` | `32` |
| `--max-rooms-per-conn` | `PAIRLY_RELAY_MAX_ROOMS_PER_CONN` | `64` |
| `--stats-interval` | `PAIRLY_RELAY_STATS_INTERVAL` | `300` seconds |

Logging: `PAIRLY_RELAY_LOG=debug`.

## Trying it locally

```sh
cargo run -p pairly-relay -- --data-dir /tmp/relay --listen 0.0.0.0:47200 \
    --token dev --public-host <this PC's IP as the phone sees it>
```

Put the printed address in the PC's `[relay]` config. To force traffic through the relay while
both devices share a network, set `[lan] mdns = false` on the PC and restart `pairlyd`.
