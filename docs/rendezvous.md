# Running the rendezvous server

Two machines on different home connections cannot address each other. Both sit behind a router
that has no inbound mapping until something inside creates one, and neither knows the other's
public address. The rendezvous server is the fixed point they can both reach: each sends it a
datagram, it observes where that datagram came from, and it tells each side where the other is.
Then both send to the address they were given and each side's outbound packet opens the mapping
its router needs for the other's.

That is all it does. It carries no video, no input, and no keys.

## What it is trusted with

Nothing.

It cannot read a session and cannot forge one. The Noise handshake between the two peers proves
who is at each end regardless of what the server said, so a hostile or compromised server can
refuse to introduce two peers, or introduce them to a wrong address — and a wrong address simply
fails to handshake. Nothing here needs the server to be honest, only reachable.

This is why running it on a cheap box is not a compromise, and why it is worth self-hosting
rather than depending on somebody else's signalling service.

## Deploying it

From a clone of this repository, on the machine that will run it:

```sh
docker compose -f deploy/rendezvous/compose.yaml up -d --build
```

The image is built from source and comes out at about **1.5 MB**: one statically linked binary
in an otherwise empty image, with no shell, no package manager, and no libraries. Every
dependency in the binary is pure Rust, which is what makes that possible.

To watch it:

```sh
docker logs -f prism-rendezvous
```

It prints a line when it starts and a summary once a minute. `--verbose` in the compose file's
`command` makes it print a line per message, which is what to reach for when a peer is not
connecting.

## Host networking is not a preference

The compose file uses `network_mode: host`, and swapping it for a published port breaks the
server in a way that looks like something else entirely.

The server's whole job is to observe the source address of a datagram and tell the other peer
to send there. Docker's bridge networking rewrites that address whenever the userland proxy
handles the packet. Measured, with `-p 47500:47500/udp` on Docker Desktop:

```
host registered ... reachable at 185.125.190.82:45762
client: the host is at 185.125.190.82:45762, and this machine appears at 185.125.190.82:20016
```

Both peers were on the same machine as the server, at `127.0.0.1`. Both were told to punch at an
address neither of them has. Nothing connected, and nothing said why — it looks exactly like a
NAT that will not traverse. Host networking is the only arrangement where what the server
observes is what the peer actually is.

## Making it reachable

The server needs one **UDP** port open. It defaults to 47300.

On a VPS, that means the provider's firewall and whatever runs on the box:

```sh
ufw allow 47300/udp
```

On a machine at home — which works, and is the cheapest option if a machine is already
running — it also needs a port forward on the router:

| Field | Value |
|---|---|
| Protocol | **UDP** (not TCP, and not "both" if that costs a rule) |
| External port | 47300 |
| Internal address | the server's LAN address |
| Internal port | 47300 |

A home connection also needs its public address to be stable, or peers configured with it will
lose the server when it changes. A dynamic DNS name avoids that; peers accept a name as readily
as an address.

There is one thing a home deployment cannot do that a VPS can: if the *host* being streamed
from is on the same connection as the rendezvous server, some routers will not hairpin a peer
back to their own public address. That is a limitation of the router rather than of the
protocol, and it only affects a client on that same network — which does not need the server
anyway, since it can address the host directly.

## Checking it works

Point a host and a client at it:

```sh
# on the machine being streamed from
prism-cli host --bind 0.0.0.0:0 --rendezvous <server>:47300

# on the machine watching
prism-cli client --rendezvous <server>:47300
```

Both print the address the server observed for them. Those are the numbers that matter: if
either says something in a private range (`10.`, `172.16.`–`172.31.`, `192.168.`) or a loopback
address, the server is behind something that rewrote it and no amount of retrying will help.

## What it holds

Two hash maps and a socket. A registered host costs a few tens of bytes; a challenge in flight
costs the same and expires in ten seconds. Registrations expire ninety seconds after the last
keepalive, and hosts send one every fifteen. Nothing is written to disk, so there is nothing to
back up and nothing to migrate — a server that is restarted is repopulated by its hosts within
fifteen seconds.

## What it refuses

- **A registration it cannot verify.** A host claims a public key, and the server answers with a
  challenge only the holder of the matching private key can read. Without that, anyone who has
  ever seen a host's key — every machine it has paired with — could register it and point its
  clients elsewhere. They would learn nothing, but the host would be unreachable.
- **A registration that moves without a new proof.** A keepalive refreshes a registration only
  from the address it was made at.
- **Introductions faster than one every 200 ms per caller.** An introduction makes the server
  send a datagram to a host. It cannot be aimed — it goes to the registered address, not one the
  caller chose, so it is not an amplifier — but without a floor it is a way to flood a host with
  wake-ups.
- **Everything else.** A caller that fails to prove a key gets silence, not an explanation, and a
  key the server has never heard of gets `unknown-host` rather than a description of why.
