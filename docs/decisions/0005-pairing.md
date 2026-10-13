# ADR 0005: One-time pairing with SPAKE2 and a PSK-keyed Noise XX handshake

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone / PR:** M2 / #?

## Context

Before two devices sync, each must learn the other's long-term static public
key so later sessions (ADR 0004) can pin it. The user should only have to
type a short code, and an attacker on the same LAN (brief §4 threat model)
who can sniff, tamper, or impersonate must not be able to pair in the user's
place or learn the code. Short codes have little entropy, so the protocol
must not let an attacker test guesses offline.

## Decision

`nebula pair` on device A generates a code and listens. `nebula pair <code>`
on device B connects to A. The code lives for 120 seconds and dies after 3
failed attempts.

1. **Code.** 6 decimal digits drawn from the OS CSPRNG (`getrandom`, with
   rejection sampling so every code is equally likely), shown as `123-456`.
   Parsing ignores dashes and spaces and accepts only ASCII digits.
2. **SPAKE2.** Both sides run SPAKE2 over Ed25519 (the `spake2` crate) with
   the 6 digits as the password, in asymmetric mode: A uses side A with
   identity `nebula-pair-v1/listener`, B uses side B with
   `nebula-pair-v1/joiner`. Asymmetric mode is the standard RFC 9382 form,
   the roles are fixed anyway (one side listens, one connects), and binding
   the roles into the transcript rules out reflecting a side's own message
   back to it. Each side sends one 33-byte message and derives a 32-byte key K.
3. **Key confirmation and identity exchange.** K is used directly as the PSK
   of a Noise handshake `Noise_XXpsk2_25519_ChaChaPoly_BLAKE2s`, prologue
   `nebula-pair/1`, with each device's long-term static key. B is the Noise
   initiator.

   ```
   -> e
   <- e, ee, s, es, psk   payload: A's name
   -> s, se               payload: B's name
   <- (transport msg)     empty: "A accepted"
   ```

   A wrong code gives a different K, so the AEAD on message 2's payload
   fails at B, and B aborts with "wrong code". Every static key and name
   is accepted only after a PSK-keyed AEAD tag over the whole transcript
   has checked out (see below). Names are 1 to 64 bytes of UTF-8 with no
   control characters, since they are printed in a terminal.
4. **Result.** Each side gets `PairedPeer { public_key, device_id, name }`
   and the caller stores it in the allowlist.

**Attempt accounting.** A handles one connection at a time. A connection
that delivers its SPAKE2 message and then fails for any reason (wrong code,
protocol error, disconnect, timeout) counts as a failed attempt; after 3, A
returns "too many attempts" and closes the listener. Connections that drop
before sending a SPAKE2 message (port scans) do not count. The whole listen
has a 120 s deadline, and each connection a 15 s limit, so one silent
connection cannot stall the code until expiry. Every read is a bounded frame:
33 bytes for SPAKE2 messages, 256 bytes for handshake messages.

**Why XXpsk2, not XXpsk3.** The original plan was XXpsk3. With snow we
checked where the PSK is mixed. In XXpsk3 the `psk` token comes at the end
of message 3, so when B (the initiator) reads message 2, A's static key and
name are decrypted under keys derived only from ephemeral and static
Diffie-Hellman. Nothing in them depends on the code, and B never receives
another handshake message. So an attacker who answers B's connection, for
example by spoofing A's mDNS record, would complete the handshake from B's
point of view with its own static key. B would pin the attacker. The test
`xxpsk3_leaves_responder_static_unauthenticated` shows this with snow: a
responder holding a different PSK gets its message 2 accepted under XXpsk3
and rejected under XXpsk2.

In XXpsk2 the `psk` token comes at the end of message 2, after `s` and `es`
and before the payload. snow's `read_message` processes the tokens in order
and then decrypts the payload with keys that include the PSK. The
associated data is the handshake hash, which already covers A's static key.
If the PSK differs, the whole `read_message` call fails and B never uses the
static key. (snow stores `rs` before checking the payload tag, but we read it
only after `read_message` succeeds.) Message 3, carrying B's static key, is
encrypted entirely under PSK-mixed keys, so A gets the same guarantee. The
last message of a 3-message handshake never gets a reply, so A sends one
empty encrypted transport message at the end. Without it, B could report
success even after A had rejected the pairing. This costs one extra frame.

**Guessing bound.** SPAKE2 is a PAKE, so a transcript (passive or active)
does not let an attacker test guesses offline. Each guess costs one live
protocol run against A, which counts as an attempt. With 3 attempts against
10^6 equally likely codes, the attacker succeeds with probability at most
3 / 10^6 = 3 × 10^-6 per code. Impersonating A to B also takes one live
guess per run of `nebula pair <code>`.

## Alternatives considered

- **XXpsk3** (the original plan). Rejected for the reason above.
- **XXpsk3 plus a final confirmation message from A.** This also works,
  because B would wait for A's PSK-keyed transport message before accepting.
  But B would already have sent its static key and name to an
  unauthenticated responder (encrypted under a PSK-mixed key, so the
  attacker cannot read them). XXpsk2 authenticates A one message earlier at
  no extra cost.
- **Symmetric SPAKE2 with a single identity.** This is also secure. We chose
  asymmetric mode because the roles are fixed anyway and it is the standard
  form.
- **Explicit SPAKE2 key-confirmation MACs, then exchanging keys over a
  session cipher.** That adds a hand-rolled MAC exchange and framing. Noise
  already gives key confirmation and an identity exchange in one audited
  construction.
- **Longer or alphanumeric codes.** These are harder to read aloud and type.
  The online bound above is already small.

## Source

Brief §9 source #4: Trautwein, Raulf & Hilt (2023), "Introducing Peer Copy,"
arXiv:2305.02049. Peer Copy authenticates transfers with a PAKE over a short
code. **Deviation:** Peer Copy runs a PAKE for every transfer, so the user
types a code each time. Nebula runs the PAKE once, at pairing, and uses it
only to authenticate an exchange of long-term static keys, which are then
pinned. Every later connection is a Noise handshake with those pinned keys
and needs no code (ADR 0004). SPAKE2 itself is cited through the crate
documentation (<https://docs.rs/spake2>), as brief §9 directs. The Noise
handshake pattern and PSK modifiers follow the Noise specification (Perrin,
reference only).

## Consequences

- Pairing is one round trip of SPAKE2 plus 4 Noise frames. No code is
  needed after pairing.
- **Pairing DoS.** An attacker on the LAN can connect to A and burn the 3
  attempts, or keep connecting so A spends each 15 s window on it. That
  denies pairing (the user generates a new code and retries) but reveals
  nothing about the code and cannot produce a pairing. We accept this.
- A spoofed listener (for example via mDNS) gets one online guess per
  `nebula pair <code>` run on B. The UI should show the peer's name and
  short device ID after pairing so the user can spot a surprise.
- The joiner reports success only after A's confirmation, but A finishes one
  message before B. If that last frame is lost, A has paired and B has not.
  The user re-pairs.
- Tests: same code pairs both ways; wrong code pairs neither side; lockout
  after 3 wrong codes over real TCP (and the listener is closed); 2 wrong
  codes then the right one succeeds; port-scan connections are not counted;
  expiry; code parsing; bad peer names; captured wire bytes contain neither
  the code nor either device name; the XXpsk3 counterexample above.
