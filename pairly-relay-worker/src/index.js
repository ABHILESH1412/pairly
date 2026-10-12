// Pairly's internet relay as a Cloudflare Worker: the WebSocket twin of `pairly-relay`.
//
// Blind: it only connects two WebSockets that name the same room and copies bytes between them.
// Rooms are derived from each pairing's secret, and everything it copies is an end-to-end
// encrypted Noise session, so it never learns device names, ids or what is sent.
//
// Every request carries `Authorization: Bearer <token>` (the TOKEN secret), and names a room
// (32 hex digits) and the device's member id in that room (32 hex digits, derived from the room
// and the device, so a restarted device replaces its old connection instead of adding one):
//
//   GET /v1/listen/<room>?m=<member>
//       Wait in the room. The relay sends "present" / "absent" as the other member comes and
//       goes, and "call <id>" when it dials in.
//   GET /v1/dial/<room>?m=<member>
//       Call the other member (404 if it isn't listening). "matched" once it answers; from then
//       on, binary messages are passed to it.
//   GET /v1/answer/<room>?m=<member>&call=<id>
//       Answer a call: "matched", then the same pipe from this side.
//
// One Durable Object per room. Its sockets hibernate: an idle room costs nothing, and pings
// (WebSocket protocol pings, or the text "ping") are answered without waking it.
import { DurableObject } from "cloudflare:workers";

const HEX32 = /^[0-9a-f]{32}$/;
/// Two devices per room; more sockets than this is a misbehaving client.
const MAX_MEMBERS = 2;
const MAX_SOCKETS = 16;
/// The clients send at most 64 KiB per message.
const MAX_MESSAGE = 256 * 1024;

const reply = (status, text) => new Response(`${text}\n`, { status });

async function tokenOk(request, env) {
  const auth = request.headers.get("Authorization") ?? "";
  if (!env.TOKEN || !auth.startsWith("Bearer ")) {
    return false;
  }
  const enc = new TextEncoder();
  const given = enc.encode(auth.slice(7));
  const wanted = enc.encode(env.TOKEN);
  return given.byteLength === wanted.byteLength && crypto.subtle.timingSafeEqual(given, wanted);
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const [, version, role, room] = url.pathname.split("/");
    if (version !== "v1") {
      return reply(200, "pairly relay");
    }
    if (request.headers.get("Upgrade") !== "websocket") {
      return reply(426, "expected a WebSocket");
    }
    if (!(await tokenOk(request, env))) {
      return reply(403, "denied");
    }
    const member = url.searchParams.get("m") ?? "";
    const call = url.searchParams.get("call") ?? "";
    if (!["listen", "dial", "answer"].includes(role) || !HEX32.test(room ?? "") || !HEX32.test(member)) {
      return reply(400, "bad request");
    }
    if (role === "answer" && !HEX32.test(call)) {
      return reply(400, "bad call");
    }
    const stub = env.ROOMS.get(env.ROOMS.idFromName(room));
    return stub.fetch(`https://room/${role}?m=${member}&call=${call}`, request);
  },
};

export class Room extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  /// Sockets with this tag, and what each one is.
  sockets(tag) {
    return this.ctx.getWebSockets(tag).map((ws) => ({ ws, info: ws.deserializeAttachment() ?? {} }));
  }

  listeners() {
    return this.sockets("listen");
  }

  accept(tags, info) {
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, tags);
    server.serializeAttachment(info);
    return { client, server };
  }

  async fetch(request) {
    const url = new URL(request.url);
    const role = url.pathname.slice(1);
    const member = url.searchParams.get("m");
    if (this.ctx.getWebSockets().length >= MAX_SOCKETS) {
      return reply(429, "room full");
    }
    if (role === "listen") {
      return this.listen(member);
    }
    if (role === "dial") {
      return this.dial(member);
    }
    return this.answer(member, url.searchParams.get("call"));
  }

  listen(member) {
    const listeners = this.listeners();
    const members = new Set(listeners.map((l) => l.info.member));
    if (!members.has(member) && members.size >= MAX_MEMBERS) {
      return reply(429, "room full");
    }
    // A device that reconnects replaces its old (maybe already dead) listener.
    for (const old of listeners.filter((l) => l.info.member === member)) {
      old.ws.serializeAttachment({ ...old.info, replaced: true });
      try {
        old.ws.close(4000, "replaced");
      } catch {}
    }
    const { client, server } = this.accept(["listen", `m:${member}`], { kind: "listen", member });
    if (listeners.some((l) => l.info.member !== member)) {
      server.send("present");
      this.tellOthers(member, "present");
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  dial(member) {
    const other = this.listeners().find((l) => l.info.member !== member && !l.info.replaced);
    if (!other) {
      return reply(404, "no peer");
    }
    const call = [...crypto.getRandomValues(new Uint8Array(16))]
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    const { client } = this.accept([`call:${call}`], { kind: "dial", call, member });
    other.ws.send(`call ${call}`);
    return new Response(null, { status: 101, webSocket: client });
  }

  answer(member, call) {
    const dialer = this.sockets(`call:${call}`).find((s) => s.info.kind === "dial" && !s.info.matched);
    if (!dialer || dialer.info.member === member) {
      return reply(410, "call gone");
    }
    const { client, server } = this.accept([`call:${call}`], { kind: "answer", call, member, matched: true });
    dialer.ws.serializeAttachment({ ...dialer.info, matched: true });
    dialer.ws.send("matched");
    server.send("matched");
    return new Response(null, { status: 101, webSocket: client });
  }

  /// The other end of a call's pipe.
  peerOf(ws, info) {
    return this.ctx.getWebSockets(`call:${info.call}`).find((other) => other !== ws);
  }

  tellOthers(member, text) {
    for (const l of this.listeners()) {
      if (l.info.member !== member && !l.info.replaced) {
        try {
          l.ws.send(text);
        } catch {}
      }
    }
  }

  webSocketMessage(ws, message) {
    const info = ws.deserializeAttachment() ?? {};
    if (info.kind === "listen") {
      return; // Listeners only receive.
    }
    if (typeof message === "string" || !info.matched || message.byteLength > MAX_MESSAGE) {
      ws.close(1008, "unexpected message");
      return;
    }
    const peer = this.peerOf(ws, info);
    if (!peer) {
      ws.close(1000, "the other side left");
      return;
    }
    try {
      peer.send(message);
    } catch {
      ws.close(1011, "the other side left");
    }
  }

  webSocketClose(ws, code, reason) {
    this.gone(ws);
    try {
      ws.close(1000, "bye");
    } catch {}
  }

  webSocketError(ws) {
    this.gone(ws);
  }

  gone(ws) {
    const info = ws.deserializeAttachment() ?? {};
    if (info.kind === "listen") {
      // Absent only if the member has no other (replacement) listener.
      const still = this.listeners().some((l) => l.ws !== ws && l.info.member === info.member);
      if (!info.replaced && !still) {
        this.tellOthers(info.member, "absent");
      }
    } else if (info.call) {
      const peer = this.peerOf(ws, info);
      if (peer) {
        try {
          peer.close(1000, "the other side left");
        } catch {}
      }
    }
  }
}
