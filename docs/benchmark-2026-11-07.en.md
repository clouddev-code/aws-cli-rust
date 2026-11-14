# Performance comparison of aws and awsr (2026-11-07)

## Summary

- **awsr has far lower startup and per-invocation cost.** For work that does not touch the network it is about 10x faster (302 ms → 29 ms), uses less than 1/10 of the CPU time, and about 1/4 of the memory.
- **The gap widens under concurrent execution.** Running `s3 ls` 60 times at concurrency 16, elapsed time went from 5.9 s → 1.7 s and CPU time from 34.2 s → 1.2 s. aws processes compete for CPU during Python startup.
- **S3 transfers are bound by the network, and the difference is small.** awsr is 5–23% faster by median, but given the variance between runs (especially aws's 200 MiB download), the only clear advantage is for bulk transfers of small files.

## Environment

| Item | Details |
|---|---|
| Machine | macOS (Darwin 24.6.0, arm64) |
| aws | aws-cli 2.39.5 (Homebrew, Python 3.15.2). S3 transfers use the default classic client |
| awsr | Release build of branch `p4-s3` (models embedded) |
| Credentials | Same `login_session` profile (cache shared by both) |
| Target | `s3://awsr-tool-test` (us-west-2), measured from Japan |
| Transfer settings | Defaults for both (`max_concurrent_requests` 10, `multipart_threshold` / `multipart_chunksize` 8 MiB) |

## 1. Time per invocation

aws and awsr were run alternately, 20 times each (after one warm-up run each).

| Command | aws median | awsr median | aws p90 | awsr p90 |
|---|---|---|---|---|
| `ec2 describe-instances --generate-cli-skeleton` (no network) | 301.7 ms | 29.3 ms | 305.3 ms | 30.1 ms |
| `sts get-caller-identity` | 728.8 ms | 526.3 ms | 754.2 ms | 559.4 ms |
| `s3 ls s3://awsr-tool-test/` | 768.0 ms | 425.9 ms | 800.6 ms | 446.0 ms |

For commands that use the network, most of the time is the round trip to us-west-2 (TCP + TLS + request). aws's startup alone takes about 300 ms, and that remains as the difference.

## 2. Concurrent execution

`s3 ls s3://awsr-tool-test/` was run 60 times in total at varying concurrency. CPU time is the sum of user + sys of the child processes.

| Concurrency | aws elapsed | awsr elapsed | aws CPU time | awsr CPU time | aws per run | awsr per run |
|---|---|---|---|---|---|---|
| 1 | 44.12 s | 25.29 s | 20.21 s | 1.89 s | 735 ms | 422 ms |
| 4 | 12.75 s | 6.34 s | 25.89 s | 1.45 s | 213 ms | 106 ms |
| 16 | 5.91 s | 1.67 s | 34.22 s | 1.21 s | 99 ms | 28 ms |

Peak memory per process (`/usr/bin/time -l`, `s3 ls` run 3 times) was about 80 MB for aws and about 18 MB for awsr.

## 3. S3 transfers

Each case was run 3 times, alternating the order of aws and awsr.

| Case | aws (3 runs) | awsr (3 runs) | aws median | awsr median | Diff |
|---|---|---|---|---|---|
| Upload 4 KiB × 200 (`--recursive`) | 4.01 / 3.96 / 3.84 s | 3.55 / 3.65 / 3.70 s | 3.96 s | 3.65 s | -8% |
| Download 4 KiB × 200 (`--recursive`) | 4.30 / 4.63 / 4.28 s | 4.09 / 4.12 / 3.88 s | 4.30 s | 4.09 s | -5% |
| Upload 200 MiB | 15.30 / 17.07 / 18.46 s | 12.50 / 18.62 / 14.26 s | 17.07 s | 14.26 s | -16% |
| Download 200 MiB | 50.50 / 29.56 / 56.62 s | 40.52 / 37.24 / 39.03 s | 50.50 s | 39.03 s | -23% |

- For bulk transfers of small files, awsr was slightly faster in all 3 runs. Both use 10 parallel requests, so most of the difference comes from startup and listing time.
- The 200 MiB cases depend on network conditions and showed large variance. The upload ranges overlap. For the download, aws varied widely from 29.6 to 56.6 s, while awsr was stable at 37.2 to 40.5 s. With only 3 runs, however, the difference is indicative only.

## Caveats

- The number of runs is small (3 for transfers), and results vary with time of day and network congestion.
- aws was measured with the default settings, without the CRT transfer client (`preferred_transfer_client = crt`). Using CRT may speed up large-file transfers.
- awsr's download reads each part's body into memory before writing it to the file (up to the part size). aws writes by streaming.
- The scripts for sections 1 and 2 were run in this session's working directory. Section 3 was run with `transfer.py` in the user's shell.
