# Nebula evaluation: delta transfer and resume

Two sync engines in one process, real files on disk, every message encoded and decoded as on the wire. Chunker: content-defined, 2 / 8 / 64 KiB (min / avg / max).

## Delta efficiency (100 MiB random file)

| Change | File data sent | All protocol bytes | Sent / file size | Fixed 8 KiB blocks would send |
|---|---|---|---|---|
| First sync (no copy on the receiver) | 100.0 MiB | 101.0 MiB | 100.00 % | 100.0 MiB |
| Overwrite 1 byte in the middle | 54.7 KiB | 481.2 KiB | 0.05 % | 8.0 KiB |
| Insert 1 byte at the start | 7.1 KiB | 433.5 KiB | 0.0069 % | 100.0 MiB |
| Insert 1 KiB in the middle | 55.7 KiB | 482.2 KiB | 0.05 % | 50.0 MiB |
| Delete 1,000 bytes from the middle | 53.8 KiB | 480.2 KiB | 0.05 % | 50.0 MiB |
| Append 1 MiB at the end | 1.0 MiB | 1.4 MiB | 0.99 % | 1.0 MiB |

"All protocol bytes" includes the index entry (36 bytes per chunk), chunk requests and message headers, before Noise encryption (+20 bytes per frame of up to 64 KiB).

## Resume after a dropped connection (100 MiB file)

| Cut after | Data sent before the drop | Data sent after reconnecting | Total | Overhead vs. file size | Copy verified |
|---|---|---|---|---|---|
| 25 % | 25.0 MiB | 75.0 MiB | 100.0 MiB | 0.0000 % | yes (BLAKE3 match) |
| 50 % | 50.0 MiB | 50.0 MiB | 100.0 MiB | 0.0000 % | yes (BLAKE3 match) |
| 90 % | 90.0 MiB | 10.0 MiB | 100.0 MiB | 0.0000 % | yes (BLAKE3 match) |

Overhead is chunk data requested before the drop whose replies were lost in flight (at most 32 requests).

_Generated in 13.2 s._
