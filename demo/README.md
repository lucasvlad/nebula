# Nebula security demos

Two ways to show what an eavesdropper or a "man in the middle" can and cannot do.
All attacker code lives in the test package (`tests/src/bin/mitm_demo.rs`,
`tests/src/demo.rs`). It is never part of the `nebula` binary.

## 1. Self-contained story (no setup)

```sh
cargo run -p nebula-tests --bin mitm-demo -- simulate            # pauses between acts
cargo run -p nebula-tests --bin mitm-demo -- simulate --no-pause
NO_COLOR=1 cargo run -p nebula-tests --bin mitm-demo -- simulate # plain text
```

Four acts, all in one process over 127.0.0.1:

1. Plain connection: the eavesdropper reads the whole sentence.
2. Nebula with an eavesdropper: only scrambled bytes; both security codes match.
3. A middleman against devices that (wrongly) accept any key: Mallory reads and edits the
   message, and the two security codes differ. This setup is deliberately broken, to show
   why Nebula pins keys.
4. The same middleman against real Nebula: both devices refuse her.

## 2. Live, with real `nebula` processes

```sh
demo/mitm-demo.sh            # pauses between steps
demo/mitm-demo.sh --no-pause
```

It builds release binaries, makes two throwaway devices ("Alice's laptop", "Bob's desktop"),
pairs them, and then uses ports 17900-17979:

- Step 1: `mitm-demo snoop` relays Bob's `nebula ping --message ...` to Alice's `nebula run`.
  The snoop prints scrambled bytes and "'locker' not found"; Alice's log shows the decrypted
  message and the security code.
- Step 2: the same with `--tamper-at 150`. One flipped bit makes Alice's device report a
  decrypt error and Bob's ping fail.
- Step 3: `mitm-demo impersonate` answers Bob as if it were Alice and connects to Alice as
  if it were Bob. Both real devices refuse the unknown key.

Manual commands (one terminal each):

```sh
nebula run --port 17978
mitm-demo snoop --listen 127.0.0.1:17900 --to 127.0.0.1:17978 --look-for locker [--tamper-at 150]
nebula ping --addr 127.0.0.1:17900 -c 1 --message "My locker code is 4821."
mitm-demo impersonate --listen 127.0.0.1:17901 --to 127.0.0.1:17978
```

Note: encrypted bytes look random, and random bytes contain short letter runs by chance
(like `G6ge3`). The readable-text check therefore only reports runs of 8 or more
characters, and any it finds are listed rather than hidden.
