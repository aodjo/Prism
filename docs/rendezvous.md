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

Two ways, and the second is not a lesser one.

### As a container

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

### As a plain binary, with no root at all

The server binds one unprivileged port, reads nothing and writes nothing, so it needs no
privilege at any point. Every dependency is pure Rust, which means it links statically against
musl and can be cross-compiled from any machine with a Rust toolchain — no C compiler, on
either end:

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
  cargo build --release -p prism-rendezvous --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/prism-rendezvous server:~/bin/
```

That is a **1.2 MB** file with nothing beside it. As a user service, so it survives a reboot
and restarts if it dies:

```ini
# ~/.config/systemd/user/prism-rendezvous.service
[Unit]
Description=Prism rendezvous server
After=network-online.target

[Service]
ExecStart=%h/bin/prism-rendezvous --bind 0.0.0.0:47300
Restart=always
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
MemoryMax=128M

[Install]
WantedBy=default.target
```

```sh
sudo loginctl enable-linger $USER      # the one command that needs root
systemctl --user enable --now prism-rendezvous
```

`enable-linger` is what makes a user service start at boot rather than at login. Without it
the server comes back only when somebody logs in, which on a headless machine is never.

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

### Why a home deployment eventually stops being enough

A peer on the same network as the rendezvous server cannot be introduced through it, in either
role, unless the router hairpins — that is, routes a packet aimed at its own public address
back inside. Many do not.

The failure is not obvious from the outside, so it is worth being precise about. A peer that
reaches the server over the LAN never sends anything through the router, so no inbound mapping
is created and the address the server observes is a private one. It then tells the far peer to
punch at something like `192.168.219.110`, which goes nowhere. Measured on a router that does
not hairpin:

```
host:   registered, reachable at 110.8.104.218:47230     ← the far peer, correct
client: this machine appears at 192.168.219.110:52750    ← the local peer, useless
client: no direct path opened; asking the rendezvous server to relay
```

Nothing can fix that from the server's side: the mapping the far peer would need does not
exist, because the local peer never opened one. Enabling hairpin on the router works where the
router offers it; moving the server off that network works everywhere.

So a home box is a fine place to *start* — the traffic is negligible and nothing is trusted to
it — but the machines sharing its network are exactly the ones it cannot introduce, and those
are usually the operator's own.

## More than one region

A relayed session pays the server's distance on every round trip, so one server is one place
that sessions can be fast from. Adding a region is a machine, a port, and an address record:

```
rv.example.com.  A  203.0.113.10   # Seoul
rv.example.com.  A  198.51.100.20  # Frankfurt
```

Every peer is configured with the **name**, not an address, so a region added here is one that
existing installations start using without being updated.

What the two sides do with that list is asymmetric, and deliberately so. Servers keep their
registry in memory and never talk to each other, so a host registered in Seoul is a host
Frankfurt has never heard of — which means both sides have to end up on the same one.

- A **host registers with every server**, because it cannot know which region the client will
  turn out to be nearest to. This costs one datagram per server per fifteen seconds: about
  eighty bits a second each, against a session measured in megabits.
- A **client asks all of them at once and uses whichever answers first.** There is no probe
  and no extra round trip — the request that finds the host is the same request that measures
  which server is nearest.

The server that answered is then the one the pair relays through if punching fails, which is
the right one by construction: it has just proved both that it knows the host and that it is
the closest of them to the client.

A server that is down or unreachable costs the clients near it and nobody else. A host that no
server has heard of is reported as not running only when *every* server says so.

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

The rendezvous half is two hash maps and a socket. A registered host costs a few tens of bytes;
a challenge in flight costs the same and expires in ten seconds. Registrations expire ninety
seconds after the last keepalive, and hosts send one every fifteen. None of it is written down —
a server that is restarted is repopulated by its hosts within fifteen seconds.

The account half writes two files, both beside the path given to `--accounts`:

| File | What is in it |
|---|---|
| `accounts.json` | One record per account: the salt, the verifier, the TOTP secret, the sealed private key, and the machines. |
| `sessions.json` | One record per signed-in session: **SHA-256 of** the token, the account it belongs to, and when it expires. |

The hash is the point of the second file. A session token is a bearer credential — whoever
reads one is that account until it expires — so what is stored is enough to recognise a token
that comes back and no use at all to somebody who reads the file. There is no slower hash here
on purpose: a token is 32 bytes of randomness, so there is no smaller space to search than the
whole one.

Sessions are written down so that restarting the server does not sign everybody out. Before
that they lived in memory, which made every deployment a forced sign-in on every machine, and
turned "stay signed in" into a promise that held until the next update.

A hardened unit has to be told about that directory, because the two files are the only things
this server writes:

```ini
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/prism
```

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
