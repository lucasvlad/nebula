# Nebula evaluation: large files over real encrypted sessions

Two `nebula run` daemons on one machine (Darwin arm64), loopback TCP, Noise-encrypted.
Time runs from starting the receiver until its copy is byte-identical (`cmp`), so it
includes connecting, the handshake, the index exchange and the final hash check.

| File size | Time | Throughput | Peak memory, sender | Peak memory, receiver | Copy identical |
|---|---|---|---|---|---|
| 1 MiB | 0.15 s | 6.7 MiB/s | 11.4 MiB | 10.5 MiB | yes |
| 100 MiB | 1.05 s | 95.4 MiB/s | 17.0 MiB | 14.0 MiB | yes |
| 1 GiB | 9.62 s | 106.4 MiB/s | 96.8 MiB | 48.7 MiB | yes |

## Resume after the receiver is killed (kill -9) mid-transfer

- File: 1024 MiB. Partial download on disk when killed: 522.9 MiB (51 %).
- After restarting the receiver, the copy was complete and byte-identical in 5.28 s.
- Receiver log: "resuming; 66607 of 130444 chunks" (only the remaining chunks were requested).
