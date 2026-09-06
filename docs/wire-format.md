# Prism Wire Format v1

All integers are **little-endian**. All packets travel over a single UDP flow and are
sealed with AES-GCM after the Noise_IK handshake (M5); the layouts below describe the
**plaintext** that goes inside that seal.

The first byte of every packet is the channel tag.

| Channel | Value | Direction | Purpose |
|---|---|---|---|
| `Control`  | 0 | both  | session setup, codec negotiation, cursor updates |
| `Video`    | 1 | host → client | encoded video slices |
| `Audio`    | 2 | host → client | Opus frames |
| `Input`    | 3 | client → host | keyboard, mouse, gamepad |
| `Feedback` | 4 | client → host | frame ACKs (LTR), clock sync, congestion signals |

## Size limits

| Constant | Value | Reason |
|---|---|---|
| `MAX_PACKET_SIZE` | 1200 | Stays under the safe PMTU floor so packets never fragment |
| `CONTROL_HEADER_LEN` | 2 | Channel tag + control message type |
| `CLOCK_PING_LEN` | 10 | Fixed size |
| `CLOCK_PONG_LEN` | 26 | Fixed size |
| `INPUT_PACKET_LEN` | 15 | Fixed size, every kind |
| `CURSOR_POSITION_LEN` | 18 | Fixed size |
| `VIDEO_HEADER_LEN` | 20 | Channel tag + video header |
| `MAX_VIDEO_PAYLOAD` | 1180 | `MAX_PACKET_SIZE - VIDEO_HEADER_LEN` |
| `FEEDBACK_PACKET_LEN` | 17 | Fixed size |

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
20      ..    payload         bytes  <= 1180
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

Types 3 and above are reserved. A decoder that sees one rejects the packet.

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

## Reserved

The `Audio` payload layout is defined in M7. Until then only its channel tag is fixed.

## Test vectors

`packages/protocol/vectors.json` is the single source of truth. Both `cargo test`
and `vitest` assert against it. **Change the vectors before changing either
implementation.**
