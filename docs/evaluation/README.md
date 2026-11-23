# Evaluation results

- Commit: `667694d`
- Date: 2026-11-23
- Machine: Apple M3, 16 GiB RAM, Darwin 26.0
- Rust: rustc 1.99.0 (b940084d7 2026-09-28)

| Brief §6 evaluation | Result file |
|---|---|
| Sync correctness (random operations converge) | [correctness-and-security.md](correctness-and-security.md) |
| Delta efficiency (bytes sent vs. file size) | [delta-and-resume.md](delta-and-resume.md) |
| Conflict handling (no bytes lost) | [correctness-and-security.md](correctness-and-security.md) |
| Large files (1 MiB, 100 MiB, 1 GiB; throughput, memory) | [large-files.md](large-files.md) |
| Dropped connections (resume) | [delta-and-resume.md](delta-and-resume.md), [large-files.md](large-files.md) |
| Security | [correctness-and-security.md](correctness-and-security.md) |

Regenerate with `eval/run-all.sh`.
