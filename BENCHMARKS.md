# Benchmark Report

This report compares the local `zstandard` checkout against the official `zstd` implementation at the revision pinned in `upstream-zstd.ref`. Numbers measured against any other revision are not comparable: upstream changes its level mapping, parser heuristics, and block splitter between releases.

Notes:
- Rust encode is benchmarked for the currently supported public compression levels `1..=22`.
- Upstream encode is benchmarked for levels `1..=22`.
- Rust decode and upstream decode are benchmarked on the same upstream-produced frame for each row.
- Dictionary cases use the deterministic raw and trained dictionary fixtures emitted by the upstream helper.
- Throughput is machine-local and depends on the current host; use this file for relative comparison, not cross-machine claims.
- Each throughput number is the fastest of 3 timing trials, since background load can only make a trial slower. Ratios are exact byte counts and reproduce exactly; throughput does not. The two sides of each comparison are timed seconds apart inside a ten-minute sweep, so a case's throughput carries drift from wherever it sits in the run: across three sweeps of identical code one case's slow-level count read 4, 4 and 8, and another's decode count 0, 2 and 16. Read the throughput columns as indicative, not as a gate, and do not act on a change of one or two.
- Each trial runs as many iterations as fit its time budget, sized per row from a single probe iteration, so a fast row is measured over a longer loop rather than a slower one being measured more times. Reports generated before this replaced a fixed byte target read low levels over windows of a few milliseconds; their fast-level throughput is measured over too short an interval to compare against numbers here.

| Setting | Value |
| --- | --- |
| Output file | `BENCHMARKS.md` |
| Corpus cases | 11 |
| Input bytes per case | 4194304 |
| Benchmarked levels | `1-22` |
| Case filters | all |
| Block size | 131072 |
| Streaming piece size | 32 KiB |
| Streaming sensitivity pieces | 16 KiB, 128 KiB, 1 MiB |
| zstandard revision | `v0.1.6-1-gfcefd1c` |
| Upstream zstd reference | `v1.5.7` |
| Timing trial budget | 60 ms |
| Iterations per trial | 1-128, sized per row |
| Stage profiling target bytes | 33554432 |
| Timing trials per row (fastest reported) | 3 |

## Coverage Summary

| Metric | Value |
| --- | --- |
| Corpus cases | 11 |
| Total case/level rows | 242 |
| Rust encode rows supported | 242/242 |
| Rust encode rows completed | 242 |
| Rust decode rows completed | 242 |
| Rust encoder level range | -131072..=22 |
| Benchmarked levels | 1-22 |


## Target Gaps

| Metric | Value |
| --- | --- |
| Encode rows below 50% | 0 |
| Decode rows below 50% | 0 |
| Ratio regressions | 25 |
| Ratio regressions above 1% | 0 |
| Cases behind upstream on a third of encode levels | 3 |
| Cases behind upstream on a third of decode levels | 0 |
| Streaming rows above upstream | 18 |
| Streaming rows above upstream by 1% | 0 |
| Streaming rows below upstream by 1% | 65 |

### Throughput by Case

This crate's throughput as a fraction of upstream's, summarized over the benchmarked levels. Above 1.00x is this crate being faster.

The `slow` column counts the case's levels below 90% of upstream, and it is the one to read. A single row's throughput moves a tenth between sweeps of identical code, so no one row means anything here; the median is worth less than it looks too, because a case split into a slow band and a fast one has its median on the boundary between them and crosses it on noise. The worst column says which level to open first once `slow` has flagged the case.

| Case | Encode slow | Encode median | Encode worst | Level | Decode slow | Decode median | Decode worst | Level |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| small-alphabet | 0/22 | 1.09x | 0.97x | 13 | 0/22 | 4.45x | 2.48x | 16 |
| repeated-chunk | 0/22 | 1.16x | 1.02x | 12 | 0/22 | 3.17x | 1.54x | 16 |
| json-records | 0/22 | 1.12x | 0.96x | 5 | 1/22 | 0.99x | 0.90x | 21 |
| log-lines | 0/22 | 0.99x | 0.91x | 5 | 4/22 | 0.93x | 0.88x | 21 |
| mixed-entropy | 8/22 | 0.96x | 0.71x | 21 | 6/22 | 0.93x | 0.87x | 7 |
| wikipedia | 0/22 | 1.07x | 0.97x | 1 | 7/22 | 0.95x | 0.88x | 18 |
| tabular-csv | 2/22 | 0.97x | 0.89x | 8 | 6/22 | 0.99x | 0.88x | 18 |
| binary-structured | 0/22 | 1.05x | 0.91x | 8 | 0/22 | 1.01x | 0.94x | 1 |
| pseudorandom | 10/22 | 0.90x | 0.58x | 15 | 0/22 | 1.35x | 1.14x | 4 |
| raw-dictionary | 7/22 | 1.13x | 0.67x | 9 | 4/22 | 0.91x | 0.88x | 10 |
| trained-dictionary | 9/22 | 0.95x | 0.67x | 10 | 1/22 | 0.94x | 0.87x | 14 |

### Ratio Regressions

Rows where this crate emitted more bytes than upstream, largest relative excess first. The comparison is on exact byte counts: most of these rows differ by a handful of bytes on a multi-megabyte case and are listed for completeness, not as defects.

- raw-dictionary L18 +0.08% (53411 vs 53368 bytes, +43)
- raw-dictionary L19 +0.08% (53411 vs 53368 bytes, +43)
- raw-dictionary L20 +0.08% (53411 vs 53368 bytes, +43)
- raw-dictionary L21 +0.08% (53411 vs 53368 bytes, +43)
- raw-dictionary L22 +0.08% (53411 vs 53368 bytes, +43)
- raw-dictionary L9 +0.05% (58376 vs 58345 bytes, +31)
- raw-dictionary L10 +0.05% (58376 vs 58345 bytes, +31)
- raw-dictionary L14 +0.02% (52212 vs 52200 bytes, +12)
- wikipedia L21 +0.01% (9719 vs 9718 bytes, +1)
- tabular-csv L22 +0.01% (459064 vs 459021 bytes, +43)
- binary-structured L16 +0.01% (522221 vs 522182 bytes, +39)
- binary-structured L17 +0.01% (522226 vs 522187 bytes, +39)
- tabular-csv L20 +0.01% (460818 vs 460784 bytes, +34)
- tabular-csv L21 +0.01% (460818 vs 460784 bytes, +34)
- tabular-csv L19 +0.01% (460820 vs 460787 bytes, +33)
- mixed-entropy L18 +0.01% (1395682 vs 1395604 bytes, +78)
- mixed-entropy L19 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L20 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L21 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L22 +0.01% (1395682 vs 1395605 bytes, +77)
- binary-structured L19 +0.00% (520950 vs 520945 bytes, +5)
- binary-structured L20 +0.00% (520950 vs 520945 bytes, +5)
- binary-structured L21 +0.00% (520950 vs 520945 bytes, +5)
- binary-structured L22 +0.00% (520950 vs 520945 bytes, +5)
- binary-structured L18 +0.00% (520963 vs 520958 bytes, +5)

### Streaming Size Deltas Above 1%

Signed against upstream's streaming encoder at the same piece size, largest excess first. Negative rows are this crate emitting fewer bytes; they are listed because a streaming size difference in either direction is a block-layout difference, and the direction alone does not say which implementation made the better choice.

- log-lines L8 piece 32 KiB -1.04% (624891 vs 631474 bytes)
- tabular-csv L4 piece 32 KiB -1.21% (635004 vs 642754 bytes)
- wikipedia L3 piece 16 KiB -1.41% (53978 vs 54752 bytes)
- wikipedia L3 piece 32 KiB -1.41% (53978 vs 54752 bytes)
- wikipedia L3 piece 128 KiB -1.41% (53978 vs 54752 bytes)
- wikipedia L3 piece 1 MiB -1.41% (53978 vs 54752 bytes)
- wikipedia L4 piece 32 KiB -1.41% (53978 vs 54752 bytes)
- tabular-csv L5 piece 32 KiB -1.50% (619153 vs 628604 bytes)
- tabular-csv L9 piece 16 KiB -1.94% (533529 vs 544092 bytes)
- tabular-csv L9 piece 32 KiB -1.94% (533529 vs 544092 bytes)
- tabular-csv L9 piece 128 KiB -1.94% (533529 vs 544092 bytes)
- tabular-csv L9 piece 1 MiB -1.94% (533529 vs 544092 bytes)
- tabular-csv L8 piece 32 KiB -1.97% (532403 vs 543126 bytes)
- json-records L8 piece 32 KiB -2.10% (292013 vs 298284 bytes)
- tabular-csv L7 piece 32 KiB -2.12% (583489 vs 596146 bytes)
- json-records L9 piece 16 KiB -2.31% (296381 vs 303380 bytes)
- json-records L9 piece 32 KiB -2.31% (296381 vs 303380 bytes)
- json-records L9 piece 128 KiB -2.31% (296381 vs 303380 bytes)
- json-records L9 piece 1 MiB -2.31% (296381 vs 303380 bytes)
- tabular-csv L6 piece 32 KiB -2.50% (565980 vs 580506 bytes)
- wikipedia L8 piece 32 KiB -2.53% (11319 vs 11613 bytes)
- wikipedia L9 piece 16 KiB -2.53% (11352 vs 11647 bytes)
- wikipedia L9 piece 32 KiB -2.53% (11352 vs 11647 bytes)
- wikipedia L9 piece 128 KiB -2.53% (11352 vs 11647 bytes)
- wikipedia L9 piece 1 MiB -2.53% (11352 vs 11647 bytes)
- wikipedia L7 piece 32 KiB -2.56% (11327 vs 11625 bytes)
- tabular-csv L10 piece 32 KiB -2.60% (535301 vs 549584 bytes)
- wikipedia L10 piece 32 KiB -2.83% (11047 vs 11369 bytes)
- wikipedia L11 piece 32 KiB -2.86% (11017 vs 11341 bytes)
- wikipedia L12 piece 32 KiB -2.86% (11017 vs 11341 bytes)
- wikipedia L14 piece 32 KiB -3.25% (10746 vs 11107 bytes)
- wikipedia L13 piece 32 KiB -3.26% (10807 vs 11171 bytes)
- wikipedia L15 piece 16 KiB -3.27% (10663 vs 11024 bytes)
- wikipedia L15 piece 32 KiB -3.27% (10663 vs 11024 bytes)
- wikipedia L15 piece 128 KiB -3.27% (10663 vs 11024 bytes)
- wikipedia L15 piece 1 MiB -3.27% (10663 vs 11024 bytes)
- tabular-csv L11 piece 32 KiB -3.62% (565881 vs 587115 bytes)
- log-lines L10 piece 32 KiB -3.78% (541522 vs 562802 bytes)
- tabular-csv L12 piece 32 KiB -3.95% (562277 vs 585381 bytes)
- log-lines L13 piece 32 KiB -4.79% (518945 vs 545060 bytes)
- log-lines L14 piece 32 KiB -4.85% (516870 vs 543191 bytes)
- log-lines L11 piece 32 KiB -4.89% (517889 vs 544533 bytes)
- log-lines L12 piece 32 KiB -4.92% (517298 vs 544045 bytes)
- tabular-csv L13 piece 32 KiB -4.94% (607440 vs 639024 bytes)
- json-records L13 piece 32 KiB -5.09% (233318 vs 245824 bytes)
- tabular-csv L14 piece 32 KiB -5.40% (610769 vs 645651 bytes)
- log-lines L15 piece 16 KiB -5.72% (502035 vs 532496 bytes)
- log-lines L15 piece 32 KiB -5.72% (502035 vs 532496 bytes)
- log-lines L15 piece 128 KiB -5.72% (502035 vs 532496 bytes)
- log-lines L15 piece 1 MiB -5.72% (502035 vs 532496 bytes)
- json-records L14 piece 32 KiB -5.73% (224779 vs 238447 bytes)
- json-records L15 piece 16 KiB -5.92% (221786 vs 235753 bytes)
- json-records L15 piece 32 KiB -5.92% (221786 vs 235753 bytes)
- json-records L15 piece 128 KiB -5.92% (221786 vs 235753 bytes)
- json-records L15 piece 1 MiB -5.92% (221786 vs 235753 bytes)
- json-records L11 piece 32 KiB -6.30% (235770 vs 251618 bytes)
- json-records L12 piece 32 KiB -6.32% (235787 vs 251682 bytes)
- json-records L10 piece 32 KiB -6.34% (236264 vs 252246 bytes)
- tabular-csv L15 piece 16 KiB -7.28% (573388 vs 618401 bytes)
- tabular-csv L15 piece 32 KiB -7.28% (573388 vs 618401 bytes)
- tabular-csv L15 piece 128 KiB -7.28% (573388 vs 618401 bytes)
- tabular-csv L15 piece 1 MiB -7.28% (573388 vs 618401 bytes)
- json-records L5 piece 32 KiB -7.81% (380127 vs 412350 bytes)
- json-records L6 piece 32 KiB -14.63% (297251 vs 348192 bytes)
- json-records L7 piece 32 KiB -14.97% (295302 vs 347284 bytes)



## small-alphabet

Four-symbol high-redundancy synthetic text.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0001 | 0.0001 | 16310.71 | 14205.09 | 38936.41 | 5473.20 |
| 2 | ok | ok | 0.0001 | 0.0001 | 15684.31 | 14069.80 | 38181.77 | 5498.35 |
| 3 | ok | ok | 0.0001 | 0.0001 | 8814.79 | 8099.18 | 36316.78 | 5492.48 |
| 4 | ok | ok | 0.0001 | 0.0001 | 8128.16 | 8048.29 | 36047.66 | 5456.23 |
| 5 | ok | ok | 0.0001 | 0.0001 | 3955.13 | 3843.06 | 44211.78 | 5447.51 |
| 6 | ok | ok | 0.0001 | 0.0001 | 2766.42 | 2525.09 | 37871.22 | 8675.60 |
| 7 | ok | ok | 0.0001 | 0.0001 | 2728.51 | 2496.93 | 56869.14 | 8647.31 |
| 8 | ok | ok | 0.0001 | 0.0001 | 1495.96 | 1421.91 | 39262.42 | 8720.70 |
| 9 | ok | ok | 0.0001 | 0.0001 | 1470.06 | 1409.15 | 38604.00 | 8616.93 |
| 10 | ok | ok | 0.0001 | 0.0001 | 1423.41 | 1376.29 | 47361.73 | 8704.98 |
| 11 | ok | ok | 0.0001 | 0.0001 | 1419.92 | 1388.54 | 38289.80 | 8725.81 |
| 12 | ok | ok | 0.0001 | 0.0001 | 1329.12 | 1294.62 | 38242.25 | 8655.24 |
| 13 | ok | ok | 0.0001 | 0.0001 | 1433.05 | 1469.92 | 37275.56 | 8776.12 |
| 14 | ok | ok | 0.0001 | 0.0001 | 1357.73 | 1367.16 | 41201.03 | 8701.72 |
| 15 | ok | ok | 0.0001 | 0.0001 | 1295.80 | 1276.53 | 40634.79 | 8562.39 |
| 16 | ok | ok | 0.0001 | 0.0001 | 4022.52 | 467.61 | 39751.81 | 16027.93 |
| 17 | ok | ok | 0.0001 | 0.0001 | 3501.52 | 447.95 | 41071.99 | 15579.36 |
| 18 | ok | ok | 0.0001 | 0.0001 | 3510.62 | 442.57 | 40170.25 | 16017.87 |
| 19 | ok | ok | 0.0001 | 0.0001 | 2451.28 | 462.20 | 38985.27 | 15593.92 |
| 20 | ok | ok | 0.0001 | 0.0001 | 2047.27 | 436.59 | 44128.26 | 15082.60 |
| 21 | ok | ok | 0.0001 | 0.0001 | 2082.61 | 432.11 | 40883.28 | 15197.46 |
| 22 | ok | ok | 0.0001 | 0.0001 | 2094.37 | 462.17 | 39438.34 | 15598.19 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 406 | 406 | +0.00% | -0.25% |
| 2 | 406 | 406 | +0.00% | -0.25% |
| 3 | 406 | 406 | +0.00% | -0.25% |
| 4 | 406 | 406 | +0.00% | -0.25% |
| 5 | 406 | 407 | -0.25% | -0.25% |
| 6 | 371 | 373 | -0.54% | -0.27% |
| 7 | 371 | 373 | -0.54% | -0.27% |
| 8 | 371 | 373 | -0.54% | -0.27% |
| 9 | 371 | 371 | +0.00% | +0.00% |
| 10 | 371 | 371 | +0.00% | +0.00% |
| 11 | 371 | 371 | +0.00% | +0.00% |
| 12 | 371 | 371 | +0.00% | +0.00% |
| 13 | 371 | 372 | -0.27% | +0.00% |
| 14 | 371 | 372 | -0.27% | +0.00% |
| 15 | 371 | 372 | -0.27% | +0.00% |
| 16 | 379 | 379 | +0.00% | +0.00% |
| 17 | 379 | 379 | +0.00% | +0.00% |
| 18 | 379 | 379 | +0.00% | +0.00% |
| 19 | 379 | 379 | +0.00% | +0.00% |
| 20 | 379 | 379 | +0.00% | +0.00% |
| 21 | 379 | 379 | +0.00% | +0.00% |
| 22 | 379 | 379 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | -0.27% | 15 |
| 128 KiB | +0.00% | 19 | -0.27% | 15 |
| 1 MiB | +0.00% | 19 | -0.27% | 15 |


## repeated-chunk

Single repeated chunk that stresses match finding and repcodes.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0001 | 0.0001 | 20688.96 | 17472.61 | 43541.12 | 13677.20 |
| 2 | ok | ok | 0.0001 | 0.0001 | 20302.96 | 17477.98 | 46300.80 | 13622.76 |
| 3 | ok | ok | 0.0001 | 0.0001 | 11940.57 | 10112.38 | 45706.97 | 13655.06 |
| 4 | ok | ok | 0.0001 | 0.0001 | 10967.90 | 10176.20 | 46895.40 | 13675.44 |
| 5 | ok | ok | 0.0001 | 0.0001 | 7103.02 | 6631.95 | 50124.21 | 13730.93 |
| 6 | ok | ok | 0.0001 | 0.0001 | 3998.26 | 3478.19 | 43396.42 | 13734.81 |
| 7 | ok | ok | 0.0001 | 0.0001 | 3903.67 | 3474.00 | 48293.91 | 13717.33 |
| 8 | ok | ok | 0.0001 | 0.0001 | 2294.09 | 2062.07 | 43509.97 | 13692.88 |
| 9 | ok | ok | 0.0001 | 0.0001 | 2237.90 | 2074.74 | 44134.28 | 13624.60 |
| 10 | ok | ok | 0.0001 | 0.0001 | 2116.55 | 1992.32 | 42315.36 | 13652.32 |
| 11 | ok | ok | 0.0001 | 0.0001 | 2117.96 | 1997.45 | 50466.35 | 13659.50 |
| 12 | ok | ok | 0.0001 | 0.0001 | 1927.75 | 1883.09 | 42406.19 | 13710.79 |
| 13 | ok | ok | 0.0001 | 0.0001 | 290.74 | 263.09 | 42721.26 | 13755.48 |
| 14 | ok | ok | 0.0001 | 0.0001 | 209.56 | 186.66 | 43095.22 | 13652.29 |
| 15 | ok | ok | 0.0001 | 0.0001 | 118.71 | 99.47 | 43649.54 | 13662.93 |
| 16 | ok | ok | 0.0001 | 0.0001 | 5229.90 | 1445.26 | 39645.49 | 25686.05 |
| 17 | ok | ok | 0.0001 | 0.0001 | 4411.68 | 1350.80 | 40439.81 | 24911.77 |
| 18 | ok | ok | 0.0001 | 0.0001 | 4268.25 | 1369.91 | 45325.11 | 14556.21 |
| 19 | ok | ok | 0.0001 | 0.0001 | 2871.39 | 1355.93 | 44012.88 | 14823.19 |
| 20 | ok | ok | 0.0001 | 0.0001 | 2365.08 | 1245.77 | 42725.41 | 14936.09 |
| 21 | ok | ok | 0.0001 | 0.0001 | 2367.39 | 1275.82 | 43915.83 | 15112.74 |
| 22 | ok | ok | 0.0001 | 0.0001 | 2365.91 | 1216.33 | 51975.30 | 14856.45 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 441 | 441 | +0.00% | -0.23% |
| 2 | 441 | 441 | +0.00% | -0.23% |
| 3 | 441 | 441 | +0.00% | -0.23% |
| 4 | 441 | 441 | +0.00% | -0.23% |
| 5 | 441 | 442 | -0.23% | -0.23% |
| 6 | 441 | 442 | -0.23% | -0.23% |
| 7 | 441 | 442 | -0.23% | -0.23% |
| 8 | 441 | 442 | -0.23% | -0.23% |
| 9 | 441 | 441 | +0.00% | +0.00% |
| 10 | 441 | 441 | +0.00% | +0.00% |
| 11 | 441 | 441 | +0.00% | +0.00% |
| 12 | 441 | 441 | +0.00% | +0.00% |
| 13 | 441 | 441 | +0.00% | +0.00% |
| 14 | 441 | 441 | +0.00% | +0.00% |
| 15 | 441 | 441 | +0.00% | +0.00% |
| 16 | 412 | 412 | +0.00% | +0.00% |
| 17 | 412 | 412 | +0.00% | +0.00% |
| 18 | 410 | 410 | +0.00% | +0.00% |
| 19 | 410 | 410 | +0.00% | +0.00% |
| 20 | 410 | 410 | +0.00% | +0.00% |
| 21 | 410 | 410 | +0.00% | +0.00% |
| 22 | 410 | 410 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | +0.00% | 1 |
| 128 KiB | +0.00% | 19 | +0.00% | 1 |
| 1 MiB | +0.00% | 19 | +0.00% | 1 |


## json-records

Structured JSON-like service records with repeated keys and modest value churn.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0578 | 0.0578 | 1935.92 | 1717.95 | 5312.39 | 5353.47 |
| 2 | ok | ok | 0.0900 | 0.0900 | 1325.60 | 1109.07 | 3859.41 | 3857.86 |
| 3 | ok | ok | 0.1082 | 0.1082 | 927.73 | 934.01 | 3891.61 | 3797.47 |
| 4 | ok | ok | 0.1073 | 0.1073 | 892.92 | 922.82 | 3845.39 | 3848.13 |
| 5 | ok | ok | 0.0906 | 0.0983 | 334.67 | 347.08 | 3847.98 | 3881.27 |
| 6 | ok | ok | 0.0709 | 0.0830 | 284.98 | 283.99 | 4563.09 | 4556.02 |
| 7 | ok | ok | 0.0704 | 0.0828 | 243.66 | 247.80 | 4550.76 | 4616.38 |
| 8 | ok | ok | 0.0696 | 0.0711 | 202.69 | 199.87 | 4432.25 | 4559.80 |
| 9 | ok | ok | 0.0707 | 0.0723 | 192.98 | 184.96 | 4348.73 | 4471.25 |
| 10 | ok | ok | 0.0563 | 0.0601 | 167.69 | 146.44 | 5359.50 | 5390.25 |
| 11 | ok | ok | 0.0562 | 0.0600 | 125.93 | 109.84 | 5404.16 | 5364.12 |
| 12 | ok | ok | 0.0562 | 0.0600 | 118.79 | 98.45 | 5315.52 | 5425.27 |
| 13 | ok | ok | 0.0556 | 0.0586 | 102.94 | 88.81 | 5431.48 | 5405.97 |
| 14 | ok | ok | 0.0536 | 0.0569 | 95.18 | 75.31 | 5613.34 | 5505.14 |
| 15 | ok | ok | 0.0529 | 0.0562 | 76.31 | 71.06 | 5661.27 | 5688.51 |
| 16 | ok | ok | 0.0532 | 0.0532 | 7.01 | 6.27 | 4663.81 | 4961.18 |
| 17 | ok | ok | 0.0532 | 0.0532 | 6.15 | 5.97 | 4676.10 | 4809.94 |
| 18 | ok | ok | 0.0565 | 0.0565 | 5.24 | 4.64 | 4810.61 | 5211.32 |
| 19 | ok | ok | 0.0480 | 0.0480 | 3.00 | 2.76 | 5772.37 | 6326.84 |
| 20 | ok | ok | 0.0480 | 0.0480 | 3.07 | 2.62 | 5606.25 | 6219.52 |
| 21 | ok | ok | 0.0480 | 0.0480 | 2.97 | 2.65 | 5593.06 | 6246.30 |
| 22 | ok | ok | 0.0480 | 0.0480 | 2.97 | 2.60 | 5666.92 | 6266.88 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 242385 | 242385 | +0.00% | -0.00% |
| 2 | 377551 | 377551 | +0.00% | -0.00% |
| 3 | 453910 | 456240 | -0.51% | -0.00% |
| 4 | 450201 | 452855 | -0.59% | -0.00% |
| 5 | 380127 | 412350 | -7.81% | -0.00% |
| 6 | 297251 | 348192 | -14.63% | -0.00% |
| 7 | 295302 | 347284 | -14.97% | -0.00% |
| 8 | 292013 | 298284 | -2.10% | -0.00% |
| 9 | 296381 | 303380 | -2.31% | +0.00% |
| 10 | 236264 | 252246 | -6.34% | +0.00% |
| 11 | 235770 | 251618 | -6.30% | +0.00% |
| 12 | 235787 | 251682 | -6.32% | +0.00% |
| 13 | 233318 | 245824 | -5.09% | +0.00% |
| 14 | 224779 | 238447 | -5.73% | +0.00% |
| 15 | 221786 | 235753 | -5.92% | +0.00% |
| 16 | 223302 | 223302 | +0.00% | +0.00% |
| 17 | 222962 | 222962 | +0.00% | +0.00% |
| 18 | 237130 | 237130 | +0.00% | +0.00% |
| 19 | 201232 | 201232 | +0.00% | +0.00% |
| 20 | 201232 | 201232 | +0.00% | +0.00% |
| 21 | 201232 | 201232 | +0.00% | +0.00% |
| 22 | 201232 | 201232 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | -5.92% | 15 |
| 128 KiB | +0.00% | 19 | -5.92% | 15 |
| 1 MiB | +0.00% | 19 | -5.92% | 15 |

### Rust First-Block Stage Timing

- Samples the first raw `block_size` chunk only, so the timing breakdown stays aligned with the real block-local hot path.
- Uses prepared dictionaries for dictionary-backed cases so the sample reflects encoder hot paths instead of repeated dictionary parsing.
- The stage table above is sampled with the planner's phase timers off, so its milliseconds and its shares are both the real encoder's. The two sub-breakdown tables below need those timers and are sampled separately, because a timer taken per lazy parser step costs far more than the step: with them on, this case's first block reads up to 18x its real time and 99% of the frame lands in `Plan`. Read the sub-breakdowns as shares of their own row and never against the table above.
- The planning sub-breakdown covers row and chain/extdict lazy paths; other planner families may still report zeros. The lazy parser phase sub-breakdown is instrumented for no-dict row and trained-dictionary chain/extdict cases, and likewise reports zeros elsewhere.
- Sampled on levels 3-7 over 3 iterations.

| Level | Sampled ms | Blocks | Compressed | Split % | Plan % | Lit % | Seq % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.88 | 1 | 1 | 0.1 | 32.8 | 50.3 | 11.7 | 5.1 |
| 4 | 0.49 | 1 | 1 | 0.0 | 64.5 | 5.9 | 18.0 | 11.5 |
| 5 | 1.24 | 1 | 1 | 0.0 | 87.9 | 2.3 | 6.7 | 3.1 |
| 6 | 1.41 | 1 | 1 | 0.0 | 91.5 | 2.1 | 5.4 | 1.0 |
| 7 | 1.60 | 1 | 1 | 0.0 | 92.7 | 1.8 | 4.6 | 0.9 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.13 | 1 | 1 | 2.7 | 2.2 | 6.8 | 87.9 | 0.4 |
| 4 | 1.15 | 1 | 1 | 2.8 | 2.0 | 7.1 | 87.7 | 0.4 |
| 5 | 0.95 | 1 | 1 | 3.0 | 2.3 | 6.9 | 87.4 | 0.5 |
| 6 | 0.93 | 1 | 1 | 2.8 | 2.2 | 6.9 | 87.4 | 0.7 |
| 7 | 0.92 | 1 | 1 | 3.1 | 2.3 | 6.9 | 87.3 | 0.4 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.0 | 24.3 | 0.0 | 53.6 |
| 4 | 22.2 | 22.8 | 0.0 | 55.0 |
| 5 | 22.1 | 22.5 | 0.0 | 55.5 |
| 6 | 22.6 | 22.3 | 0.0 | 55.1 |
| 7 | 23.0 | 23.3 | 0.0 | 53.8 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 6.4 | 0.0 | 1.7 | 6.0 | 12.6 | 65.9 |
| 6 | 7.2 | 0.0 | 3.1 | 6.1 | 13.1 | 64.5 |
| 7 | 6.9 | 0.0 | 4.0 | 6.5 | 13.5 | 64.1 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 15.1 | 38.1 | 0.0 | 2.7 | 5.5 | 38.6 |
| 6 | 12.6 | 30.9 | 17.3 | 2.1 | 4.5 | 32.6 |
| 7 | 10.9 | 26.0 | 28.8 | 1.8 | 3.8 | 28.7 |

### Rust First-Block Parser Stats

- One-shot trace of the first block at each level. Sequence counts, byte breakdowns, and repcode usage come from the parser trace, not from timing.
- Sampled on levels 3-7.

| Level | Sequences | C Seqs | Lit bytes | Match bytes | Rep1 | Rep2 | Rep3 | Rep1-1 | Explicit | Avg ML | Avg offset |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 3688 | 3688 | 9038 | 122034 | 1295 | 23 | 0 | 0 | 2370 | 33.1 | 24310 |
| 4 | 3688 | 3688 | 9040 | 122032 | 1294 | 21 | 0 | 0 | 2373 | 33.1 | 24390 |
| 5 | 3104 | 3104 | 8669 | 122403 | 1278 | 757 | 143 | 1 | 925 | 39.4 | 30928 |
| 6 | 3038 | 3038 | 8916 | 122156 | 1391 | 618 | 79 | 1 | 949 | 40.2 | 26890 |
| 7 | 3017 | 3017 | 8939 | 122133 | 1403 | 620 | 76 | 1 | 917 | 40.5 | 28127 |


## log-lines

Timestamped log-style lines with stable fields and changing numeric values.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.1715 | 0.1716 | 772.48 | 769.81 | 2060.99 | 2192.21 |
| 2 | ok | ok | 0.1671 | 0.1671 | 736.12 | 642.86 | 1946.71 | 2085.38 |
| 3 | ok | ok | 0.1917 | 0.1917 | 504.06 | 512.41 | 1760.99 | 1979.93 |
| 4 | ok | ok | 0.2016 | 0.2016 | 463.33 | 493.35 | 1654.91 | 1845.27 |
| 5 | ok | ok | 0.1675 | 0.1679 | 224.02 | 247.47 | 2147.75 | 2399.90 |
| 6 | ok | ok | 0.1665 | 0.1669 | 184.41 | 188.47 | 2260.37 | 2458.28 |
| 7 | ok | ok | 0.1604 | 0.1609 | 163.00 | 174.84 | 2602.09 | 2676.76 |
| 8 | ok | ok | 0.1490 | 0.1506 | 134.65 | 142.57 | 2984.77 | 2950.22 |
| 9 | ok | ok | 0.1493 | 0.1505 | 130.21 | 138.81 | 3039.54 | 2968.25 |
| 10 | ok | ok | 0.1291 | 0.1342 | 101.36 | 109.32 | 3141.35 | 3197.99 |
| 11 | ok | ok | 0.1235 | 0.1298 | 78.64 | 76.23 | 3357.28 | 3376.95 |
| 12 | ok | ok | 0.1233 | 0.1297 | 69.57 | 74.27 | 3365.23 | 3414.13 |
| 13 | ok | ok | 0.1237 | 0.1300 | 47.55 | 43.68 | 3220.73 | 3293.02 |
| 14 | ok | ok | 0.1232 | 0.1295 | 29.39 | 28.20 | 3365.39 | 3411.90 |
| 15 | ok | ok | 0.1197 | 0.1270 | 24.18 | 22.98 | 3463.21 | 3500.46 |
| 16 | ok | ok | 0.1058 | 0.1058 | 4.63 | 4.61 | 3366.06 | 3689.01 |
| 17 | ok | ok | 0.1035 | 0.1035 | 4.02 | 4.21 | 3581.11 | 3898.98 |
| 18 | ok | ok | 0.1050 | 0.1050 | 3.12 | 3.29 | 3529.68 | 3878.32 |
| 19 | ok | ok | 0.1010 | 0.1010 | 2.39 | 2.33 | 3086.98 | 3428.67 |
| 20 | ok | ok | 0.1010 | 0.1010 | 2.29 | 2.26 | 3055.35 | 3382.21 |
| 21 | ok | ok | 0.1010 | 0.1010 | 2.37 | 2.26 | 3009.17 | 3411.48 |
| 22 | ok | ok | 0.1010 | 0.1010 | 2.42 | 2.38 | 3089.21 | 3410.91 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 719428 | 719665 | -0.03% | -0.00% |
| 2 | 701002 | 701002 | +0.00% | -0.00% |
| 3 | 803842 | 805666 | -0.23% | -0.00% |
| 4 | 845587 | 847357 | -0.21% | -0.00% |
| 5 | 702601 | 704287 | -0.24% | -0.00% |
| 6 | 698181 | 699889 | -0.24% | -0.00% |
| 7 | 672599 | 674797 | -0.33% | -0.00% |
| 8 | 624891 | 631474 | -1.04% | -0.00% |
| 9 | 626405 | 631314 | -0.78% | +0.00% |
| 10 | 541522 | 562802 | -3.78% | +0.00% |
| 11 | 517889 | 544533 | -4.89% | +0.00% |
| 12 | 517298 | 544045 | -4.92% | +0.00% |
| 13 | 518945 | 545060 | -4.79% | +0.00% |
| 14 | 516870 | 543191 | -4.85% | +0.00% |
| 15 | 502035 | 532496 | -5.72% | +0.00% |
| 16 | 443842 | 443842 | +0.00% | +0.00% |
| 17 | 433911 | 433911 | +0.00% | +0.00% |
| 18 | 440258 | 440258 | +0.00% | +0.00% |
| 19 | 423592 | 423592 | +0.00% | +0.00% |
| 20 | 423592 | 423592 | +0.00% | +0.00% |
| 21 | 423592 | 423592 | +0.00% | +0.00% |
| 22 | 423592 | 423640 | -0.01% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | -5.72% | 15 |
| 128 KiB | +0.00% | 19 | -5.72% | 15 |
| 1 MiB | +0.00% | 19 | -5.72% | 15 |

### Rust First-Block Stage Timing

- Samples the first raw `block_size` chunk only, so the timing breakdown stays aligned with the real block-local hot path.
- Uses prepared dictionaries for dictionary-backed cases so the sample reflects encoder hot paths instead of repeated dictionary parsing.
- The stage table above is sampled with the planner's phase timers off, so its milliseconds and its shares are both the real encoder's. The two sub-breakdown tables below need those timers and are sampled separately, because a timer taken per lazy parser step costs far more than the step: with them on, this case's first block reads up to 18x its real time and 99% of the frame lands in `Plan`. Read the sub-breakdowns as shares of their own row and never against the table above.
- The planning sub-breakdown covers row and chain/extdict lazy paths; other planner families may still report zeros. The lazy parser phase sub-breakdown is instrumented for no-dict row and trained-dictionary chain/extdict cases, and likewise reports zeros elsewhere.
- Sampled on levels 3-7 over 3 iterations.

| Level | Sampled ms | Blocks | Compressed | Split % | Plan % | Lit % | Seq % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.79 | 1 | 1 | 0.1 | 69.4 | 6.1 | 19.5 | 4.9 |
| 4 | 0.85 | 1 | 1 | 0.0 | 70.5 | 4.6 | 18.9 | 6.0 |
| 5 | 1.90 | 1 | 1 | 0.0 | 88.5 | 2.0 | 8.0 | 1.4 |
| 6 | 2.40 | 1 | 1 | 0.0 | 91.9 | 1.8 | 5.3 | 1.1 |
| 7 | 2.61 | 1 | 1 | 0.0 | 93.0 | 1.7 | 4.8 | 0.6 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 2.26 | 1 | 1 | 1.8 | 1.0 | 7.2 | 89.8 | 0.2 |
| 4 | 2.22 | 1 | 1 | 1.8 | 1.0 | 7.0 | 90.0 | 0.2 |
| 5 | 1.64 | 1 | 1 | 2.6 | 1.6 | 6.8 | 88.6 | 0.4 |
| 6 | 1.45 | 1 | 1 | 2.6 | 1.5 | 7.1 | 88.5 | 0.2 |
| 7 | 1.19 | 1 | 1 | 3.2 | 1.8 | 7.1 | 87.5 | 0.3 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.3 | 22.7 | 0.0 | 55.0 |
| 4 | 22.7 | 22.6 | 0.0 | 54.7 |
| 5 | 22.5 | 22.8 | 0.0 | 54.8 |
| 6 | 22.4 | 22.6 | 0.0 | 55.0 |
| 7 | 22.4 | 22.6 | 0.0 | 54.9 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 5.1 | 0.0 | 2.2 | 6.4 | 11.4 | 67.1 |
| 6 | 5.6 | 0.0 | 3.6 | 6.3 | 11.8 | 66.4 |
| 7 | 8.9 | 0.0 | 4.0 | 6.0 | 11.7 | 64.1 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 16.0 | 40.6 | 0.0 | 2.8 | 5.8 | 34.9 |
| 6 | 13.2 | 32.9 | 16.5 | 2.1 | 4.5 | 30.9 |
| 7 | 11.0 | 28.0 | 25.6 | 1.9 | 3.6 | 29.8 |

### Rust First-Block Parser Stats

- One-shot trace of the first block at each level. Sequence counts, byte breakdowns, and repcode usage come from the parser trace, not from timing.
- Sampled on levels 3-7.

| Level | Sequences | C Seqs | Lit bytes | Match bytes | Rep1 | Rep2 | Rep3 | Rep1-1 | Explicit | Avg ML | Avg offset |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 7594 | 7594 | 17813 | 113259 | 1010 | 625 | 0 | 0 | 5959 | 14.9 | 15575 |
| 4 | 7614 | 7614 | 17747 | 113325 | 1010 | 629 | 0 | 0 | 5975 | 14.9 | 15846 |
| 5 | 5151 | 5151 | 16871 | 114201 | 855 | 351 | 143 | 0 | 3802 | 22.2 | 24078 |
| 6 | 4978 | 4978 | 16985 | 114087 | 840 | 230 | 137 | 0 | 3771 | 22.9 | 23446 |
| 7 | 4053 | 4053 | 17678 | 113394 | 625 | 93 | 232 | 0 | 3103 | 28.0 | 23761 |


## mixed-entropy

Alternating compressible and incompressible 8 KiB regions.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.3336 | 0.3336 | 8464.78 | 4416.00 | 36718.30 | 28190.73 |
| 2 | ok | ok | 0.3335 | 0.3335 | 8155.47 | 4026.29 | 35092.53 | 27717.33 |
| 3 | ok | ok | 0.3327 | 0.3327 | 3290.79 | 2807.50 | 33635.25 | 37109.52 |
| 4 | ok | ok | 0.3327 | 0.3327 | 3105.81 | 2770.65 | 33803.84 | 37685.85 |
| 5 | ok | ok | 0.3327 | 0.3327 | 1224.82 | 1284.45 | 34133.90 | 38160.54 |
| 6 | ok | ok | 0.3327 | 0.3327 | 1104.28 | 1129.24 | 34855.92 | 37399.56 |
| 7 | ok | ok | 0.3327 | 0.3327 | 1078.55 | 1123.75 | 33300.99 | 38146.33 |
| 8 | ok | ok | 0.3327 | 0.3327 | 880.90 | 891.57 | 34352.33 | 38773.19 |
| 9 | ok | ok | 0.3327 | 0.3327 | 785.75 | 844.02 | 33615.10 | 38134.96 |
| 10 | ok | ok | 0.3327 | 0.3327 | 655.07 | 674.37 | 34101.88 | 38237.49 |
| 11 | ok | ok | 0.3327 | 0.3327 | 610.50 | 628.63 | 34182.04 | 37691.40 |
| 12 | ok | ok | 0.3327 | 0.3327 | 489.97 | 509.37 | 34857.40 | 38351.50 |
| 13 | ok | ok | 0.3326 | 0.3326 | 270.44 | 312.29 | 34276.15 | 37104.14 |
| 14 | ok | ok | 0.3326 | 0.3326 | 219.07 | 240.26 | 34811.68 | 36914.20 |
| 15 | ok | ok | 0.3326 | 0.3326 | 215.96 | 224.94 | 33903.92 | 37536.66 |
| 16 | ok | ok | 0.3324 | 0.3328 | 60.01 | 79.99 | 36254.53 | 36558.37 |
| 17 | ok | ok | 0.3326 | 0.3328 | 58.39 | 77.50 | 36517.09 | 35846.81 |
| 18 | ok | ok | 0.3328 | 0.3327 | 44.94 | 61.75 | 38122.54 | 38817.29 |
| 19 | ok | ok | 0.3328 | 0.3327 | 49.07 | 59.62 | 37796.32 | 38899.86 |
| 20 | ok | ok | 0.3328 | 0.3327 | 38.68 | 53.01 | 37638.76 | 38840.84 |
| 21 | ok | ok | 0.3328 | 0.3327 | 38.87 | 54.46 | 38251.18 | 38994.67 |
| 22 | ok | ok | 0.3328 | 0.3327 | 38.86 | 51.73 | 37005.14 | 38502.03 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1399332 | 1399332 | +0.00% | -0.00% |
| 2 | 1398635 | 1398635 | +0.00% | -0.00% |
| 3 | 1398235 | 1398235 | +0.00% | +0.21% |
| 4 | 1398235 | 1398235 | +0.00% | +0.21% |
| 5 | 1398209 | 1398211 | -0.00% | +0.21% |
| 6 | 1398211 | 1398211 | +0.00% | +0.21% |
| 7 | 1398211 | 1398211 | +0.00% | +0.21% |
| 8 | 1398211 | 1398211 | +0.00% | +0.21% |
| 9 | 1398211 | 1398211 | +0.00% | +0.21% |
| 10 | 1398211 | 1398211 | +0.00% | +0.21% |
| 11 | 1398211 | 1398211 | +0.00% | +0.21% |
| 12 | 1398211 | 1398211 | +0.00% | +0.21% |
| 13 | 1397974 | 1397990 | -0.00% | +0.20% |
| 14 | 1397978 | 1397993 | -0.00% | +0.20% |
| 15 | 1397980 | 1397997 | -0.00% | +0.20% |
| 16 | 1394027 | 1394165 | -0.01% | +0.00% |
| 17 | 1395055 | 1395370 | -0.02% | +0.00% |
| 18 | 1395682 | 1395910 | -0.02% | +0.00% |
| 19 | 1395682 | 1395911 | -0.02% | +0.00% |
| 20 | 1395682 | 1395911 | -0.02% | +0.00% |
| 21 | 1395682 | 1395911 | -0.02% | +0.00% |
| 22 | 1395682 | 1395649 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 9 | -0.02% | 19 |
| 128 KiB | +0.00% | 9 | -0.02% | 19 |
| 1 MiB | +0.00% | 9 | -0.02% | 19 |


## wikipedia

Encyclopaedic prose with structural repetition and moderate vocabulary.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0700 | 0.0700 | 1458.59 | 1510.17 | 3769.43 | 3843.55 |
| 2 | ok | ok | 0.0716 | 0.0716 | 1539.22 | 1546.41 | 3863.83 | 3830.54 |
| 3 | ok | ok | 0.0129 | 0.0131 | 3615.56 | 3326.28 | 11292.81 | 12838.32 |
| 4 | ok | ok | 0.0129 | 0.0131 | 3400.22 | 3237.59 | 11178.43 | 12672.96 |
| 5 | ok | ok | 0.0050 | 0.0050 | 2370.33 | 2184.16 | 26199.48 | 25485.32 |
| 6 | ok | ok | 0.0039 | 0.0039 | 2116.41 | 1967.43 | 27734.14 | 29128.98 |
| 7 | ok | ok | 0.0027 | 0.0028 | 2480.31 | 2319.49 | 28315.39 | 29873.39 |
| 8 | ok | ok | 0.0027 | 0.0028 | 1706.72 | 1597.24 | 27850.75 | 29513.15 |
| 9 | ok | ok | 0.0027 | 0.0028 | 1636.62 | 1554.27 | 28165.72 | 29921.63 |
| 10 | ok | ok | 0.0026 | 0.0027 | 1392.09 | 1361.01 | 29042.09 | 30236.77 |
| 11 | ok | ok | 0.0026 | 0.0027 | 1267.54 | 1202.45 | 28827.14 | 29994.14 |
| 12 | ok | ok | 0.0026 | 0.0027 | 1206.71 | 1146.13 | 29128.15 | 30436.33 |
| 13 | ok | ok | 0.0026 | 0.0027 | 776.30 | 768.55 | 29216.03 | 30627.50 |
| 14 | ok | ok | 0.0026 | 0.0026 | 722.09 | 746.23 | 29279.80 | 30548.93 |
| 15 | ok | ok | 0.0025 | 0.0026 | 649.55 | 657.52 | 29479.64 | 30832.23 |
| 16 | ok | ok | 0.0025 | 0.0025 | 79.28 | 68.30 | 28695.73 | 31332.23 |
| 17 | ok | ok | 0.0024 | 0.0024 | 74.42 | 64.26 | 28245.22 | 31970.03 |
| 18 | ok | ok | 0.0026 | 0.0026 | 69.31 | 62.46 | 28029.64 | 31942.10 |
| 19 | ok | ok | 0.0026 | 0.0026 | 38.33 | 29.84 | 28647.76 | 32144.65 |
| 20 | ok | ok | 0.0026 | 0.0026 | 38.22 | 28.87 | 28440.63 | 31672.80 |
| 21 | ok | ok | 0.0023 | 0.0023 | 25.17 | 18.04 | 28927.24 | 31478.07 |
| 22 | ok | ok | 0.0022 | 0.0022 | 8.28 | 5.36 | 28323.82 | 32144.65 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 293507 | 293507 | +0.00% | -0.00% |
| 2 | 300467 | 300467 | +0.00% | -0.00% |
| 3 | 53978 | 54752 | -1.41% | -0.00% |
| 4 | 53978 | 54752 | -1.41% | -0.00% |
| 5 | 21077 | 21081 | -0.02% | -0.00% |
| 6 | 16374 | 16399 | -0.15% | -0.01% |
| 7 | 11327 | 11625 | -2.56% | -0.01% |
| 8 | 11319 | 11613 | -2.53% | -0.01% |
| 9 | 11352 | 11647 | -2.53% | +0.00% |
| 10 | 11047 | 11369 | -2.83% | +0.00% |
| 11 | 11017 | 11341 | -2.86% | +0.00% |
| 12 | 11017 | 11341 | -2.86% | +0.00% |
| 13 | 10807 | 11171 | -3.26% | +0.00% |
| 14 | 10746 | 11107 | -3.25% | +0.00% |
| 15 | 10663 | 11024 | -3.27% | +0.00% |
| 16 | 10310 | 10310 | +0.00% | +0.00% |
| 17 | 10114 | 10114 | +0.00% | +0.00% |
| 18 | 11101 | 11101 | +0.00% | +0.00% |
| 19 | 10787 | 10787 | +0.00% | +0.00% |
| 20 | 10787 | 10787 | +0.00% | +0.00% |
| 21 | 9719 | 9718 | +0.01% | +0.00% |
| 22 | 9245 | 9248 | -0.03% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | -3.27% | 15 |
| 128 KiB | +0.00% | 19 | -3.27% | 15 |
| 1 MiB | +0.00% | 19 | -3.27% | 15 |


## tabular-csv

CSV rows with column repetition and numeric variation.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.2001 | 0.2001 | 642.49 | 660.97 | 1963.56 | 2086.13 |
| 2 | ok | ok | 0.1380 | 0.1380 | 656.51 | 627.29 | 1906.50 | 1912.52 |
| 3 | ok | ok | 0.1518 | 0.1518 | 536.61 | 548.52 | 1982.89 | 1897.16 |
| 4 | ok | ok | 0.1514 | 0.1514 | 503.77 | 558.89 | 2081.27 | 1997.73 |
| 5 | ok | ok | 0.1476 | 0.1499 | 235.40 | 245.50 | 1807.81 | 1732.86 |
| 6 | ok | ok | 0.1349 | 0.1384 | 138.61 | 145.11 | 2238.56 | 2110.95 |
| 7 | ok | ok | 0.1391 | 0.1421 | 125.81 | 119.29 | 2151.56 | 2125.76 |
| 8 | ok | ok | 0.1269 | 0.1295 | 87.66 | 98.72 | 2074.46 | 2121.29 |
| 9 | ok | ok | 0.1272 | 0.1297 | 88.13 | 88.09 | 2097.48 | 2079.38 |
| 10 | ok | ok | 0.1276 | 0.1310 | 67.14 | 69.31 | 2065.63 | 2100.08 |
| 11 | ok | ok | 0.1349 | 0.1400 | 45.88 | 50.58 | 2271.42 | 2301.67 |
| 12 | ok | ok | 0.1341 | 0.1396 | 48.17 | 43.39 | 2315.75 | 2315.40 |
| 13 | ok | ok | 0.1448 | 0.1524 | 32.45 | 31.87 | 2529.57 | 2572.50 |
| 14 | ok | ok | 0.1456 | 0.1539 | 25.89 | 26.81 | 2624.98 | 2657.22 |
| 15 | ok | ok | 0.1367 | 0.1474 | 21.26 | 20.07 | 2791.28 | 2780.65 |
| 16 | ok | ok | 0.1172 | 0.1172 | 4.73 | 5.25 | 1479.53 | 1683.69 |
| 17 | ok | ok | 0.1111 | 0.1111 | 4.37 | 4.53 | 1536.62 | 1719.59 |
| 18 | ok | ok | 0.0896 | 0.0897 | 2.77 | 2.79 | 1075.63 | 1224.36 |
| 19 | ok | ok | 0.1099 | 0.1099 | 2.05 | 2.14 | 1545.72 | 1690.24 |
| 20 | ok | ok | 0.1099 | 0.1099 | 2.06 | 2.10 | 1481.34 | 1656.16 |
| 21 | ok | ok | 0.1099 | 0.1099 | 2.08 | 2.11 | 1525.90 | 1710.20 |
| 22 | ok | ok | 0.1094 | 0.1094 | 1.13 | 1.17 | 1537.69 | 1708.80 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 839143 | 839172 | -0.00% | -0.00% |
| 2 | 578789 | 578807 | -0.00% | -0.00% |
| 3 | 636656 | 641501 | -0.76% | -0.00% |
| 4 | 635004 | 642754 | -1.21% | -0.00% |
| 5 | 619153 | 628604 | -1.50% | -0.00% |
| 6 | 565980 | 580506 | -2.50% | -0.00% |
| 7 | 583489 | 596146 | -2.12% | -0.00% |
| 8 | 532403 | 543126 | -1.97% | -0.00% |
| 9 | 533529 | 544092 | -1.94% | +0.00% |
| 10 | 535301 | 549584 | -2.60% | +0.00% |
| 11 | 565881 | 587115 | -3.62% | +0.00% |
| 12 | 562277 | 585381 | -3.95% | +0.00% |
| 13 | 607440 | 639024 | -4.94% | +0.00% |
| 14 | 610769 | 645651 | -5.40% | +0.00% |
| 15 | 573388 | 618401 | -7.28% | +0.00% |
| 16 | 491468 | 491471 | -0.00% | +0.00% |
| 17 | 466149 | 466160 | -0.00% | +0.00% |
| 18 | 375870 | 376433 | -0.15% | +0.00% |
| 19 | 460820 | 460787 | +0.01% | +0.00% |
| 20 | 460818 | 460784 | +0.01% | +0.00% |
| 21 | 460818 | 460784 | +0.01% | +0.00% |
| 22 | 459064 | 460725 | -0.36% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.01% | 19 | -7.28% | 15 |
| 128 KiB | +0.01% | 19 | -7.28% | 15 |
| 1 MiB | +0.01% | 19 | -7.28% | 15 |


## binary-structured

Repeating binary records with fixed headers and variable payloads.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.1379 | 0.1379 | 1517.11 | 1469.53 | 4278.49 | 4531.32 |
| 2 | ok | ok | 0.1379 | 0.1379 | 1562.89 | 1105.75 | 4280.02 | 4526.88 |
| 3 | ok | ok | 0.1337 | 0.1337 | 1267.59 | 1317.09 | 5700.54 | 5401.87 |
| 4 | ok | ok | 0.1337 | 0.1337 | 1239.92 | 1290.45 | 5724.97 | 5397.48 |
| 5 | ok | ok | 0.1337 | 0.1337 | 371.35 | 373.60 | 5700.47 | 5390.47 |
| 6 | ok | ok | 0.1333 | 0.1333 | 281.54 | 283.74 | 6241.59 | 5828.48 |
| 7 | ok | ok | 0.1333 | 0.1333 | 252.59 | 261.89 | 6269.08 | 5802.15 |
| 8 | ok | ok | 0.1361 | 0.1361 | 178.62 | 195.65 | 4928.17 | 4852.33 |
| 9 | ok | ok | 0.1361 | 0.1361 | 169.54 | 179.41 | 4949.36 | 4855.55 |
| 10 | ok | ok | 0.1361 | 0.1361 | 117.18 | 113.33 | 4945.37 | 4867.45 |
| 11 | ok | ok | 0.1361 | 0.1361 | 99.76 | 94.04 | 4918.01 | 4861.43 |
| 12 | ok | ok | 0.1361 | 0.1361 | 88.27 | 82.27 | 4931.83 | 4824.98 |
| 13 | ok | ok | 0.1358 | 0.1358 | 82.96 | 75.54 | 4910.15 | 4847.23 |
| 14 | ok | ok | 0.1358 | 0.1358 | 67.40 | 58.34 | 4887.34 | 4802.74 |
| 15 | ok | ok | 0.1358 | 0.1358 | 60.34 | 54.43 | 4859.17 | 4804.53 |
| 16 | ok | ok | 0.1245 | 0.1245 | 5.68 | 5.37 | 3095.34 | 3191.49 |
| 17 | ok | ok | 0.1245 | 0.1245 | 5.39 | 5.47 | 3163.52 | 3259.30 |
| 18 | ok | ok | 0.1242 | 0.1242 | 4.43 | 4.22 | 3000.99 | 3118.57 |
| 19 | ok | ok | 0.1242 | 0.1242 | 4.37 | 4.10 | 3010.69 | 3106.70 |
| 20 | ok | ok | 0.1242 | 0.1242 | 4.34 | 4.09 | 3007.77 | 3102.70 |
| 21 | ok | ok | 0.1242 | 0.1242 | 4.27 | 4.10 | 3063.09 | 3153.68 |
| 22 | ok | ok | 0.1242 | 0.1242 | 4.34 | 3.98 | 3065.24 | 3186.34 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 578438 | 578542 | -0.02% | -0.00% |
| 2 | 578438 | 578438 | +0.00% | -0.00% |
| 3 | 560605 | 560605 | +0.00% | -0.00% |
| 4 | 560605 | 560605 | +0.00% | -0.00% |
| 5 | 560663 | 560664 | -0.00% | -0.00% |
| 6 | 559045 | 559052 | -0.00% | -0.00% |
| 7 | 559045 | 559052 | -0.00% | -0.00% |
| 8 | 557845 | 557855 | -0.00% | -2.26% |
| 9 | 557845 | 557845 | +0.00% | -2.26% |
| 10 | 557845 | 557845 | +0.00% | -2.26% |
| 11 | 557845 | 557845 | +0.00% | -2.26% |
| 12 | 557845 | 557845 | +0.00% | -2.26% |
| 13 | 557774 | 557774 | +0.00% | -2.10% |
| 14 | 557774 | 557774 | +0.00% | -2.10% |
| 15 | 557774 | 557774 | +0.00% | -2.10% |
| 16 | 522221 | 522182 | +0.01% | +0.00% |
| 17 | 522226 | 522187 | +0.01% | +0.00% |
| 18 | 520963 | 520958 | +0.00% | +0.00% |
| 19 | 520950 | 520945 | +0.00% | +0.00% |
| 20 | 520950 | 520945 | +0.00% | +0.00% |
| 21 | 520950 | 520945 | +0.00% | +0.00% |
| 22 | 520950 | 520945 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | -0.02% | 1 |
| 128 KiB | +0.00% | 19 | -0.02% | 1 |
| 1 MiB | +0.00% | 19 | -0.02% | 1 |

### Rust First-Block Stage Timing

- Samples the first raw `block_size` chunk only, so the timing breakdown stays aligned with the real block-local hot path.
- Uses prepared dictionaries for dictionary-backed cases so the sample reflects encoder hot paths instead of repeated dictionary parsing.
- The stage table above is sampled with the planner's phase timers off, so its milliseconds and its shares are both the real encoder's. The two sub-breakdown tables below need those timers and are sampled separately, because a timer taken per lazy parser step costs far more than the step: with them on, this case's first block reads up to 18x its real time and 99% of the frame lands in `Plan`. Read the sub-breakdowns as shares of their own row and never against the table above.
- The planning sub-breakdown covers row and chain/extdict lazy paths; other planner families may still report zeros. The lazy parser phase sub-breakdown is instrumented for no-dict row and trained-dictionary chain/extdict cases, and likewise reports zeros elsewhere.
- Sampled on levels 3-7 over 3 iterations.

| Level | Sampled ms | Blocks | Compressed | Split % | Plan % | Lit % | Seq % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.35 | 1 | 1 | 0.1 | 62.4 | 4.1 | 24.7 | 8.7 |
| 4 | 0.40 | 1 | 1 | 0.0 | 62.3 | 3.5 | 20.8 | 13.3 |
| 5 | 1.16 | 1 | 1 | 0.0 | 86.0 | 4.5 | 7.2 | 2.3 |
| 6 | 1.35 | 1 | 1 | 0.0 | 92.0 | 1.0 | 5.9 | 1.1 |
| 7 | 1.60 | 1 | 1 | 0.0 | 93.2 | 0.9 | 4.9 | 0.9 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.12 | 1 | 1 | 0.1 | 2.3 | 7.5 | 89.7 | 0.4 |
| 4 | 1.11 | 1 | 1 | 0.0 | 2.2 | 7.3 | 90.2 | 0.3 |
| 5 | 1.10 | 1 | 1 | 0.0 | 2.0 | 7.1 | 90.5 | 0.3 |
| 6 | 0.95 | 1 | 1 | 0.1 | 2.6 | 7.1 | 89.8 | 0.4 |
| 7 | 0.94 | 1 | 1 | 0.0 | 2.6 | 7.2 | 89.6 | 0.6 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.2 | 22.7 | 0.0 | 55.1 |
| 4 | 22.0 | 22.6 | 0.0 | 55.4 |
| 5 | 22.5 | 22.8 | 0.0 | 54.7 |
| 6 | 22.6 | 23.0 | 0.0 | 54.4 |
| 7 | 21.9 | 22.5 | 0.0 | 55.5 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 6.3 | 0.0 | 0.9 | 6.1 | 12.7 | 65.7 |
| 6 | 5.7 | 0.0 | 2.6 | 6.2 | 12.8 | 65.6 |
| 7 | 5.7 | 0.0 | 3.9 | 6.1 | 12.6 | 65.4 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 17.1 | 40.9 | 0.0 | 2.0 | 4.2 | 35.8 |
| 6 | 15.2 | 35.3 | 12.1 | 1.6 | 3.4 | 32.4 |
| 7 | 13.4 | 31.1 | 21.2 | 1.4 | 2.9 | 30.0 |

### Rust First-Block Parser Stats

- One-shot trace of the first block at each level. Sequence counts, byte breakdowns, and repcode usage come from the parser trace, not from timing.
- Sampled on levels 3-7.

| Level | Sequences | C Seqs | Lit bytes | Match bytes | Rep1 | Rep2 | Rep3 | Rep1-1 | Explicit | Avg ML | Avg offset |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 3835 | 3835 | 17030 | 114042 | 3795 | 17 | 0 | 0 | 23 | 29.7 | 57 |
| 4 | 3835 | 3835 | 17030 | 114042 | 3795 | 17 | 0 | 0 | 23 | 29.7 | 57 |
| 5 | 3835 | 3835 | 17030 | 114042 | 3795 | 17 | 0 | 0 | 23 | 29.7 | 57 |
| 6 | 3298 | 3298 | 16493 | 114579 | 3257 | 18 | 0 | 0 | 23 | 34.7 | 57 |
| 7 | 3298 | 3298 | 16493 | 114579 | 3257 | 18 | 0 | 0 | 23 | 34.7 | 57 |


## pseudorandom

Deterministic incompressible-looking bytes.

- Input bytes: 4194304
- Dictionary mode: none
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 1.0000 | 1.0000 | 12338.52 | 7668.21 | 46385.69 | 38796.70 |
| 2 | ok | ok | 1.0000 | 1.0000 | 11765.39 | 8259.43 | 46181.08 | 35476.72 |
| 3 | ok | ok | 1.0000 | 1.0000 | 9704.02 | 7520.87 | 47515.75 | 33909.53 |
| 4 | ok | ok | 1.0000 | 1.0000 | 9094.08 | 7659.23 | 45172.65 | 39607.02 |
| 5 | ok | ok | 1.0000 | 1.0000 | 4358.75 | 4438.67 | 45294.87 | 33053.58 |
| 6 | ok | ok | 1.0000 | 1.0000 | 4378.94 | 4497.62 | 47440.17 | 39548.90 |
| 7 | ok | ok | 1.0000 | 1.0000 | 3943.89 | 4420.62 | 47713.35 | 31405.26 |
| 8 | ok | ok | 1.0000 | 1.0000 | 4024.93 | 4420.48 | 47946.99 | 36099.56 |
| 9 | ok | ok | 1.0000 | 1.0000 | 3631.70 | 4033.82 | 45937.82 | 33248.91 |
| 10 | ok | ok | 1.0000 | 1.0000 | 2919.26 | 2816.69 | 47725.21 | 31156.82 |
| 11 | ok | ok | 1.0000 | 1.0000 | 2618.40 | 2589.00 | 43888.69 | 33531.99 |
| 12 | ok | ok | 1.0000 | 1.0000 | 2071.53 | 1955.29 | 47085.87 | 31898.32 |
| 13 | ok | ok | 1.0000 | 1.0000 | 166.11 | 282.32 | 47275.36 | 32854.21 |
| 14 | ok | ok | 1.0000 | 1.0000 | 119.55 | 203.09 | 48234.77 | 40493.51 |
| 15 | ok | ok | 1.0000 | 1.0000 | 116.04 | 201.02 | 46735.23 | 32979.07 |
| 16 | ok | ok | 1.0000 | 1.0000 | 19.45 | 21.46 | 47647.85 | 37002.24 |
| 17 | ok | ok | 1.0000 | 1.0000 | 15.83 | 18.72 | 46958.12 | 32090.25 |
| 18 | ok | ok | 1.0000 | 1.0000 | 12.40 | 16.03 | 46052.66 | 35939.91 |
| 19 | ok | ok | 1.0000 | 1.0000 | 13.17 | 15.02 | 44946.29 | 39260.79 |
| 20 | ok | ok | 1.0000 | 1.0000 | 12.72 | 16.91 | 46816.22 | 35337.15 |
| 21 | ok | ok | 1.0000 | 1.0000 | 12.53 | 17.00 | 48506.01 | 31565.97 |
| 22 | ok | ok | 1.0000 | 1.0000 | 12.54 | 16.44 | 46822.32 | 34058.40 |
### Streaming vs Upstream Streaming

- Both sides are fed 32 KiB at a time with no pledged source size, so both frames declare a window rather than a content size.
- `delta` is signed: positive means this crate emitted more than upstream. `vs one-shot` is this crate's streaming output against its own one-shot output at the same level.
- Every frame behind this table is decoded back to the original input before its size is recorded, in both directions.

| Level | Rust stream | zstd stream | delta | vs one-shot |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 4194409 | 4194409 | +0.00% | -0.00% |
| 2 | 4194409 | 4194409 | +0.00% | -0.00% |
| 3 | 4194409 | 4194409 | +0.00% | -0.00% |
| 4 | 4194409 | 4194409 | +0.00% | -0.00% |
| 5 | 4194409 | 4194409 | +0.00% | -0.00% |
| 6 | 4194409 | 4194409 | +0.00% | -0.00% |
| 7 | 4194409 | 4194409 | +0.00% | -0.00% |
| 8 | 4194409 | 4194409 | +0.00% | -0.00% |
| 9 | 4194409 | 4194409 | +0.00% | +0.00% |
| 10 | 4194409 | 4194409 | +0.00% | +0.00% |
| 11 | 4194409 | 4194409 | +0.00% | +0.00% |
| 12 | 4194409 | 4194409 | +0.00% | +0.00% |
| 13 | 4194409 | 4194409 | +0.00% | +0.00% |
| 14 | 4194409 | 4194409 | +0.00% | +0.00% |
| 15 | 4194409 | 4194409 | +0.00% | +0.00% |
| 16 | 4194409 | 4194409 | +0.00% | +0.00% |
| 17 | 4194409 | 4194409 | +0.00% | +0.00% |
| 18 | 4194409 | 4194409 | +0.00% | +0.00% |
| 19 | 4194409 | 4194409 | +0.00% | +0.00% |
| 20 | 4194409 | 4194409 | +0.00% | +0.00% |
| 21 | 4194409 | 4194409 | +0.00% | +0.00% |
| 22 | 4194409 | 4194409 | +0.00% | +0.00% |

### Streaming Piece-Size Sensitivity

- Block layout is a function of how much input arrives per call, so the table above is one sample of a curve. Sampled on levels 1,3,9,15,19, one per parser strategy.
- 128 KiB is the block max: upstream's buffered path hands its frame-chunk loop one of those per call, so it is the alignment where a chunk yields at most two blocks.

| Piece | Most over upstream | Level | Most under upstream | Level |
| ---: | ---: | ---: | ---: | ---: |
| 16 KiB | +0.00% | 19 | +0.00% | 1 |
| 128 KiB | +0.00% | 19 | +0.00% | 1 |
| 1 MiB | +0.00% | 19 | +0.00% | 1 |


## raw-dictionary

HTTP-like records aligned with the raw-content dictionary fixture.

- Input bytes: 4194304
- Dictionary mode: raw-content
- Dictionary bytes: 156 (1 per 26886 bytes of input)
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0148 | 0.0148 | 4040.59 | 3627.20 | 9558.26 | 10217.69 |
| 2 | ok | ok | 0.0148 | 0.0148 | 3989.86 | 3570.26 | 9652.56 | 10342.46 |
| 3 | ok | ok | 0.0138 | 0.0187 | 2566.72 | 2667.20 | 8464.79 | 9265.54 |
| 4 | ok | ok | 0.0334 | 0.0334 | 666.24 | 862.08 | 5729.83 | 5811.18 |
| 5 | ok | ok | 0.0315 | 0.0315 | 546.52 | 711.83 | 7527.98 | 7530.32 |
| 6 | ok | ok | 0.0289 | 0.0289 | 415.16 | 516.04 | 8524.39 | 8280.97 |
| 7 | ok | ok | 0.0289 | 0.0289 | 399.53 | 498.04 | 8309.43 | 8300.05 |
| 8 | ok | ok | 0.0289 | 0.0289 | 405.09 | 495.55 | 8599.36 | 8397.56 |
| 9 | ok | ok | 0.0139 | 0.0139 | 301.87 | 453.86 | 10540.51 | 11967.43 |
| 10 | ok | ok | 0.0139 | 0.0139 | 302.16 | 449.86 | 10533.90 | 11997.22 |
| 11 | ok | ok | 0.0124 | 0.0124 | 28.77 | 24.62 | 14002.51 | 15147.06 |
| 12 | ok | ok | 0.0125 | 0.0125 | 27.41 | 24.40 | 13377.24 | 14858.05 |
| 13 | ok | ok | 0.0125 | 0.0125 | 27.28 | 24.18 | 13190.11 | 14738.87 |
| 14 | ok | ok | 0.0124 | 0.0124 | 27.45 | 23.48 | 13838.06 | 14784.17 |
| 15 | ok | ok | 0.0127 | 0.0127 | 6.19 | 4.72 | 12414.96 | 13414.95 |
| 16 | ok | ok | 0.0125 | 0.0125 | 27.43 | 23.85 | 12776.43 | 14177.08 |
| 17 | ok | ok | 0.0126 | 0.0126 | 26.80 | 23.49 | 12373.48 | 13779.31 |
| 18 | ok | ok | 0.0127 | 0.0127 | 6.21 | 4.73 | 12232.25 | 13554.64 |
| 19 | ok | ok | 0.0127 | 0.0127 | 6.21 | 4.72 | 12337.28 | 13617.77 |
| 20 | ok | ok | 0.0127 | 0.0127 | 6.21 | 4.74 | 12278.50 | 13604.99 |
| 21 | ok | ok | 0.0127 | 0.0127 | 6.19 | 4.72 | 12287.03 | 13477.94 |
| 22 | ok | ok | 0.0127 | 0.0127 | 6.21 | 4.73 | 12389.10 | 13613.51 |

## trained-dictionary

Structured multi-endpoint records aligned with the trained dictionary fixture.

- Input bytes: 4194304
- Dictionary mode: trained
- Dictionary bytes: 512 (1 per 8192 bytes of input)
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0456 | 0.0456 | 1695.63 | 1709.33 | 4329.48 | 4248.92 |
| 2 | ok | ok | 0.0528 | 0.0528 | 1443.53 | 1561.14 | 3944.86 | 3938.28 |
| 3 | ok | ok | 0.0582 | 0.0587 | 917.00 | 1244.88 | 3270.92 | 3245.22 |
| 4 | ok | ok | 0.0643 | 0.0643 | 463.60 | 626.30 | 2732.01 | 2804.45 |
| 5 | ok | ok | 0.0614 | 0.0614 | 344.27 | 454.07 | 2900.09 | 2994.37 |
| 6 | ok | ok | 0.0532 | 0.0532 | 234.29 | 310.48 | 3126.21 | 3379.83 |
| 7 | ok | ok | 0.0532 | 0.0532 | 232.88 | 301.33 | 3093.16 | 3264.97 |
| 8 | ok | ok | 0.0532 | 0.0532 | 230.39 | 302.42 | 3154.68 | 3295.56 |
| 9 | ok | ok | 0.0528 | 0.0528 | 124.59 | 185.77 | 3129.14 | 3396.80 |
| 10 | ok | ok | 0.0528 | 0.0528 | 125.22 | 186.90 | 3143.88 | 3286.59 |
| 11 | ok | ok | 0.0458 | 0.0467 | 30.66 | 33.27 | 4179.82 | 4264.39 |
| 12 | ok | ok | 0.0431 | 0.0439 | 27.10 | 30.51 | 4525.33 | 4419.81 |
| 13 | ok | ok | 0.0436 | 0.0442 | 20.68 | 21.18 | 4275.10 | 4271.96 |
| 14 | ok | ok | 0.0384 | 0.0385 | 11.38 | 9.41 | 2803.56 | 3224.56 |
| 15 | ok | ok | 0.0393 | 0.0442 | 9.08 | 7.83 | 3859.01 | 4151.93 |
| 16 | ok | ok | 0.0432 | 0.0432 | 16.61 | 15.48 | 3725.98 | 4005.71 |
| 17 | ok | ok | 0.0393 | 0.0441 | 9.10 | 7.74 | 3593.56 | 3867.77 |
| 18 | ok | ok | 0.0393 | 0.0441 | 9.14 | 7.73 | 3573.06 | 3860.94 |
| 19 | ok | ok | 0.0393 | 0.0441 | 9.07 | 7.73 | 3589.14 | 3847.52 |
| 20 | ok | ok | 0.0393 | 0.0441 | 9.07 | 7.73 | 3596.11 | 3863.36 |
| 21 | ok | ok | 0.0393 | 0.0441 | 8.81 | 7.73 | 3550.06 | 3813.22 |
| 22 | ok | ok | 0.0393 | 0.0441 | 9.09 | 7.49 | 3541.76 | 3761.56 |
### Rust First-Block Stage Timing

- Samples the first raw `block_size` chunk only, so the timing breakdown stays aligned with the real block-local hot path.
- Uses prepared dictionaries for dictionary-backed cases so the sample reflects encoder hot paths instead of repeated dictionary parsing.
- The stage table above is sampled with the planner's phase timers off, so its milliseconds and its shares are both the real encoder's. The two sub-breakdown tables below need those timers and are sampled separately, because a timer taken per lazy parser step costs far more than the step: with them on, this case's first block reads up to 18x its real time and 99% of the frame lands in `Plan`. Read the sub-breakdowns as shares of their own row and never against the table above.
- The planning sub-breakdown covers row and chain/extdict lazy paths; other planner families may still report zeros. The lazy parser phase sub-breakdown is instrumented for no-dict row and trained-dictionary chain/extdict cases, and likewise reports zeros elsewhere.
- Sampled on levels 3-7 over 3 iterations.

| Level | Sampled ms | Blocks | Compressed | Split % | Plan % | Lit % | Seq % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.53 | 1 | 1 | 1.0 | 71.9 | 4.6 | 19.0 | 3.5 |
| 4 | 0.77 | 1 | 1 | 0.0 | 84.4 | 2.6 | 12.3 | 0.6 |
| 5 | 1.13 | 1 | 1 | 0.0 | 88.9 | 2.2 | 8.5 | 0.4 |
| 6 | 1.45 | 1 | 1 | 0.0 | 92.1 | 1.9 | 5.7 | 0.3 |
| 7 | 1.39 | 1 | 1 | 0.0 | 92.3 | 1.9 | 5.5 | 0.3 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.33 | 1 | 1 | 1.4 | 1.7 | 7.2 | 89.1 | 0.5 |
| 4 | 1.30 | 1 | 1 | 1.4 | 1.7 | 7.5 | 89.0 | 0.5 |
| 5 | 1.20 | 1 | 1 | 1.6 | 1.9 | 7.3 | 88.9 | 0.5 |
| 6 | 1.08 | 1 | 1 | 2.0 | 2.1 | 6.9 | 87.9 | 1.1 |
| 7 | 1.00 | 1 | 1 | 2.2 | 2.4 | 7.3 | 87.6 | 0.6 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 20.8 | 19.3 | 0.2 | 59.7 |
| 4 | 22.9 | 21.4 | 0.1 | 55.6 |
| 5 | 22.1 | 20.6 | 0.2 | 57.2 |
| 6 | 23.3 | 21.9 | 0.2 | 54.7 |
| 7 | 22.7 | 21.4 | 0.1 | 55.8 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 9.7 | 1.2 | 10.4 | 5.4 | 64.5 |
| 5 | 0.0 | 19.3 | 4.1 | 8.8 | 6.9 | 55.2 |
| 6 | 0.0 | 23.6 | 6.5 | 8.3 | 6.8 | 50.4 |
| 7 | 0.0 | 24.0 | 6.5 | 8.4 | 6.8 | 50.2 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 19.1 | 15.7 | 0.0 | 5.8 | 14.1 | 45.3 |
| 5 | 14.4 | 15.4 | 15.9 | 4.4 | 9.9 | 40.0 |
| 6 | 11.9 | 13.0 | 26.5 | 3.6 | 7.2 | 37.8 |
| 7 | 11.9 | 13.2 | 26.4 | 3.5 | 7.0 | 38.1 |

### Rust First-Block Parser Stats

- One-shot trace of the first block at each level. Sequence counts, byte breakdowns, and repcode usage come from the parser trace, not from timing.
- Sampled on levels 3-7.

| Level | Sequences | C Seqs | Lit bytes | Match bytes | Rep1 | Rep2 | Rep3 | Rep1-1 | Explicit | Avg ML | Avg offset |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 4565 | 4566 | 4579 | 126493 | 2057 | 1123 | 0 | 0 | 1385 | 27.7 | 10495 |
| 4 | 4542 | 4542 | 4455 | 126617 | 2190 | 1137 | 0 | 0 | 1215 | 27.9 | 13381 |
| 5 | 4232 | 4232 | 5285 | 125787 | 2405 | 875 | 0 | 0 | 952 | 29.7 | 14604 |
| 6 | 3489 | 3489 | 7736 | 123336 | 3181 | 139 | 0 | 0 | 169 | 35.3 | 12193 |
| 7 | 3489 | 3489 | 7736 | 123336 | 3181 | 139 | 0 | 0 | 169 | 35.3 | 12193 |


