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
| zstandard revision | `v0.1.8-4-g4f61ec4` |
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
| Encode rows below 50% | 5 |
| Decode rows below 50% | 1 |
| Ratio regressions | 26 |
| Ratio regressions above 1% | 0 |
| Cases behind upstream on a third of encode levels | 4 |
| Cases behind upstream on a third of decode levels | 1 |
| Streaming rows above upstream | 18 |
| Streaming rows above upstream by 1% | 0 |
| Streaming rows below upstream by 1% | 65 |

### Throughput by Case

This crate's throughput as a fraction of upstream's, summarized over the benchmarked levels. Above 1.00x is this crate being faster.

The `slow` column counts the case's levels below 90% of upstream, and it is the one to read. A single row's throughput moves a tenth between sweeps of identical code, so no one row means anything here; the median is worth less than it looks too, because a case split into a slow band and a fast one has its median on the boundary between them and crosses it on noise. The worst column says which level to open first once `slow` has flagged the case.

| Case | Encode slow | Encode median | Encode worst | Level | Decode slow | Decode median | Decode worst | Level |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| small-alphabet | 0/22 | 1.07x | 0.93x | 13 | 0/22 | 4.45x | 2.43x | 16 |
| repeated-chunk | 0/22 | 1.16x | 1.04x | 12 | 0/22 | 3.22x | 1.56x | 17 |
| json-records | 0/22 | 1.11x | 0.94x | 5 | 0/22 | 0.98x | 0.90x | 21 |
| log-lines | 3/22 | 0.99x | 0.87x | 18 | 5/22 | 0.93x | 0.89x | 19 |
| mixed-entropy | 8/22 | 0.94x | 0.76x | 22 | 1/22 | 0.95x | 0.85x | 12 |
| wikipedia | 0/22 | 1.09x | 0.95x | 1 | 1/22 | 0.97x | 0.90x | 4 |
| tabular-csv | 1/22 | 0.97x | 0.89x | 18 | 4/22 | 1.00x | 0.86x | 22 |
| binary-structured | 0/22 | 1.04x | 0.93x | 3 | 0/22 | 1.02x | 0.94x | 2 |
| pseudorandom | 9/22 | 0.95x | 0.55x | 14 | 0/22 | 1.39x | 1.04x | 1 |
| raw-dictionary | 8/22 | 1.04x | 0.67x | 9 | 8/22 | 0.91x | 0.66x | 22 |
| trained-dictionary | 12/22 | 0.75x | 0.30x | 9 | 7/22 | 0.94x | 0.38x | 3 |

### Encode Rows Below 50%

- trained-dictionary L3 encode 0.35x (300.30 / 867.25 MiB/s)
- trained-dictionary L6 encode 0.39x (88.63 / 225.58 MiB/s)
- trained-dictionary L9 encode 0.30x (47.16 / 159.74 MiB/s)
- trained-dictionary L10 encode 0.30x (46.63 / 156.01 MiB/s)
- trained-dictionary L11 encode 0.37x (8.88 / 24.31 MiB/s)

### Decode Rows Below 50%

- trained-dictionary L3 decode 0.38x (1145.55 / 2991.52 MiB/s)

### Ratio Regressions

Rows where this crate emitted more bytes than upstream, largest relative excess first. The comparison is on exact byte counts: most of these rows differ by a handful of bytes on a multi-megabyte case and are listed for completeness, not as defects.

- raw-dictionary L18 +0.08% (53410 vs 53368 bytes, +42)
- raw-dictionary L19 +0.08% (53410 vs 53368 bytes, +42)
- raw-dictionary L20 +0.08% (53410 vs 53368 bytes, +42)
- raw-dictionary L21 +0.08% (53410 vs 53368 bytes, +42)
- raw-dictionary L22 +0.08% (53410 vs 53368 bytes, +42)
- raw-dictionary L9 +0.05% (58377 vs 58345 bytes, +32)
- raw-dictionary L10 +0.05% (58377 vs 58345 bytes, +32)
- raw-dictionary L14 +0.02% (52211 vs 52200 bytes, +11)
- wikipedia L21 +0.01% (9719 vs 9718 bytes, +1)
- tabular-csv L22 +0.01% (459064 vs 459021 bytes, +43)
- binary-structured L16 +0.01% (522221 vs 522182 bytes, +39)
- binary-structured L17 +0.01% (522226 vs 522187 bytes, +39)
- tabular-csv L20 +0.01% (460817 vs 460784 bytes, +33)
- tabular-csv L21 +0.01% (460817 vs 460784 bytes, +33)
- tabular-csv L19 +0.01% (460820 vs 460787 bytes, +33)
- mixed-entropy L18 +0.01% (1395682 vs 1395604 bytes, +78)
- mixed-entropy L19 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L20 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L21 +0.01% (1395682 vs 1395605 bytes, +77)
- mixed-entropy L22 +0.01% (1395682 vs 1395605 bytes, +77)
- raw-dictionary L16 +0.00% (52525 vs 52524 bytes, +1)
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
| 1 | ok | ok | 0.0001 | 0.0001 | 16207.97 | 14407.52 | 38831.91 | 5467.86 |
| 2 | ok | ok | 0.0001 | 0.0001 | 15827.42 | 14227.76 | 39955.00 | 5439.38 |
| 3 | ok | ok | 0.0001 | 0.0001 | 8720.32 | 8120.49 | 39282.63 | 5541.16 |
| 4 | ok | ok | 0.0001 | 0.0001 | 8342.49 | 8060.55 | 39419.74 | 5526.96 |
| 5 | ok | ok | 0.0001 | 0.0001 | 3940.51 | 3925.53 | 39737.41 | 5543.53 |
| 6 | ok | ok | 0.0001 | 0.0001 | 2771.86 | 2548.94 | 39428.34 | 8931.66 |
| 7 | ok | ok | 0.0001 | 0.0001 | 2711.11 | 2538.34 | 39014.97 | 8792.76 |
| 8 | ok | ok | 0.0001 | 0.0001 | 1477.99 | 1408.64 | 38607.76 | 8749.30 |
| 9 | ok | ok | 0.0001 | 0.0001 | 1435.95 | 1370.30 | 43481.64 | 8601.37 |
| 10 | ok | ok | 0.0001 | 0.0001 | 1422.24 | 1374.24 | 40424.38 | 8704.21 |
| 11 | ok | ok | 0.0001 | 0.0001 | 1421.05 | 1351.35 | 38880.78 | 8709.02 |
| 12 | ok | ok | 0.0001 | 0.0001 | 1299.88 | 1285.68 | 39341.11 | 8768.80 |
| 13 | ok | ok | 0.0001 | 0.0001 | 1317.88 | 1421.83 | 41012.63 | 8539.94 |
| 14 | ok | ok | 0.0001 | 0.0001 | 1335.77 | 1320.47 | 34593.13 | 8583.24 |
| 15 | ok | ok | 0.0001 | 0.0001 | 1254.86 | 1192.76 | 42958.13 | 8662.87 |
| 16 | ok | ok | 0.0001 | 0.0001 | 4058.97 | 468.79 | 37197.03 | 15326.18 |
| 17 | ok | ok | 0.0001 | 0.0001 | 3489.17 | 452.48 | 39495.51 | 15631.11 |
| 18 | ok | ok | 0.0001 | 0.0001 | 3456.71 | 445.79 | 40095.54 | 15930.83 |
| 19 | ok | ok | 0.0001 | 0.0001 | 2482.17 | 444.06 | 38964.37 | 15713.20 |
| 20 | ok | ok | 0.0001 | 0.0001 | 2074.48 | 412.45 | 39864.13 | 15817.04 |
| 21 | ok | ok | 0.0001 | 0.0001 | 2077.55 | 405.49 | 39582.91 | 15795.08 |
| 22 | ok | ok | 0.0001 | 0.0001 | 2068.09 | 412.96 | 40114.91 | 15863.33 |
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
| 1 | ok | ok | 0.0001 | 0.0001 | 20393.60 | 17639.97 | 44284.59 | 13638.64 |
| 2 | ok | ok | 0.0001 | 0.0001 | 20135.55 | 17549.87 | 44102.60 | 13590.21 |
| 3 | ok | ok | 0.0001 | 0.0001 | 11842.44 | 10218.95 | 44643.05 | 13693.85 |
| 4 | ok | ok | 0.0001 | 0.0001 | 10928.01 | 10217.39 | 44329.48 | 13662.60 |
| 5 | ok | ok | 0.0001 | 0.0001 | 6984.04 | 6650.42 | 40974.34 | 13483.00 |
| 6 | ok | ok | 0.0001 | 0.0001 | 3978.60 | 3433.41 | 37196.46 | 13557.13 |
| 7 | ok | ok | 0.0001 | 0.0001 | 3826.45 | 3419.70 | 41299.62 | 13546.20 |
| 8 | ok | ok | 0.0001 | 0.0001 | 2246.94 | 2048.92 | 45099.37 | 13452.77 |
| 9 | ok | ok | 0.0001 | 0.0001 | 2210.85 | 2035.03 | 51002.79 | 13513.51 |
| 10 | ok | ok | 0.0001 | 0.0001 | 2106.65 | 2011.83 | 43704.50 | 13653.29 |
| 11 | ok | ok | 0.0001 | 0.0001 | 2089.79 | 1992.62 | 44616.95 | 13634.05 |
| 12 | ok | ok | 0.0001 | 0.0001 | 1902.54 | 1829.90 | 47228.12 | 13664.52 |
| 13 | ok | ok | 0.0001 | 0.0001 | 284.89 | 261.81 | 46607.42 | 13647.11 |
| 14 | ok | ok | 0.0001 | 0.0001 | 211.21 | 182.44 | 44657.98 | 13556.01 |
| 15 | ok | ok | 0.0001 | 0.0001 | 118.21 | 98.83 | 46862.84 | 13625.98 |
| 16 | ok | ok | 0.0001 | 0.0001 | 5213.66 | 1423.21 | 41434.71 | 25605.07 |
| 17 | ok | ok | 0.0001 | 0.0001 | 4360.58 | 1334.57 | 39833.51 | 25579.37 |
| 18 | ok | ok | 0.0001 | 0.0001 | 4207.22 | 1327.38 | 44254.13 | 14354.67 |
| 19 | ok | ok | 0.0001 | 0.0001 | 2836.13 | 1297.91 | 42510.94 | 14999.83 |
| 20 | ok | ok | 0.0001 | 0.0001 | 2358.68 | 1222.44 | 43526.00 | 14918.33 |
| 21 | ok | ok | 0.0001 | 0.0001 | 2324.42 | 1212.59 | 42629.07 | 14452.92 |
| 22 | ok | ok | 0.0001 | 0.0001 | 2309.94 | 1201.20 | 43026.87 | 14704.82 |
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
| 1 | ok | ok | 0.0578 | 0.0578 | 1939.59 | 1729.51 | 5297.64 | 5334.47 |
| 2 | ok | ok | 0.0900 | 0.0900 | 1311.12 | 1073.25 | 3896.00 | 3768.39 |
| 3 | ok | ok | 0.1082 | 0.1082 | 932.59 | 903.88 | 3931.77 | 3808.76 |
| 4 | ok | ok | 0.1073 | 0.1073 | 910.81 | 956.11 | 3872.88 | 3811.29 |
| 5 | ok | ok | 0.0906 | 0.0983 | 328.73 | 351.49 | 3841.58 | 3836.65 |
| 6 | ok | ok | 0.0709 | 0.0830 | 289.65 | 272.43 | 4477.33 | 4490.79 |
| 7 | ok | ok | 0.0704 | 0.0828 | 248.27 | 250.42 | 4533.63 | 4604.29 |
| 8 | ok | ok | 0.0696 | 0.0711 | 192.91 | 199.83 | 4337.86 | 4454.44 |
| 9 | ok | ok | 0.0707 | 0.0723 | 190.90 | 176.34 | 4296.94 | 4451.33 |
| 10 | ok | ok | 0.0563 | 0.0601 | 168.97 | 157.91 | 5340.00 | 5488.89 |
| 11 | ok | ok | 0.0562 | 0.0600 | 123.38 | 116.17 | 5289.75 | 5395.64 |
| 12 | ok | ok | 0.0562 | 0.0600 | 119.10 | 106.90 | 5335.12 | 5470.49 |
| 13 | ok | ok | 0.0556 | 0.0586 | 115.57 | 105.79 | 5457.15 | 5579.58 |
| 14 | ok | ok | 0.0536 | 0.0569 | 101.60 | 84.31 | 5628.03 | 5692.94 |
| 15 | ok | ok | 0.0529 | 0.0562 | 81.51 | 67.08 | 5654.44 | 5757.57 |
| 16 | ok | ok | 0.0532 | 0.0532 | 7.05 | 6.26 | 4607.30 | 4939.61 |
| 17 | ok | ok | 0.0532 | 0.0532 | 6.55 | 5.86 | 4650.98 | 4910.18 |
| 18 | ok | ok | 0.0565 | 0.0565 | 5.62 | 5.08 | 4807.23 | 5167.37 |
| 19 | ok | ok | 0.0480 | 0.0480 | 3.18 | 2.74 | 5724.86 | 6298.91 |
| 20 | ok | ok | 0.0480 | 0.0480 | 3.07 | 2.73 | 5748.33 | 6357.12 |
| 21 | ok | ok | 0.0480 | 0.0480 | 3.28 | 2.84 | 5745.41 | 6375.40 |
| 22 | ok | ok | 0.0480 | 0.0480 | 3.24 | 2.81 | 5730.70 | 6318.47 |
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
| 3 | 1.52 | 1 | 1 | 0.0 | 18.0 | 73.4 | 6.5 | 2.1 |
| 4 | 0.46 | 1 | 1 | 0.0 | 63.4 | 6.0 | 18.2 | 12.4 |
| 5 | 1.17 | 1 | 1 | 0.0 | 88.8 | 2.4 | 6.7 | 2.1 |
| 6 | 1.49 | 1 | 1 | 0.0 | 91.8 | 1.8 | 5.4 | 1.0 |
| 7 | 1.61 | 1 | 1 | 0.0 | 92.7 | 1.8 | 4.5 | 0.9 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.08 | 1 | 1 | 2.5 | 2.1 | 7.0 | 88.0 | 0.3 |
| 4 | 1.08 | 1 | 1 | 2.4 | 2.0 | 7.0 | 88.3 | 0.3 |
| 5 | 0.92 | 1 | 1 | 2.7 | 2.2 | 6.9 | 87.7 | 0.4 |
| 6 | 0.89 | 1 | 1 | 2.7 | 2.2 | 7.0 | 87.7 | 0.3 |
| 7 | 0.89 | 1 | 1 | 2.8 | 2.3 | 7.0 | 87.6 | 0.3 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.1 | 22.4 | 0.0 | 55.5 |
| 4 | 22.0 | 22.4 | 0.0 | 55.6 |
| 5 | 22.4 | 22.9 | 0.0 | 54.7 |
| 6 | 22.6 | 22.8 | 0.0 | 54.6 |
| 7 | 22.2 | 23.0 | 0.0 | 54.8 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 4.8 | 0.0 | 1.7 | 5.7 | 11.9 | 68.8 |
| 6 | 5.6 | 0.0 | 3.2 | 6.5 | 13.2 | 65.4 |
| 7 | 5.9 | 0.0 | 4.1 | 6.4 | 13.1 | 65.3 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 14.4 | 35.2 | 0.0 | 2.5 | 5.3 | 42.5 |
| 6 | 12.8 | 31.8 | 17.6 | 2.3 | 4.7 | 30.8 |
| 7 | 10.6 | 26.4 | 29.0 | 1.8 | 3.9 | 28.3 |

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
| 1 | ok | ok | 0.1715 | 0.1716 | 788.96 | 780.02 | 2089.84 | 2223.33 |
| 2 | ok | ok | 0.1671 | 0.1671 | 761.09 | 654.31 | 1984.66 | 2120.17 |
| 3 | ok | ok | 0.1917 | 0.1917 | 520.04 | 527.42 | 1781.11 | 1983.22 |
| 4 | ok | ok | 0.2016 | 0.2016 | 468.29 | 495.49 | 1724.67 | 1886.60 |
| 5 | ok | ok | 0.1675 | 0.1679 | 225.56 | 246.96 | 2191.48 | 2383.26 |
| 6 | ok | ok | 0.1665 | 0.1669 | 182.93 | 192.86 | 2273.36 | 2445.39 |
| 7 | ok | ok | 0.1604 | 0.1609 | 161.23 | 172.32 | 2589.81 | 2693.43 |
| 8 | ok | ok | 0.1490 | 0.1506 | 132.61 | 140.29 | 2955.61 | 2927.56 |
| 9 | ok | ok | 0.1493 | 0.1505 | 129.24 | 140.69 | 3018.59 | 2986.92 |
| 10 | ok | ok | 0.1291 | 0.1342 | 105.56 | 108.39 | 3147.35 | 3195.35 |
| 11 | ok | ok | 0.1235 | 0.1298 | 81.04 | 81.71 | 3318.65 | 3355.94 |
| 12 | ok | ok | 0.1233 | 0.1297 | 74.98 | 75.07 | 3312.42 | 3385.92 |
| 13 | ok | ok | 0.1237 | 0.1300 | 44.36 | 50.03 | 3361.38 | 3364.38 |
| 14 | ok | ok | 0.1232 | 0.1295 | 33.26 | 29.15 | 3359.44 | 3365.55 |
| 15 | ok | ok | 0.1197 | 0.1270 | 23.23 | 26.44 | 3482.84 | 3474.21 |
| 16 | ok | ok | 0.1058 | 0.1058 | 4.70 | 4.70 | 3321.34 | 3697.57 |
| 17 | ok | ok | 0.1035 | 0.1035 | 4.53 | 4.47 | 3562.68 | 3859.35 |
| 18 | ok | ok | 0.1050 | 0.1050 | 2.97 | 3.41 | 3440.67 | 3748.31 |
| 19 | ok | ok | 0.1010 | 0.1010 | 2.54 | 2.49 | 3013.89 | 3398.70 |
| 20 | ok | ok | 0.1010 | 0.1010 | 2.59 | 2.50 | 3063.92 | 3364.02 |
| 21 | ok | ok | 0.1010 | 0.1010 | 2.45 | 2.45 | 3059.03 | 3425.56 |
| 22 | ok | ok | 0.1010 | 0.1010 | 2.53 | 2.34 | 3046.61 | 3420.07 |
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
| 3 | 0.77 | 1 | 1 | 0.0 | 70.6 | 6.3 | 19.5 | 3.6 |
| 4 | 0.87 | 1 | 1 | 0.0 | 70.6 | 4.6 | 18.9 | 5.9 |
| 5 | 1.78 | 1 | 1 | 0.0 | 89.1 | 2.1 | 7.3 | 1.6 |
| 6 | 2.27 | 1 | 1 | 0.0 | 92.3 | 1.7 | 5.4 | 0.6 |
| 7 | 2.78 | 1 | 1 | 0.0 | 92.2 | 1.7 | 5.4 | 0.8 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 2.24 | 1 | 1 | 1.8 | 1.0 | 7.1 | 90.0 | 0.2 |
| 4 | 2.17 | 1 | 1 | 1.6 | 0.9 | 7.2 | 90.1 | 0.1 |
| 5 | 1.57 | 1 | 1 | 2.6 | 1.5 | 7.0 | 88.6 | 0.3 |
| 6 | 1.48 | 1 | 1 | 2.3 | 1.4 | 7.0 | 88.3 | 0.9 |
| 7 | 1.22 | 1 | 1 | 3.1 | 1.7 | 7.0 | 88.0 | 0.2 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.4 | 22.6 | 0.0 | 55.0 |
| 4 | 22.2 | 22.7 | 0.0 | 55.1 |
| 5 | 22.4 | 22.7 | 0.0 | 54.9 |
| 6 | 22.6 | 22.8 | 0.0 | 54.6 |
| 7 | 22.5 | 22.8 | 0.0 | 54.7 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 6.0 | 0.0 | 2.2 | 6.2 | 11.3 | 66.6 |
| 6 | 5.6 | 0.0 | 3.6 | 6.3 | 11.8 | 66.3 |
| 7 | 6.7 | 0.0 | 4.2 | 6.2 | 12.1 | 65.5 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 15.9 | 40.7 | 0.0 | 2.7 | 5.7 | 35.1 |
| 6 | 13.0 | 33.0 | 16.4 | 2.1 | 4.5 | 30.9 |
| 7 | 11.1 | 28.1 | 26.2 | 1.9 | 3.7 | 29.0 |

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
| 1 | ok | ok | 0.3336 | 0.3336 | 8302.23 | 4411.88 | 38166.59 | 27543.17 |
| 2 | ok | ok | 0.3335 | 0.3335 | 7965.09 | 3991.23 | 36055.07 | 27369.43 |
| 3 | ok | ok | 0.3327 | 0.3327 | 3284.61 | 2885.60 | 36701.30 | 37780.40 |
| 4 | ok | ok | 0.3327 | 0.3327 | 3102.98 | 2878.22 | 34857.10 | 37083.36 |
| 5 | ok | ok | 0.3327 | 0.3327 | 1234.32 | 1288.64 | 36435.99 | 37713.61 |
| 6 | ok | ok | 0.3327 | 0.3327 | 1126.24 | 1104.60 | 35549.90 | 37914.69 |
| 7 | ok | ok | 0.3327 | 0.3327 | 1057.43 | 1089.28 | 35261.91 | 37241.78 |
| 8 | ok | ok | 0.3327 | 0.3327 | 862.46 | 878.26 | 34462.55 | 37487.19 |
| 9 | ok | ok | 0.3327 | 0.3327 | 807.87 | 867.43 | 35058.79 | 37572.47 |
| 10 | ok | ok | 0.3327 | 0.3327 | 640.38 | 689.88 | 33908.78 | 37096.07 |
| 11 | ok | ok | 0.3327 | 0.3327 | 504.22 | 586.95 | 33840.24 | 36945.81 |
| 12 | ok | ok | 0.3327 | 0.3327 | 434.76 | 436.84 | 30835.71 | 36112.74 |
| 13 | ok | ok | 0.3326 | 0.3326 | 254.57 | 270.84 | 34898.68 | 35591.84 |
| 14 | ok | ok | 0.3326 | 0.3326 | 190.07 | 209.45 | 33471.98 | 35056.40 |
| 15 | ok | ok | 0.3326 | 0.3326 | 196.56 | 196.07 | 33241.81 | 35676.98 |
| 16 | ok | ok | 0.3324 | 0.3328 | 54.77 | 64.86 | 35246.34 | 35883.91 |
| 17 | ok | ok | 0.3326 | 0.3328 | 50.41 | 58.09 | 35883.66 | 34742.48 |
| 18 | ok | ok | 0.3328 | 0.3327 | 39.92 | 48.91 | 36677.97 | 37680.31 |
| 19 | ok | ok | 0.3328 | 0.3327 | 36.88 | 46.38 | 35212.30 | 35809.91 |
| 20 | ok | ok | 0.3328 | 0.3327 | 35.86 | 44.60 | 35066.99 | 37674.76 |
| 21 | ok | ok | 0.3328 | 0.3327 | 35.99 | 45.23 | 38127.86 | 36829.18 |
| 22 | ok | ok | 0.3328 | 0.3327 | 34.14 | 45.13 | 36127.05 | 38039.97 |
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
| 13 | 1397971 | 1397990 | -0.00% | +0.20% |
| 14 | 1397974 | 1397993 | -0.00% | +0.20% |
| 15 | 1397973 | 1397997 | -0.00% | +0.20% |
| 16 | 1394019 | 1394165 | -0.01% | +0.00% |
| 17 | 1395021 | 1395370 | -0.03% | +0.00% |
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
| 1 | ok | ok | 0.0700 | 0.0700 | 1421.80 | 1492.25 | 3749.70 | 3709.33 |
| 2 | ok | ok | 0.0716 | 0.0716 | 1521.34 | 1492.44 | 3932.63 | 3784.53 |
| 3 | ok | ok | 0.0129 | 0.0131 | 3586.81 | 3296.58 | 11392.25 | 12584.91 |
| 4 | ok | ok | 0.0129 | 0.0131 | 3491.55 | 3292.18 | 11360.74 | 12673.11 |
| 5 | ok | ok | 0.0050 | 0.0050 | 2379.68 | 2187.94 | 26387.78 | 25337.64 |
| 6 | ok | ok | 0.0039 | 0.0039 | 2081.25 | 1907.49 | 29269.06 | 28408.15 |
| 7 | ok | ok | 0.0027 | 0.0028 | 2450.46 | 2239.18 | 28451.42 | 29500.27 |
| 8 | ok | ok | 0.0027 | 0.0028 | 1677.13 | 1557.13 | 29001.93 | 29643.91 |
| 9 | ok | ok | 0.0027 | 0.0028 | 1608.03 | 1531.26 | 29311.30 | 28841.70 |
| 10 | ok | ok | 0.0026 | 0.0027 | 1392.93 | 1285.52 | 28751.39 | 29336.47 |
| 11 | ok | ok | 0.0026 | 0.0027 | 1262.21 | 1127.22 | 31786.52 | 30235.03 |
| 12 | ok | ok | 0.0026 | 0.0027 | 1188.56 | 1015.81 | 29981.48 | 29371.27 |
| 13 | ok | ok | 0.0026 | 0.0027 | 737.09 | 736.30 | 30950.35 | 29837.97 |
| 14 | ok | ok | 0.0026 | 0.0026 | 651.79 | 627.14 | 28239.19 | 29413.93 |
| 15 | ok | ok | 0.0025 | 0.0026 | 650.56 | 535.69 | 29113.31 | 30218.31 |
| 16 | ok | ok | 0.0025 | 0.0025 | 77.79 | 62.38 | 27741.84 | 30747.94 |
| 17 | ok | ok | 0.0024 | 0.0024 | 74.87 | 60.03 | 27993.37 | 30063.89 |
| 18 | ok | ok | 0.0026 | 0.0026 | 71.99 | 57.63 | 29169.08 | 30975.59 |
| 19 | ok | ok | 0.0026 | 0.0026 | 37.24 | 29.59 | 28762.90 | 31799.38 |
| 20 | ok | ok | 0.0026 | 0.0026 | 37.47 | 30.03 | 29860.90 | 32110.38 |
| 21 | ok | ok | 0.0023 | 0.0023 | 25.21 | 18.21 | 29915.64 | 31544.14 |
| 22 | ok | ok | 0.0022 | 0.0022 | 8.37 | 5.29 | 30698.13 | 31640.09 |
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
| 1 | ok | ok | 0.2001 | 0.2001 | 645.57 | 659.36 | 1963.84 | 2082.96 |
| 2 | ok | ok | 0.1380 | 0.1380 | 657.19 | 621.26 | 1902.51 | 1889.58 |
| 3 | ok | ok | 0.1518 | 0.1518 | 526.60 | 549.04 | 1981.14 | 1921.78 |
| 4 | ok | ok | 0.1514 | 0.1514 | 532.48 | 549.31 | 2113.98 | 2001.84 |
| 5 | ok | ok | 0.1476 | 0.1499 | 241.65 | 258.05 | 1819.86 | 1775.96 |
| 6 | ok | ok | 0.1349 | 0.1384 | 146.69 | 151.85 | 2252.35 | 2124.62 |
| 7 | ok | ok | 0.1391 | 0.1421 | 125.66 | 132.25 | 2166.60 | 2127.74 |
| 8 | ok | ok | 0.1269 | 0.1295 | 95.20 | 97.41 | 2133.11 | 2122.27 |
| 9 | ok | ok | 0.1272 | 0.1297 | 94.06 | 95.11 | 2135.08 | 2113.01 |
| 10 | ok | ok | 0.1276 | 0.1310 | 69.15 | 70.75 | 2085.81 | 2102.76 |
| 11 | ok | ok | 0.1349 | 0.1400 | 50.95 | 48.86 | 2274.22 | 2264.83 |
| 12 | ok | ok | 0.1341 | 0.1396 | 49.90 | 48.18 | 2317.93 | 2341.60 |
| 13 | ok | ok | 0.1448 | 0.1524 | 33.13 | 33.84 | 2543.34 | 2557.28 |
| 14 | ok | ok | 0.1456 | 0.1539 | 28.03 | 28.48 | 2616.44 | 2622.14 |
| 15 | ok | ok | 0.1367 | 0.1474 | 21.65 | 20.83 | 2789.23 | 2771.83 |
| 16 | ok | ok | 0.1172 | 0.1172 | 5.14 | 5.50 | 1493.41 | 1705.96 |
| 17 | ok | ok | 0.1111 | 0.1111 | 4.57 | 4.78 | 1543.24 | 1711.30 |
| 18 | ok | ok | 0.0896 | 0.0897 | 2.63 | 2.94 | 1068.33 | 1225.23 |
| 19 | ok | ok | 0.1099 | 0.1099 | 2.12 | 2.20 | 1528.67 | 1697.27 |
| 20 | ok | ok | 0.1099 | 0.1099 | 2.06 | 2.16 | 1514.50 | 1697.33 |
| 21 | ok | ok | 0.1099 | 0.1099 | 2.04 | 2.23 | 1523.08 | 1662.48 |
| 22 | ok | ok | 0.1094 | 0.1094 | 1.19 | 1.17 | 1382.40 | 1612.49 |
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
| 16 | 491471 | 491471 | +0.00% | +0.00% |
| 17 | 466151 | 466160 | -0.00% | +0.00% |
| 18 | 375877 | 376433 | -0.15% | +0.00% |
| 19 | 460820 | 460787 | +0.01% | +0.00% |
| 20 | 460817 | 460784 | +0.01% | +0.00% |
| 21 | 460817 | 460784 | +0.01% | +0.00% |
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
| 1 | ok | ok | 0.1379 | 0.1379 | 1494.04 | 1447.75 | 4287.89 | 4523.80 |
| 2 | ok | ok | 0.1379 | 0.1379 | 1573.11 | 1096.94 | 4267.37 | 4517.63 |
| 3 | ok | ok | 0.1337 | 0.1337 | 1203.70 | 1294.23 | 5624.24 | 5309.39 |
| 4 | ok | ok | 0.1337 | 0.1337 | 1232.65 | 1296.43 | 5652.98 | 5277.30 |
| 5 | ok | ok | 0.1337 | 0.1337 | 360.52 | 371.37 | 5605.80 | 5301.27 |
| 6 | ok | ok | 0.1333 | 0.1333 | 275.93 | 271.17 | 6132.72 | 5819.06 |
| 7 | ok | ok | 0.1333 | 0.1333 | 239.92 | 242.94 | 6136.52 | 5685.50 |
| 8 | ok | ok | 0.1361 | 0.1361 | 168.56 | 173.27 | 4835.38 | 4763.43 |
| 9 | ok | ok | 0.1361 | 0.1361 | 154.75 | 159.87 | 4888.39 | 4766.74 |
| 10 | ok | ok | 0.1361 | 0.1361 | 127.92 | 115.55 | 4969.72 | 4758.09 |
| 11 | ok | ok | 0.1361 | 0.1361 | 102.03 | 97.25 | 4978.80 | 4881.84 |
| 12 | ok | ok | 0.1361 | 0.1361 | 97.34 | 88.94 | 4992.25 | 4866.18 |
| 13 | ok | ok | 0.1358 | 0.1358 | 95.51 | 85.04 | 4935.95 | 4815.65 |
| 14 | ok | ok | 0.1358 | 0.1358 | 70.15 | 68.82 | 4953.13 | 4820.84 |
| 15 | ok | ok | 0.1358 | 0.1358 | 67.45 | 57.02 | 4922.31 | 4834.38 |
| 16 | ok | ok | 0.1245 | 0.1245 | 5.73 | 5.70 | 3118.40 | 3253.95 |
| 17 | ok | ok | 0.1245 | 0.1245 | 5.65 | 5.46 | 3103.21 | 3243.79 |
| 18 | ok | ok | 0.1242 | 0.1242 | 4.42 | 4.21 | 3053.20 | 3148.08 |
| 19 | ok | ok | 0.1242 | 0.1242 | 4.27 | 4.09 | 2987.30 | 3022.52 |
| 20 | ok | ok | 0.1242 | 0.1242 | 4.33 | 4.16 | 3021.52 | 3134.99 |
| 21 | ok | ok | 0.1242 | 0.1242 | 4.60 | 4.12 | 2980.68 | 3098.31 |
| 22 | ok | ok | 0.1242 | 0.1242 | 4.65 | 4.00 | 3065.25 | 3103.12 |
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
| 3 | 0.37 | 1 | 1 | 0.1 | 60.3 | 4.1 | 27.8 | 7.7 |
| 4 | 0.41 | 1 | 1 | 0.0 | 61.4 | 3.6 | 21.3 | 13.7 |
| 5 | 1.16 | 1 | 1 | 0.0 | 85.2 | 4.7 | 7.4 | 2.7 |
| 6 | 1.41 | 1 | 1 | 0.0 | 92.1 | 1.1 | 5.8 | 1.1 |
| 7 | 1.63 | 1 | 1 | 0.0 | 93.2 | 0.9 | 4.9 | 0.9 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.13 | 1 | 1 | 0.0 | 2.2 | 7.3 | 90.1 | 0.3 |
| 4 | 1.13 | 1 | 1 | 0.0 | 5.4 | 7.0 | 87.3 | 0.3 |
| 5 | 1.19 | 1 | 1 | 0.0 | 2.3 | 9.2 | 88.0 | 0.4 |
| 6 | 0.95 | 1 | 1 | 0.0 | 2.7 | 7.2 | 89.6 | 0.4 |
| 7 | 0.94 | 1 | 1 | 0.0 | 2.7 | 7.2 | 89.7 | 0.3 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 22.5 | 22.9 | 0.0 | 54.6 |
| 4 | 21.9 | 22.5 | 0.0 | 55.5 |
| 5 | 22.5 | 22.8 | 0.0 | 54.8 |
| 6 | 21.2 | 25.2 | 0.0 | 53.6 |
| 7 | 22.4 | 22.8 | 0.0 | 54.8 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 5.3 | 0.0 | 0.9 | 6.1 | 13.0 | 66.3 |
| 6 | 6.1 | 0.0 | 2.4 | 6.3 | 13.0 | 64.8 |
| 7 | 6.0 | 0.0 | 3.8 | 6.2 | 12.6 | 65.3 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 5 | 17.3 | 40.6 | 0.0 | 2.0 | 4.1 | 36.0 |
| 6 | 14.8 | 35.9 | 12.1 | 1.5 | 3.3 | 32.4 |
| 7 | 13.1 | 31.2 | 21.3 | 1.3 | 2.8 | 30.3 |

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
| 1 | ok | ok | 1.0000 | 1.0000 | 11699.27 | 7220.22 | 35244.55 | 33985.84 |
| 2 | ok | ok | 1.0000 | 1.0000 | 11282.48 | 8037.33 | 40960.71 | 31472.83 |
| 3 | ok | ok | 1.0000 | 1.0000 | 9484.44 | 7468.10 | 47176.93 | 30611.58 |
| 4 | ok | ok | 1.0000 | 1.0000 | 9206.56 | 7689.67 | 43517.31 | 29900.95 |
| 5 | ok | ok | 1.0000 | 1.0000 | 4333.77 | 4481.17 | 46796.38 | 30693.27 |
| 6 | ok | ok | 1.0000 | 1.0000 | 4270.65 | 4457.29 | 44990.54 | 34410.91 |
| 7 | ok | ok | 1.0000 | 1.0000 | 4072.10 | 4329.41 | 46087.38 | 37388.64 |
| 8 | ok | ok | 1.0000 | 1.0000 | 3851.46 | 4403.52 | 42615.92 | 31614.70 |
| 9 | ok | ok | 1.0000 | 1.0000 | 3632.04 | 3822.10 | 48280.91 | 33834.59 |
| 10 | ok | ok | 1.0000 | 1.0000 | 2905.73 | 2674.53 | 47689.59 | 35506.78 |
| 11 | ok | ok | 1.0000 | 1.0000 | 2617.66 | 2497.07 | 48013.20 | 32388.66 |
| 12 | ok | ok | 1.0000 | 1.0000 | 1977.69 | 1723.62 | 42253.89 | 31387.94 |
| 13 | ok | ok | 1.0000 | 1.0000 | 174.14 | 244.35 | 39380.10 | 34796.79 |
| 14 | ok | ok | 1.0000 | 1.0000 | 113.54 | 208.13 | 46002.29 | 32128.51 |
| 15 | ok | ok | 1.0000 | 1.0000 | 113.48 | 179.65 | 47162.67 | 29765.71 |
| 16 | ok | ok | 1.0000 | 1.0000 | 18.85 | 18.64 | 39489.19 | 31579.60 |
| 17 | ok | ok | 1.0000 | 1.0000 | 18.33 | 19.98 | 46983.15 | 32726.11 |
| 18 | ok | ok | 1.0000 | 1.0000 | 13.98 | 17.05 | 47369.34 | 34042.55 |
| 19 | ok | ok | 1.0000 | 1.0000 | 13.84 | 16.98 | 45870.89 | 32841.57 |
| 20 | ok | ok | 1.0000 | 1.0000 | 13.33 | 17.42 | 46611.92 | 33668.71 |
| 21 | ok | ok | 1.0000 | 1.0000 | 12.92 | 16.21 | 44621.27 | 30780.33 |
| 22 | ok | ok | 1.0000 | 1.0000 | 13.72 | 17.34 | 44126.86 | 32270.26 |
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
| 1 | ok | ok | 0.0148 | 0.0148 | 4101.50 | 3617.04 | 9617.64 | 10325.96 |
| 2 | ok | ok | 0.0148 | 0.0148 | 3979.92 | 3600.30 | 9625.65 | 10512.48 |
| 3 | ok | ok | 0.0138 | 0.0187 | 2545.62 | 2639.70 | 8618.24 | 9154.13 |
| 4 | ok | ok | 0.0334 | 0.0334 | 653.42 | 854.20 | 5783.32 | 5711.32 |
| 5 | ok | ok | 0.0315 | 0.0315 | 545.64 | 701.84 | 7230.42 | 7308.86 |
| 6 | ok | ok | 0.0289 | 0.0289 | 409.86 | 509.34 | 8360.42 | 8125.44 |
| 7 | ok | ok | 0.0289 | 0.0289 | 406.28 | 489.14 | 8567.80 | 8292.22 |
| 8 | ok | ok | 0.0289 | 0.0289 | 405.36 | 489.02 | 8461.62 | 8234.69 |
| 9 | ok | ok | 0.0139 | 0.0139 | 300.29 | 450.60 | 10491.11 | 11769.88 |
| 10 | ok | ok | 0.0139 | 0.0139 | 298.17 | 444.56 | 10344.07 | 11692.11 |
| 11 | ok | ok | 0.0124 | 0.0124 | 28.81 | 24.31 | 13984.69 | 15173.20 |
| 12 | ok | ok | 0.0125 | 0.0125 | 26.36 | 24.03 | 13069.09 | 14374.85 |
| 13 | ok | ok | 0.0125 | 0.0125 | 26.37 | 22.88 | 12989.68 | 14152.84 |
| 14 | ok | ok | 0.0124 | 0.0124 | 26.17 | 22.91 | 13479.64 | 14545.45 |
| 15 | ok | ok | 0.0127 | 0.0127 | 4.58 | 4.63 | 11795.32 | 13159.84 |
| 16 | ok | ok | 0.0125 | 0.0125 | 20.27 | 17.56 | 10108.56 | 11144.08 |
| 17 | ok | ok | 0.0126 | 0.0126 | 20.12 | 18.19 | 9747.94 | 10967.96 |
| 18 | ok | ok | 0.0127 | 0.0127 | 4.02 | 3.54 | 8380.41 | 11402.51 |
| 19 | ok | ok | 0.0127 | 0.0127 | 3.97 | 2.99 | 8951.64 | 10222.60 |
| 20 | ok | ok | 0.0127 | 0.0127 | 2.82 | 3.31 | 8307.36 | 9145.64 |
| 21 | ok | ok | 0.0127 | 0.0127 | 5.60 | 4.46 | 11795.11 | 13124.20 |
| 22 | ok | ok | 0.0127 | 0.0127 | 1.54 | 1.57 | 8257.68 | 12569.83 |

## trained-dictionary

Structured multi-endpoint records aligned with the trained dictionary fixture.

- Input bytes: 4194304
- Dictionary mode: trained
- Dictionary bytes: 512 (1 per 8192 bytes of input)
- Timing trial budget: 60 ms

| Level | Rust encode | Rust decode upstream | Rust ratio | zstd ratio | Rust enc MiB/s | zstd enc MiB/s | Rust dec MiB/s | zstd dec MiB/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | ok | ok | 0.0456 | 0.0456 | 1029.73 | 1099.98 | 2560.01 | 2958.96 |
| 2 | ok | ok | 0.0528 | 0.0528 | 1372.96 | 1013.24 | 2683.58 | 2845.70 |
| 3 | ok | ok | 0.0582 | 0.0587 | 300.30 | 867.25 | 1145.55 | 2991.52 |
| 4 | ok | ok | 0.0643 | 0.0643 | 352.77 | 607.00 | 1876.25 | 2519.95 |
| 5 | ok | ok | 0.0614 | 0.0614 | 244.59 | 338.28 | 2069.56 | 2109.32 |
| 6 | ok | ok | 0.0532 | 0.0532 | 88.63 | 225.58 | 1737.00 | 2479.51 |
| 7 | ok | ok | 0.0532 | 0.0532 | 130.33 | 232.13 | 2492.10 | 2573.11 |
| 8 | ok | ok | 0.0532 | 0.0532 | 150.31 | 240.36 | 2244.42 | 2928.90 |
| 9 | ok | ok | 0.0528 | 0.0528 | 47.16 | 159.74 | 2979.37 | 3204.17 |
| 10 | ok | ok | 0.0528 | 0.0528 | 46.63 | 156.01 | 2911.72 | 3015.74 |
| 11 | ok | ok | 0.0458 | 0.0467 | 8.88 | 24.31 | 4025.60 | 3838.33 |
| 12 | ok | ok | 0.0431 | 0.0439 | 12.50 | 21.17 | 3246.32 | 3617.13 |
| 13 | ok | ok | 0.0436 | 0.0442 | 10.37 | 18.20 | 4178.38 | 4096.96 |
| 14 | ok | ok | 0.0384 | 0.0385 | 5.71 | 4.64 | 2563.51 | 3015.59 |
| 15 | ok | ok | 0.0393 | 0.0442 | 3.95 | 3.65 | 3734.67 | 3422.27 |
| 16 | ok | ok | 0.0432 | 0.0432 | 9.66 | 10.13 | 3585.00 | 3818.75 |
| 17 | ok | ok | 0.0393 | 0.0441 | 5.56 | 5.27 | 3466.01 | 3504.06 |
| 18 | ok | ok | 0.0393 | 0.0441 | 4.54 | 4.93 | 3430.81 | 3372.40 |
| 19 | ok | ok | 0.0393 | 0.0441 | 5.18 | 5.07 | 3424.49 | 3681.26 |
| 20 | ok | ok | 0.0393 | 0.0441 | 4.62 | 5.89 | 3456.08 | 3649.95 |
| 21 | ok | ok | 0.0393 | 0.0441 | 8.43 | 5.46 | 3449.92 | 3682.78 |
| 22 | ok | ok | 0.0393 | 0.0441 | 8.28 | 7.45 | 3416.63 | 3590.73 |
### Rust First-Block Stage Timing

- Samples the first raw `block_size` chunk only, so the timing breakdown stays aligned with the real block-local hot path.
- Uses prepared dictionaries for dictionary-backed cases so the sample reflects encoder hot paths instead of repeated dictionary parsing.
- The stage table above is sampled with the planner's phase timers off, so its milliseconds and its shares are both the real encoder's. The two sub-breakdown tables below need those timers and are sampled separately, because a timer taken per lazy parser step costs far more than the step: with them on, this case's first block reads up to 18x its real time and 99% of the frame lands in `Plan`. Read the sub-breakdowns as shares of their own row and never against the table above.
- The planning sub-breakdown covers row and chain/extdict lazy paths; other planner families may still report zeros. The lazy parser phase sub-breakdown is instrumented for no-dict row and trained-dictionary chain/extdict cases, and likewise reports zeros elsewhere.
- Sampled on levels 3-7 over 3 iterations.

| Level | Sampled ms | Blocks | Compressed | Split % | Plan % | Lit % | Seq % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.60 | 1 | 1 | 0.0 | 69.4 | 4.5 | 22.2 | 3.8 |
| 4 | 0.83 | 1 | 1 | 0.0 | 82.8 | 2.8 | 13.7 | 0.7 |
| 5 | 1.18 | 1 | 1 | 0.0 | 88.7 | 2.1 | 8.8 | 0.5 |
| 6 | 1.50 | 1 | 1 | 0.0 | 92.3 | 1.9 | 5.5 | 0.3 |
| 7 | 1.52 | 1 | 1 | 0.0 | 92.2 | 1.9 | 5.6 | 0.3 |

### Rust First-Block Decode Timing

- Profiles Rust decode against the same upstream-produced frame family used by the decode throughput benchmark.
- Uses prepared dictionaries for dictionary-backed cases so decode attribution stays on block decode instead of dictionary parsing.
- Read these as proportions, not costs. Timing each stage separately requires decoding sequence commands into a buffer and then executing them, where the real decoder fuses the two into one pass and runs several times faster. The MiB/s column above is the real path; this table is not.
- Sampled on levels 3-7 over 3 iterations, and only on the first block. Rows in the decode column are whole frames, so this cannot by itself explain one.

| Level | Sampled ms | Blocks | Compressed | Lit % | SeqTable % | SeqCmd % | Exec % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1.35 | 1 | 1 | 1.3 | 1.7 | 7.2 | 89.2 | 0.5 |
| 4 | 1.42 | 1 | 1 | 1.6 | 1.8 | 6.9 | 89.1 | 0.7 |
| 5 | 1.27 | 1 | 1 | 1.7 | 2.0 | 7.0 | 88.6 | 0.7 |
| 6 | 1.49 | 1 | 1 | 1.8 | 1.7 | 5.1 | 90.6 | 0.9 |
| 7 | 1.07 | 1 | 1 | 2.2 | 2.2 | 7.0 | 87.9 | 0.8 |

| Level | LitCopy % of exec | PrefixMatch % | DictMatch % | Exec Other % |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 23.0 | 21.0 | 0.2 | 55.8 |
| 4 | 23.0 | 21.3 | 0.1 | 55.6 |
| 5 | 18.1 | 16.6 | 0.1 | 65.1 |
| 6 | 22.7 | 21.4 | 0.2 | 55.7 |
| 7 | 22.5 | 21.2 | 0.2 | 56.1 |

| Level | Row % of plan | Chain % of plan | Match % | Rep % | Insert % | Parser % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 0.0 | 9.7 | 1.2 | 10.4 | 5.4 | 64.6 |
| 5 | 0.0 | 19.7 | 4.1 | 8.9 | 6.7 | 55.1 |
| 6 | 0.0 | 24.1 | 6.4 | 8.2 | 7.1 | 50.0 |
| 7 | 0.0 | 23.6 | 6.4 | 8.3 | 6.8 | 50.5 |

| Level | Base Rep % of parser | Base Reg % | Continue % | Store % | Rep2 % | Other % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 4 | 18.8 | 15.8 | 0.0 | 5.8 | 14.2 | 45.4 |
| 5 | 14.1 | 15.3 | 15.8 | 4.4 | 9.9 | 40.6 |
| 6 | 11.8 | 13.0 | 26.7 | 3.5 | 7.0 | 37.9 |
| 7 | 11.9 | 13.5 | 26.3 | 3.5 | 7.0 | 37.7 |

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


