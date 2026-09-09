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
cp deploy/rendezvous/.env.example deploy/rendezvous/.env
$EDITOR deploy/rendezvous/.env
docker compose -f deploy/rendezvous/compose.yaml up -d --build
```

One container, built from source, coming out at **2.4 MB** — one statically linked binary in an
otherwise empty image, with no shell, no package manager and no libraries. It writes nothing, so
there is no volume, and it answers no HTTP, so there is nothing to put a certificate in front of.

Two ports have to be open: **47300/udp** for signalling and **47301/udp** for the relay.

#### It reports; nothing asks it

The operator dashboard shows how many hosts a region is holding, what it is relaying and what
that is costing its link. Those numbers leave the region on an outbound connection, once a
minute, to `accounts.presm.kr`:

```sh
--report-to https://accounts.presm.kr   # where the dashboard reads from
--link-mbps 1000                        # what this machine's plan will carry
PRISM_REPORT_TOKEN=…                    # what proves the report came from a region
```

Without all three the region reports nothing and works exactly as it did.

The direction is the whole point. Being *asked* would mean every region answering HTTPS from
the internet — a hostname each, a certificate each, a proxy each, and two more ports open on a
box whose entire attack surface today is one UDP port and SSH. That would turn "a machine, a
port, and an address record" into something nobody adds a region casually. Pushing costs an
outbound connection and nothing else, and it means the dashboard reads its own database rather
than waiting on five machines scattered around the world before it can draw anything.

`--link-mbps` is configuration because a server cannot discover what its plan allows. A wrong
number there makes a busy region look idle or an idle one look full.

**Every total a region reports is since it last started.** It keeps no state — see below — so
there is nowhere for a monthly figure to live, and one would reset without warning on every
deploy. The uptime travels beside the totals so whoever reads them knows what window they cover.

#### Accounts are not here

A region keeps none. They live in `packages/accounts`, a Cloudflare Worker over D1 at a name of
its own, and this server has no `--accounts` flag in its compose file.

That split is deliberate and the two halves want opposite things. Signalling is on the session
path — when hole punching fails, the relay carries the video and adds its own distance to every
round trip — and it is soft state that any server can serve, so it is replicated per region and
a client uses whichever answers first. Accounts are on nobody's path: no session, direct or
relayed, ever calls the account server. What they need is not to be near anybody, it is to
survive, and a JSON file on one cheap disk was the only copy of every account's sealed key and
second factor.

So the names differ, and must:

```
rv.presm.kr.        A  203.0.113.10   # a region
rv.presm.kr.        A  198.51.100.20  # another region
accounts.presm.kr.  → the Worker      # one place, because accounts are state
```

A name that round-robined between regions would sign somebody in against whichever server
answered and then tell them, on the next call, that their account does not exist.

Build the image on the machine that will run it, which is what the command above does. Building
it elsewhere for another architecture goes through emulation — though with enough cores that can
still beat a single-core server, so it is worth measuring rather than assuming.

To watch it:

```sh
docker logs -f prism-rendezvous
```

It prints a line when it starts and a summary once a minute. `--verbose` in the compose file's
`command` makes it print a line per message, which is what to reach for when a peer is not
connecting.

### On a push, by itself

`.github/workflows/deploy.yml` builds the image once and puts it where it runs. A push to
`develop` deploys to the `staging` environment and a push to `main` to `production`, and only
after CI has passed on that commit — a pipeline whose point is that what reaches a server is
what the tests ran against.

Which machine each name means is set in the repository's environment settings rather than here,
so pointing both at one box while there is only one box is a change to configuration and not to
code. Each environment needs:

| Secret | What it is |
|---|---|
| `DEPLOY_HOST` | The machine's address. |
| `DEPLOY_USER` | The account to connect as. It has to be able to run `docker`. |
| `DEPLOY_PATH` | The directory on it holding `.env`, and where the compose file is put. |
| `DEPLOY_SSH_KEY` | A private key whose public half is in that account's `authorized_keys`. |
| `DEPLOY_HOST_KEY` | The server's own key, from `ssh-keyscan <host>`. Pinned rather than accepted on sight: a deploy that trusts whatever answers can be pointed at something else by anything that answers first. |
| `DEPLOY_PORT` | Optional. Defaults to 22. |

The `.env` is **not** deployed and must be put on each server once, by hand. It holds the mail
key, and a server's secrets belong to the server rather than to a repository that builds it.

Create both environments **before** the first run and put the secrets on the environments rather
than on the repository, so a staging credential cannot reach the production box. Naming an
environment that does not exist creates it on first use with no protection at all. Give
`production` a required reviewer and a deployment branch policy limiting it to `main`.

Four things about this that are easy to be caught by:

- `workflow_run` only fires for a workflow file that is on the **default branch**. Until this
  file is on `main`, a push to `develop` builds nothing.
- A `workflow_run` job runs with **this** repository's secrets whatever triggered it, and a pull
  request from a fork triggers CI. The branch filter does not help — it matches the head branch
  of the run that fired, and every fork has a `main`. The build job therefore checks that the run
  came from a push in this repository, and those two conditions are the only thing standing
  between a stranger's Dockerfile and the machine that keeps everybody's accounts.
- The image is deployed **by digest**, not by tag. A tag is a name somebody can move.
- The image is built for both architectures, because the machines it runs on are not all the
  same one and an image that only runs where it was built is not a deployable artifact.

A push deploy will not start on a machine that is already running the server another way. Osaka
runs it as a systemd user service on the same UDP port; stop and disable that first
(`systemctl --user disable --now prism-rendezvous`) or the container will fail to bind.

To put a known-good build back without pushing a commit whose only purpose is to trigger a
deploy, run the workflow by hand and choose the environment.

### Nothing here needs backing up

A region is entirely disposable, which is the point of it holding no accounts. The signalling
registry lives in memory and its hosts repopulate it within fifteen seconds of a restart; the
image is rebuilt from a commit. Destroying one of these machines costs the sessions in flight on
it and nothing else.

The one thing that could not be reconstructed — each account's sealed private key, its TOTP
secret and the machines it knows — is no longer on a disk anybody here owns. It is in D1, where
keeping copies of it is somebody else's job.

### As a plain binary, with no root at all

The server binds one unprivileged port and needs no privilege at any point.

Build it **on the machine that will run it**. It used to cross-compile from anywhere with a
Rust toolchain and no C compiler at either end, and that stopped being true when the account
API brought in a TLS client: `reqwest` pulls in `rustls`, which pulls in `ring`, which is C and
assembly. Cross-compiling from a Mac now ends here:

```
error: failed to run custom build command for `ring v0.17.14`
  failed to find tool "x86_64-linux-musl-gcc"
```

Installing a musl cross toolchain fixes it, and so does not needing one:

```sh
# on the server, which already has the right C toolchain for itself
cargo build --release -p prism-rendezvous
install -D target/release/prism-rendezvous ~/bin/prism-rendezvous
```

As a user service, so it survives a reboot and restarts if it dies:

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

That is the whole of it. A region run this way writes nothing at all, so a hardened unit needs
no writable path:

```ini
ProtectSystem=strict
ProtectHome=read-only
```

The account half is still in this binary and still works — `--accounts` turns it on, and it
keeps `accounts.json` and `sessions.json` beside the path it is given. It is not what runs any
more. `packages/accounts` serves the same API from a Worker over D1, because the store was the
one piece of state here with no second copy, and a JSON file rewritten in full on one disk of
one cheap server is a poor place for every account's sealed key and second factor.

What is stored is worth knowing either way. A session is kept as the **SHA-256 of** its token
rather than the token: a session token is a bearer credential, so whoever reads one is that
account until it expires, and what is written down is enough to recognise a token that comes
back and no use at all to somebody who reads it. There is no slower hash on purpose — a token is
32 bytes of randomness, so there is no smaller space to search than the whole one.

Sessions are written down so that restarting does not sign everybody out. Before that they lived
in memory, which made every deployment a forced sign-in on every machine, and turned "stay
signed in" into a promise that held until the next update.

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
