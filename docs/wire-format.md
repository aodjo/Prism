# Prism Wire Format v1

All integers are **little-endian**. All packets travel over a single UDP flow and are
sealed with ChaCha20-Poly1305 under keys a Noise_IK handshake produced. **Every layout below
describes the plaintext inside that seal** — nothing in this document ever appears on the
wire in the form it is written here.

The first byte of every packet is the channel tag.

| Channel | Value | Direction | Purpose |
|---|---|---|---|
| `Control`  | 0 | both  | session setup, codec negotiation, cursor updates |
| `Video`    | 1 | host → client | encoded video slices |
| `Audio`    | 2 | host → client | Opus frames |
| `Input`    | 3 | client → host | keyboard, mouse, gamepad |
| `Feedback` | 4 | client → host | frame ACKs (LTR), clock sync, congestion signals |
| `Fec`      | 5 | host → client | Reed-Solomon parity for video slices |
| `File`     | 6 | both  | files moving between the two machines |

## Size limits

| Constant | Value | Reason |
|---|---|---|
| `MAX_PACKET_SIZE` | 1200 | What leaves the socket. Stays under the safe PMTU floor so packets never fragment |
| `SEAL_OVERHEAD` | 24 | 8-byte nonce counter in the clear + 16-byte authentication tag |
| `MAX_PLAINTEXT_SIZE` | 1176 | `MAX_PACKET_SIZE - SEAL_OVERHEAD`. Every layout below is budgeted against this, not against `MAX_PACKET_SIZE` |
| `CONTROL_HEADER_LEN` | 2 | Channel tag + control message type |
| `CLOCK_PING_LEN` | 10 | Fixed size |
| `CLOCK_PONG_LEN` | 26 | Fixed size |
| `INPUT_PACKET_LEN` | 15 | Fixed size, every kind |
| `CURSOR_POSITION_LEN` | 18 | Fixed size |
| `VIDEO_HEADER_LEN` | 20 | Channel tag + video header |
| `MAX_VIDEO_PAYLOAD` | 1156 | `MAX_PLAINTEXT_SIZE - VIDEO_HEADER_LEN` |
| `FEEDBACK_PACKET_LEN` | 17 | Fixed size |
| `FEC_HEADER_LEN` | 20 | Same as the video header, so a parity shard is exactly as long as the data shards it repairs |
| `MAX_FEC_PAYLOAD` | 1156 | `MAX_PLAINTEXT_SIZE - FEC_HEADER_LEN` |
| `AUDIO_HEADER_LEN` | 13 | Channel tag + sequence + capture timestamp |
| `MAX_AUDIO_PAYLOAD` | 1163 | `MAX_PLAINTEXT_SIZE - AUDIO_HEADER_LEN`. Far more than needed, which is the point |

## Sealing

Every datagram on the wire is:

```
[8B nonce counter, little-endian, in the clear][ciphertext][16B Poly1305 tag]
```

The counter is the only thing an observer can read. The channel tag and every header are
inside the seal, so a packet's size is all that leaks — not whether it carries video, a
keystroke, or an acknowledgement.

**Nonce.** 96 bits: the counter in the low 64, zero above. One counter per direction,
never reused, never allowed to wrap. Reuse would leak both the plaintexts' XOR and the
authentication key, so a session that reaches the ceiling ends rather than wrapping.

**Replay.** A 64-counter sliding window per direction. The window has to slide rather than
demand order, because UDP reorders and a receiver insisting on monotonic counters would
discard good packets on every path with jitter. **The tag is verified before the counter is
judged** — the other order would let anyone who can guess a counter push the window forward
without holding the key, and every genuine packet after it would fall outside.

**Cipher.** ChaCha20-Poly1305 rather than AES-256-GCM. Measured on the client machine
(Apple Silicon, stable Rust), a full 1176-byte packet costs **3.7 us** to seal and open with
ChaCha against **10.9 us** with AES-GCM, because the pure-Rust `aes` crate reaches its
hardware instructions on x86 at run time but not on stable aarch64. ChaCha needs no
per-platform story anywhere.

## Handshake

Two messages, before anything above exists. `Noise_IK_25519_ChaChaPoly_BLAKE2s`, with
`prism-handshake-v1` as the prologue so the version cannot be stripped or downgraded.

| Message | Direction | Size |
|---|---|---|
| init | dialler → answerer | 96 + payload |
| response | answerer → dialler | 48 + payload |

`IK` means the dialler already knows the answerer's static key — pairing put it there — which
is what makes one round trip enough. Both sides then check the key they ended up facing
against what pairing recorded; a peer that does not match gets **silence**, not a refusal,
because a refusal tells a scan it found a live host.

Neither message is sealed, and neither needs a type byte to be told apart from a sealed
packet: authentication itself separates them. A sealed packet never verifies as a handshake
message, and a handshake message never opens under a session key.

**Losing a message.** The dialler resends the *same bytes* until it is answered — a second
message would carry a second ephemeral key and the answerer would derive keys the dialler
has discarded. The answerer replies to a repeat with the bytes it sent before, and stays
ready to do so until the first sealed packet arrives, which is its only proof that its
answer got through.

## Video packet (channel 1)

```
offset  size  field           type   notes
0       1     channel         u8     always 1
1       4     frame_id        u32    monotonic, wraps
5       2     slice_id        u16    slice index within the frame
7       2     pkt_idx         u16    packet index within the slice
9       2     pkt_count       u16    total packets in this slice
11      1     flags           u8     see below
12      8     capture_ts_us   u64    host clock, microseconds
20      ..    payload         bytes  <= 1156
```

### Flags

| Bit | Mask | Name | Meaning |
|---|---|---|---|
| 0 | `0x01` | `IDR` | slice belongs to an IDR frame |
| 1 | `0x02` | `LAST_OF_FRAME` | last slice of this frame |
| 2 | `0x04` | `LTR_REF` | frame is marked as a long-term reference |

Bits 3-7 are reserved and MUST be zero. A decoder that sees a non-zero reserved bit
rejects the packet rather than guessing.

`capture_ts_us` is stamped once per frame at capture time and copied into every packet
of that frame. It is the anchor for the whole latency instrumentation chain.

## Feedback packet (channel 4)

```
offset  size  field           type   notes
0       1     channel         u8     always 4
1       4     last_frame_id   u32    highest fully received frame
5       4     recv_bitmap     u32    bit N set = frame (last_frame_id - 1 - N) received
9       8     client_ts_us    u64    client clock, microseconds
```

`recv_bitmap` drives LTR reference invalidation: the host encodes against the newest
frame the client has confirmed, so packet loss never forces an IDR.

`client_ts_us` doubles as the clock-sync sample.

## Control packets (channel 0)

A control packet carries a message type after the channel tag.

```
offset  size  field           type   notes
0       1     channel         u8     always 0
1       1     control         u8     message type
2       ..    payload         bytes  depends on the type
```

| Type | Value | Direction | Purpose |
|---|---|---|---|
| `ClockPing` | 0 | client → host | start a clock synchronisation exchange |
| `ClockPong` | 1 | host → client | answer with the host's send and receive times |
| `CursorPosition` | 2 | host → client | where the host's pointer is |
| `Goodbye` | 3 | host → client | the host is ending the session |

Types 4 and above are reserved. A decoder that sees one rejects the packet.

### Clock synchronisation

Both sides stamp frames against the Unix epoch, which only makes the numbers comparable
on one machine. Across machines the offset between the two clocks is unknown, and without
correcting for it a latency figure is meaningless — a client whose clock trails the host's
measures negative latency.

The exchange is Cristian's algorithm. The client sends a ping at `t1`; the host records
`t2` when it arrives and `t3` when it answers; the client records `t4` on receipt.

```
round trip = (t4 - t1) - (t3 - t2)
offset     = ((t2 - t1) + (t3 - t4)) / 2
```

`offset` is how far the host's clock is ahead of the client's. Subtracting it from a host
timestamp converts it to client time.

The estimate is only as good as the exchange was symmetric, so the sample with the
**smallest round trip** is kept and the others discarded: a fast exchange has had the least
opportunity to be delayed unevenly in one direction.

**ClockPing** (control type 0), 10 bytes:
```
offset  size  field    type   notes
2       8     t1       u64    client clock when the ping was sent
```

**ClockPong** (control type 1), 26 bytes:
```
offset  size  field    type   notes
2       8     t1       u64    echoed from the ping, so the client can pair the reply
10      8     t2       u64    host clock when the ping arrived
18      8     t3       u64    host clock when the pong was sent
```

### Cursor position

The host keeps its cursor out of the captured video, so the client draws one. A cursor
baked into the frames inherits the whole video latency; one drawn by the client answers the
hand holding the mouse immediately.

That makes these messages **correction, not the source**. The client applies each movement
it sends the instant it happens and draws there. A reading carries everything the client
could not have known — the host's own user, a window warping the pointer, an edge it
clamped against.

**CursorPosition** (control type 2), 18 bytes:
```
offset  size  field           type   notes
2       8     sample_ts_us    u64    host clock when the pointer was read
10      2     x               u16    pixels from the left of the primary display
12      2     y               u16    pixels from the top
14      2     screen_width    u16    never zero
16      2     screen_height   u16    never zero
```

`sample_ts_us` is what makes a reading usable as correction rather than only as display.
The client stamps the input it sends in the same clock, so a reading already accounts for
every movement older than it: those are dropped and the newer ones replayed on top. Without
the replay the cursor would jump backwards by one round trip every time a reading arrived.

The screen size travels with every message rather than being negotiated once. It is four
bytes on a packet already this small, and it means a client that joins late, or misses the
message where the host changed resolution, is never left scaling against a screen that no
longer exists.

A screen of no pixels is refused at both ends. The client divides by these to place the
cursor, so a zero would either crash it or put the cursor nowhere.

### Goodbye

**Goodbye** (control type 3), 2 bytes: the header and nothing after it.

The host sends it when the session ends on its side — sharing was stopped, or Prism is
quitting — and the client closes its window when it arrives. Without it the client only
learns the host has gone by hearing nothing for its whole idle timeout, and goes on showing
the last picture for all of that time.

It is sent three times, because it travels over UDP like everything else and nothing
answers it. A client that misses all three still ends at its idle timeout, as it would have
before the message existed.

## Input packets (channel 3)

Fixed at 15 bytes for every kind. The two coordinate fields are reinterpreted per kind; a
tagged union with per-kind lengths would save a few bytes on a packet that is already tiny,
at the cost of a decoder that has to branch before it knows how much to read.

```
offset  size  field           type   notes
0       1     channel         u8     always 3
1       1     kind            u8     see below
2       8     origin_ts_us    u64    when it happened, in the HOST's clock
10      2     x               i16    depends on kind
12      2     y               i16    depends on kind
14      1     flags           u8     bit 0 = pressed
```

| Kind | Value | `x` | `y` | `flags` |
|---|---|---|---|---|
| `MouseMove` | 0 | dx, positive right | dy, positive down | — |
| `MouseButton` | 1 | button: 0 left, 1 right, 2 middle | — | pressed |
| `MouseScroll` | 2 | dx | dy | — |
| `Key` | 3 | USB HID usage code | — | pressed |

Motion is **relative**, not absolute: that is what a captured pointer produces and what a
game reads, and an absolute position would have to be scaled between two different screen
sizes and lose precision doing it.

Keys are identified by **USB HID usage code**. Windows and macOS each have their own
keyboard numbering and neither is portable, but both can be mapped from HID — which is
also what the client's input library reports, so the client side is the identity mapping.

`origin_ts_us` is carried **in the host's clock**, converted by the client before sending.
The client is the side that measures the offset between the two clocks, so it is the side
that can do the conversion; a host receiving a raw client timestamp could only compare it
against its own clock and get the offset back as latency.

Input is sent the instant it happens, with no pacing and no batching. It is the one path
where a few milliseconds are felt directly rather than seen.

## Audio packets (channel 2)

```
offset  size  field           type   notes
0       1     channel         u8     = 2
1       4     sequence        u32    monotonic, wraps
5       8     capture_ts_us   u64    host clock, the same one video carries
13      ..    payload         bytes  one Opus packet
```

**One frame, one packet, always.** A frame is 5 ms of 48 kHz stereo Opus, which at 128 kbps is
about 80 bytes and at 256 kbps peaked at **182** in measurement — against a 1163 byte budget.
Audio therefore never fragments, never needs reassembly, and a lost packet costs exactly one
frame rather than stalling a reassembly that would then be abandoned.

**No parity.** Opus carries its own in-band redundancy and its decoder conceals a missing
frame by continuing the pitch and spectrum of what came before. For a single frame that is
inaudible, where Reed-Solomon would spend bandwidth on every frame to repair the occasional
one. Repair belongs inside the payload, not around it.

**No pacing.** Audio is a fraction of a percent of the link and its frames are already 5 ms
apart. Spreading them would delay sound to smooth a burst that does not exist.

**The same clock as video.** `capture_ts_us` comes from the host clock the video packets carry,
which is what lets the client hold picture and sound to a common age — rather than locking them
together everywhere, which would give both the worse behaviour of the two.

## Parity packets (channel 5)

One Reed-Solomon parity shard, computed over a single slice's own packets.

```
offset  size  field           type   notes
0       1     channel         u8     always 5
1       4     frame_id        u32    frame the repaired slice belongs to
5       2     slice_id        u16    slice this parity repairs
7       1     data_count      u8     data shards in the protected block
8       1     parity_count    u8     parity shards generated for it
9       1     shard_index     u8     which parity shard this is, from zero
10      2     tail_len        u16    bytes in the slice's final data shard
12      8     capture_ts_us   u64    copied from the frame
20      ..    payload         bytes  one parity shard, exactly 1156 bytes
```

The header is exactly as long as the video header, and that is a constraint rather than a
coincidence. Every shard in a Reed-Solomon block must be the same length, so a parity shard
has to be a full `MAX_VIDEO_PAYLOAD`; a header one byte longer would push the packet past
`MAX_PACKET_SIZE` and fragment it.

`tail_len` is what makes recovery correct rather than merely possible. A receiver learns a
slice's true length from its final packet, so a slice whose final packet was lost and then
rebuilt from parity would have a length of zero — recovered bytes with no framing, handed to
the decoder with nothing reporting it. Every data shard but the last is exactly
`MAX_VIDEO_PAYLOAD`, so this one field recovers the length from any parity packet:

```
slice_len = (data_count - 1) * MAX_VIDEO_PAYLOAD + tail_len
```

A block holds at most 255 shards, data and parity together: GF(2^8) has 256 field elements
and the format stops one short so a shard count fits a byte. A decoder rejects a block
description that could not have been produced — a zero data or parity count, a shard index
not below the parity count, a tail length outside one to `MAX_VIDEO_PAYLOAD` — because the
recovery path is driven directly by these numbers.

## File packets (channel 6)

Files move in either direction, one at a time each way, through one folder on each machine.
The second byte is the message.

| Message | Value | Direction | Purpose |
|---|---|---|---|
| `Offer`   | 0 | either | a file is on its way, if the far side will have it |
| `Answer`  | 1 | either | whether it will, or why it will not |
| `Chunk`   | 2 | sender → receiver | one piece of a file that was accepted |
| `Report`  | 3 | receiver → sender | what has arrived, so the sender knows what to repeat |
| `List`    | 4 | either | what is the far machine offering |
| `Listing` | 5 | either | what it is offering |
| `Ask`     | 6 | either | send me that one |

```
Offer
offset  size  field       type   notes
0       1     channel     u8     always 6
1       1     type        u8     always 0
2       4     id          u32    chosen by the sender, unique within a session
6       8     size        u64    bytes in the file
14      4     chunks      u32    ceil(size / MAX_FILE_PAYLOAD)
18      2     name_len    u16    bytes of UTF-8 that follow
20      ..    name        bytes  a file name and nothing else

Answer                       Chunk
2   4  id        u32         2   4  id       u32
6   1  accepted  u8          6   4  index    u32
7   1  refusal   u8          10  ..  payload  bytes, at most 1166

Report                       Listing                     Ask
2   4  id        u32         2   1  more     u8          2   1  name_len  u8
6   4  have      u32         3   2  count    u16         3   ..  name      bytes
10  4  arrived   u32         5   ..  entries
                             entry: size u64, name_len u8, name bytes
```

Two numbers do the whole of the repair. `have` is how many chunks arrived in an unbroken run
from the start, so everything below it is settled and the sender can forget it. `arrived`
covers the thirty-two chunks after that: bit *i* set means chunk `have + i` is already there.
A sender stays within that window, because sending past it would be sending chunks the next
report has no way to describe.

Every chunk but the last is exactly `MAX_FILE_PAYLOAD`, which is what lets a receiver write
one that arrived out of order straight to `index * MAX_FILE_PAYLOAD` and hold nothing.

A name is refused at the wire rather than repaired: empty, longer than 255 bytes, either of
the two relative directories, or holding a separator or a null. What arrives is written into
one folder, and a name that had to be sanitised before it was safe is a name nobody meant to
send. A file lands under a temporary name and is only put in place once it is whole.

## Reserved

Nothing. Every channel tag from zero to six is defined; a decoder rejects anything above.

## Test vectors

`packages/protocol/vectors.json` is the single source of truth. Both `cargo test`
and `vitest` assert against it. **Change the vectors before changing either
implementation.**
