# Nebula evaluation: large files over real encrypted sessions

Two `nebula run` daemons on one machine (Darwin arm64), loopback TCP, Noise-encrypted.
Time runs from starting the receiver until its copy is byte-identical (`cmp`), so it
includes connecting, the handshake, the index exchange and the final hash check.

| File size | Time | Throughput | Peak memory, sender | Peak memory, receiver | Copy identical |
|---|---|---|---|---|---|
| 1 MiB | 0.14 s | 7.1 MiB/s | 11.2 MiB | 8.9 MiB | yes |
| 100 MiB | 0.70 s | 142.8 MiB/s | 17.0 MiB | 13.7 MiB | yes |
| 1 GiB | 6.62 s | 154.6 MiB/s | 92.1 MiB | 28.6 MiB | yes |

## Resume after the receiver is killed (kill -9) mid-transfer

- File: 1024 MiB. Partial download on disk when killed: 539.4 MiB (53 %).
- After restarting the receiver, the copy was complete and byte-identical in 2.87 s.
- Receiver log: "resuming; 68982 of 130981 chunks" (only the remaining chunks were requested).
