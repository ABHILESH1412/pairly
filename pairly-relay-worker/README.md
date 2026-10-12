# pairly-relay-worker

Pairly's internet relay as a free Cloudflare Worker: your phone reaches your PC when they aren't
on the same network (mobile data, another Wi-Fi), with no server of your own.

- **Blind:** it only joins two WebSockets that name the same room and copies bytes between them.
  Rooms are derived from each pairing's secret, and everything inside is the devices' end-to-end
  encrypted session, so Cloudflare never sees device names, ids or what is sent. It does see
  your IP addresses, when you're connected and roughly how much passes (Pairly pads messages to
  hide exact sizes).
- **Private:** an access token keeps everyone else off your relay.
- **Light on the phone's battery:** an idle connection only pings every 2 minutes, and
  Cloudflare answers those pings without waking the relay.

It's the WebSocket twin of [`pairly-relay`](../pairly-core/crates/pairly-relay/README.md), the
QUIC relay for your own server. Pairly takes either address.

## Set it up (about 5 minutes)

You need a free [Cloudflare account](https://dash.cloudflare.com/sign-up) and Node.js on your PC.

```sh
cd pairly-relay-worker
npm install
npx wrangler login        # opens the browser: allow
npx wrangler deploy       # prints https://pairly-relay.<your-subdomain>.workers.dev
```

The first deploy may ask you to pick a `workers.dev` subdomain; any name works.

Then give it an access token (any long random string; keep it secret):

```sh
openssl rand -hex 24 | tee relay-token.txt | tr -d '\n' | npx wrangler secret put TOKEN
```

Your relay address is `wss://TOKEN@pairly-relay.<your-subdomain>.workers.dev`, with the token
from `relay-token.txt`. Put it in Pairly on your PC:

- **Linux:** Settings → Away from home → Relay address, or `[relay] address = "wss://…"` in
  `~/.config/pairly/config.toml`
- **Windows:** Settings → Connections → Relay

Your phone learns it from the PC the next time they're connected at home. Delete
`relay-token.txt` once the address is saved.

## Limits

Cloudflare's free plan allows 100,000 requests a day (WebSocket messages count 20 to a request)
and 13,000 GB-seconds of compute a day; idle connections cost nothing. A day of notifications,
texts and clipboard takes a few hundred requests, 1 GB of files about 800. That's plenty for a
household. Screen sharing stays on the local network.

## Develop

```sh
echo TOKEN=test-token > .dev.vars
npx wrangler dev          # ws://test-token@127.0.0.1:8787
# in pairly-core:
PAIRLY_WS_RELAY=ws://test-token@127.0.0.1:8787 cargo test -p pairly-relay -- --ignored
```

The protocol is described at the top of [`src/index.js`](src/index.js) and in
`pairly-core/crates/pairly-transport-relay/src/ws.rs`.
