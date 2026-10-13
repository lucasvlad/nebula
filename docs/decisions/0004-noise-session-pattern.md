# ADR 0004: Noise XX sessions with an allowlist check

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone / PR:** M2 / #5

## Context

After pairing (SPAKE2, brief §4), each device keeps an allowlist of peer
static X25519 public keys. Every later connection needs a Noise handshake
that:

- authenticates both sides by their static keys,
- lets us check the remote key against the allowlist and drop the connection
  before any file metadata is exchanged,
- works with an explicit `--addr host:port` and no mDNS. With `--addr` the
  initiator may not know which of its paired peers is listening there, while
  with mDNS it does know, because the advertisement carries the device ID
  (hash of the static key),
- ideally uses a pattern covered by the computational analysis we cite
  (brief source #5), and does not send static keys in cleartext.

## Decision

Sessions use **`Noise_XX_25519_ChaChaPoly_BLAKE2s`** with the prologue
`nebula/1`, implemented with `snow` in `crates/nebula-net/src/transport.rs`:

```text
-> e
<- e, ee, s, es        initiator checks the responder's key; if it is not
                       allowlisted it drops the connection, so it never sends
                       its own static key
-> s, se               responder checks the initiator's key; if it is not
                       allowlisted it drops the connection
<- (empty transport message)   "accepted"; only now does connect() return Ok
```

Handshake messages carry no payload, so nothing but keys crosses the wire
until both allowlist checks have passed. The final empty message is an
ordinary encrypted transport message. It exists so that an unpaired
initiator gets an error from `connect` rather than a session that fails on
first use. The whole handshake has a 10 s timeout.

With `--addr`, the initiator connects, learns who answered, and accepts any
allowlisted key. When mDNS has given a device ID, the caller can pass a
stricter check that only that device's key is accepted.

After the handshake, each application message (up to 4 MiB) is sent as one
or more Noise transport messages. The first plaintext starts with the total
length as a u32. A total above the limit is rejected before allocating, and
any decryption failure closes the session for good. Noise's nonce counters
make replayed, reordered or dropped frames fail to decrypt.

## Alternatives considered

- **KK** (both keys known in advance; 1 round trip; strong identity hiding,
  5/5 in Noise §7.8). Dowling et al. do not prove it: it is in their Table 1
  but not marked as proven. The initiator must also pick the responder's key
  before connecting, so with `--addr` and two or more paired peers it would
  have to guess, or retry once per allowlisted key.
- **IK** (responder key known; initiator's key sent in the first message).
  Not proven by Dowling et al. It has the same `--addr` problem as KK, and the
  initiator's static key is encrypted only to the responder's static key,
  without forward secrecy (Noise §7.8 property 4).
- **XK** (responder key known; initiator's key sent encrypted in message 3).
  It is proven (Theorem 1, §6.1) and hides the responder better than XX
  (property 3 rather than 1). It still needs the responder's key in advance,
  so it fails the `--addr` constraint. It is a possible later upgrade for
  connections found through mDNS, at the cost of running two patterns.
- **NK, NX, XN, N, X**: proven, but they authenticate only one side or are
  one-way. We need mutual authentication.

## Source

Dowling, B., Rösler, P., & Schwenk, J. (2020). "Flexible Authenticated and
Confidential Channel Establishment (fACCE): Analyzing the Noise Protocol
Framework." PKC 2020, LNCS 12110, pp. 341–373; full version IACR ePrint
2019/436 (brief source #5). Page numbers below refer to the ePrint full
version.

**What the paper proves.** It proves 8 of the 15 base patterns:
**N, NN, NX, NK, X, XN, XX, XK** (§6 "Protocol Analyses", p. 27; proven rows
are marked `*` in Table 1, p. 29; full proofs in Appendix E, with XX in
§E.7). K, KN, KK, KX, IN, IK and IX appear in Table 1 without proofs. The
values in those rows come from the Noise specification and are not
established by the paper. **XX is covered.** Theorem 8 (§E.7) shows XX is
fACCE-secure with these properties:

- **Authentication** levels (3, 2): the responder is authenticated from
  stage 2, the initiator from stage 3, that is, after `-> s, se`.
- **Forward secrecy** from stage 2.
- **KCI resistance** (3, 2).
- **Replay resistance** (2, 0).

The proof assumes collision resistance of the hash, PRF security of the KDF,
AEAD security, and the nn-/sym-ms-PRF-ODH assumptions for the DH group.

**What it does not cover, for our use:**

- **Identity hiding.** It is explicitly left for future work (§7, p. 35). Our
  claims about which keys an eavesdropper or prober can learn rest only on
  the Noise specification's informal table, not on a proof.
- **Key distribution and trust decisions.** The model assumes long-term keys
  are honestly generated and known in advance. Their distribution is outside
  the scope of the analysis (§3, p. 11). Our SPAKE2 pairing and the
  allowlist check that decides whether a proven-authentic key is *trusted*
  are ours, not part of the theorem.
- **Our implementation and instantiation.** The theorem is about the abstract
  pattern. Whether X25519, ChaChaPoly and BLAKE2s meet the assumptions, and
  whether `snow` and our code implement the pattern correctly, is not covered.
  The paper also assumes no padding and that any failure aborts the session
  (§3, p. 11). We follow both.
- **Our application layer.** The paper ignores associated data on
  transport-phase messages (§3); we use none. The prologue corresponds to the
  handshake's associated data `ad` (Fig. 2). Message splitting, the length
  header and the "accepted" message are ordinary channel payloads, so the
  channel guarantees apply to them. Their parsing is our own code and is
  tested, not proven.
- **Pattern modifiers** such as PSKs (§7) and the Noise spec's rekey feature
  are outside the analysis. We use neither.

Perrin, T. *The Noise Protocol Framework*, revision 34 (2018-07-11),
reference only (not counted in brief §9). We use it for the pattern
definitions (§7.5), payload security properties (§7.7), identity hiding
(§7.8), the prologue (§6) and the 65535-byte message limit (§3).

## Consequences

- **What an attacker can learn about keys** (Noise §7.8; informal, see above):
  - The initiator's static key is encrypted with forward secrecy to an
    authenticated responder (property 8). An unpaired responder never
    receives it, because we abort before message 3.
  - The responder's static key is encrypted, but any active initiator,
    including an unpaired one, can learn it by connecting (property 1). mDNS
    already broadcasts the device ID (hash of that key), so this mainly
    confirms what discovery already reveals. The key is public and useless
    without the private key.
  - A passive eavesdropper sees neither static key.
- **Cost.** The handshake plus the confirmation message take 2 round trips,
  versus 1 for KK/IK. That is negligible on a LAN.
- **Unauthenticated work.** The responder does DH operations for unpaired
  initiators. The handshake timeout and bounded frames limit how much a
  hostile peer can hold.
- **Tests.** `transport.rs` tests cover the following, matching the security
  rows of brief §6:
  - rejection of unpaired keys in both directions;
  - a rejected initiator never sending its static key;
  - flipped bits during and after the handshake;
  - a captured-wire check that neither a plaintext marker nor either static
    key appears in cleartext;
  - oversized messages and length headers;
  - the handshake timeout;
  - a real TCP round-trip.
- Brief §4 suggested "IK or KK". It should now point to this ADR's choice
  of XX.
