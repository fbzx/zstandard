// Shared support module: several targets `#[path]`-include this same file and
// each uses a different subset, so the rest reads as dead in every one of
// them. Deleting is not an option — what one target drops, another needs.
#![allow(dead_code)]

#[path = "../support/corpora.rs"]
#[allow(dead_code)]
mod corpora;
#[path = "../support/upstream_zstd.rs"]
#[allow(dead_code)]
mod upstream_zstd;

use corpora::{DictKind, benchmark_report_cases};
use upstream_zstd::{
    UpstreamSequenceSource, UpstreamSequenceTrace, compress_once, emit_raw_dictionary,
    emit_trained_dictionary, require_helper, trace_trained_dict_sequences,
};
use zstandard::{
    BlockTraceDecision, BlockTraceMode, BlockType, CompressionLevel, EncoderDictionary,
    EncoderOptions, FrameHeader, encode_all_with_options,
    encode_all_with_prepared_dict_and_options, parse_block_header, parse_frame_header,
    trace_first_block_with_options, trace_first_block_with_prepared_dict_and_options,
};

const INPUT_BYTES: usize = 512 * 1024;
const BLOCK_SIZE: usize = 128 * 1024;

struct Target {
    case: &'static str,
    level: u8,
}

const TARGETS: &[Target] = &[
    Target {
        case: "log-lines",
        level: 1,
    },
    Target {
        case: "log-lines",
        level: 2,
    },
    Target {
        case: "log-lines",
        level: 3,
    },
    Target {
        case: "json-records",
        level: 5,
    },
    Target {
        case: "json-records",
        level: 6,
    },
    Target {
        case: "json-records",
        level: 7,
    },
    Target {
        case: "log-lines",
        level: 5,
    },
    Target {
        case: "log-lines",
        level: 6,
    },
    Target {
        case: "log-lines",
        level: 7,
    },
    Target {
        case: "log-lines",
        level: 8,
    },
    Target {
        case: "log-lines",
        level: 9,
    },
    Target {
        case: "mixed-entropy",
        level: 2,
    },
    Target {
        case: "mixed-entropy",
        level: 3,
    },
    Target {
        case: "mixed-entropy",
        level: 4,
    },
    Target {
        case: "raw-dictionary",
        level: 3,
    },
    Target {
        case: "trained-dictionary",
        level: 5,
    },
    Target {
        case: "trained-dictionary",
        level: 6,
    },
    Target {
        case: "trained-dictionary",
        level: 7,
    },
    Target {
        case: "trained-dictionary",
        level: 9,
    },
];

fn main() {
    let helper = require_helper("trace_bad_blocks");
    let raw_dictionary = emit_raw_dictionary(helper);
    let trained_dictionary = emit_trained_dictionary(helper);
    let raw_prepared = EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
    let trained_prepared =
        EncoderDictionary::new(&trained_dictionary).expect("trained dictionary must parse");
    let cases = benchmark_report_cases(INPUT_BYTES);

    for target in TARGETS {
        let case = cases
            .iter()
            .find(|case| case.name == target.case)
            .unwrap_or_else(|| panic!("missing benchmark case {}", target.case));
        match trace_target_line(case, target.level, helper, &raw_prepared, &trained_prepared) {
            Ok(line) => println!("{line}"),
            Err(error) => println!("{} L{}: error={error}", target.case, target.level),
        }
    }
}

fn trace_target_line(
    case: &corpora::CorpusCase,
    level: u8,
    helper: &std::path::Path,
    raw_prepared: &EncoderDictionary<'_>,
    trained_prepared: &EncoderDictionary<'_>,
) -> std::result::Result<String, String> {
    let options = EncoderOptions {
        block_size: BLOCK_SIZE,
        checksum: false,
        write_dict_id: true,
        compression_level: CompressionLevel::try_new(i32::from(level))
            .expect("benchmark target level must be supported"),
        ..Default::default()
    };
    let trace = match case.dict_kind {
        DictKind::None => trace_first_block_with_options(&case.input, options),
        DictKind::Raw => {
            trace_first_block_with_prepared_dict_and_options(&case.input, raw_prepared, options)
        }
        DictKind::Trained => {
            trace_first_block_with_prepared_dict_and_options(&case.input, trained_prepared, options)
        }
    }
    .map_err(|error| format!("trace_failed:{error:?}"))?;
    let upstream_sequences = if matches!(case.dict_kind, DictKind::Trained) {
        trace_trained_dict_sequences(
            helper,
            i32::from(level),
            options.checksum,
            4,
            &case.input[..case.input.len().min(BLOCK_SIZE)],
        )
    } else {
        Vec::new()
    };
    let actual_frame = rust_encoded_frame(case, options, raw_prepared, trained_prepared);
    let upstream_frame = upstream_encoded_frame(helper, case, options)?;
    let actual_blocks = parse_compressed_block_stats(&actual_frame);
    let upstream_blocks = parse_compressed_block_stats(&upstream_frame);
    let actual_block = *actual_blocks
        .first()
        .ok_or_else(|| "actual frame missing first block".to_string())?;
    let upstream_block = *upstream_blocks
        .first()
        .ok_or_else(|| "upstream frame missing first block".to_string())?;
    let first_divergent_block = first_block_stats_mismatch(&actual_blocks, &upstream_blocks);

    Ok(format!(
        "{} L{}: actual_block={} actual_literals={} actual_seqs={} actual_modes={} upstream_block={} upstream_literals={} upstream_seqs={} upstream_modes={} first_divergent_block={} planned_decision={} planned_candidate={} raw={} cparams=w{} c{} h{} s{} m{} t{} {} row={} dict_mode={} prepared={} chain={} row_hash_log={} dict_tables={} parser={} lit_bytes={} match_bytes={} avg_match={} seqs={} repcodes={} regular_sources={} rep_sources={} explicit_avg={} first_source={} row_contest={} row_emit={} row_emit2={} row_emit3={} row_emit4={} row_accept={} upstream_emit={} of_codes={} planned_literals={} planned_seq_header={} planned_last_count={} planned_bitstream={} planned_modes={} long_offsets={}",
        case.name,
        level,
        format_candidate(Some(actual_block.payload_size)),
        actual_block.literal_section_size,
        actual_block.sequence_count,
        format_upstream_modes(actual_block.modes),
        format_candidate(Some(upstream_block.payload_size)),
        upstream_block.literal_section_size,
        upstream_block.sequence_count,
        format_upstream_modes(upstream_block.modes),
        format_first_divergent_block(first_divergent_block),
        format_decision(trace.decision),
        format_candidate(trace.candidate_compressed_size),
        trace.raw_size,
        trace.compression_parameters.window_log,
        trace.compression_parameters.chain_log,
        trace.compression_parameters.hash_log,
        trace.compression_parameters.search_log,
        trace.compression_parameters.min_match,
        trace.compression_parameters.target_length,
        format_upstream_strategy(trace.compression_parameters.strategy),
        trace.compression_parameters.use_row_match_finder,
        format_dictionary_mode(trace.compression_parameters.dictionary_mode),
        trace.compression_parameters.prepared_match_state,
        trace.compression_parameters.chain_table_allocated,
        format_row_hash_log(trace.compression_parameters.row_hash_log),
        format_dictionary_table_source(trace.compression_parameters.dict_table_source),
        format_parser_strategy(trace.compression_parameters.parser_strategy),
        trace.parser_stats.literal_bytes,
        trace.parser_stats.matched_bytes,
        format_average_match(trace),
        trace.sequence_count,
        format_repcodes(trace.parser_stats.repcodes),
        format_regular_match_sources(trace.parser_stats.regular_match_sources),
        format_rep_match_sources(trace.parser_stats.rep_match_sources),
        format_average_explicit_offset(trace.parser_stats),
        format_first_match_source(trace.parser_stats.first_match_source),
        format_row_search_contest(trace.parser_stats.first_row_search_contest),
        format_emitted_match(trace.parser_stats.first_emitted_match),
        format_emitted_match(trace.parser_stats.second_emitted_match),
        format_emitted_match(trace.parser_stats.third_emitted_match),
        format_emitted_match(trace.parser_stats.fourth_emitted_match),
        format_accepted_regular_match(trace.parser_stats.first_accepted_regular_match),
        format_upstream_sequences(&upstream_sequences),
        format_offset_codes(&trace.parser_stats.offset_code_counts),
        trace.literal_section_size,
        trace.sequence_header_size,
        trace.last_count_size,
        trace.sequence_bitstream_size,
        format_modes(trace.sequence_modes),
        trace.long_offsets,
    ))
}

#[derive(Debug, Clone, Copy)]
struct EncodedBlockStats {
    block_type: BlockType,
    last_block: bool,
    payload_size: usize,
    literal_section_size: usize,
    sequence_section_size: usize,
    sequence_count: usize,
    modes: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, Copy)]
struct BlockStatsMismatch {
    block_index: usize,
    actual: EncodedBlockStats,
    upstream: EncodedBlockStats,
}

fn upstream_encoded_frame(
    helper: &std::path::Path,
    case: &corpora::CorpusCase,
    options: EncoderOptions,
) -> std::result::Result<Vec<u8>, String> {
    let mode = match case.dict_kind {
        DictKind::None => "compress-regular-configured",
        DictKind::Raw => "compress-raw-dict-configured",
        DictKind::Trained => "compress-trained-dict-configured",
    };
    std::panic::catch_unwind(|| {
        compress_once(
            helper,
            mode,
            options.compression_level.as_i32(),
            options.checksum,
            &case.input,
        )
    })
    .map_err(format_panic_payload)
}

fn format_panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn rust_encoded_frame(
    case: &corpora::CorpusCase,
    options: EncoderOptions,
    raw_prepared: &EncoderDictionary<'_>,
    trained_prepared: &EncoderDictionary<'_>,
) -> Vec<u8> {
    match case.dict_kind {
        DictKind::None => encode_all_with_options(&case.input, options),
        DictKind::Raw => {
            encode_all_with_prepared_dict_and_options(&case.input, raw_prepared, options)
        }
        DictKind::Trained => {
            encode_all_with_prepared_dict_and_options(&case.input, trained_prepared, options)
        }
    }
    .unwrap_or_else(|error| {
        panic!(
            "failed to encode {} L{} for actual first-block stats: {error:?}",
            case.name,
            options.compression_level.as_i32(),
        )
    })
}

fn parse_compressed_block_stats(frame: &[u8]) -> Vec<EncodedBlockStats> {
    let header = match parse_frame_header(frame).expect("frame header should parse") {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let mut block_header_offset = header.header_size;
    let mut blocks = Vec::new();

    loop {
        let block =
            parse_block_header(&frame[block_header_offset..]).expect("block header should parse");
        let payload_start = block_header_offset + 3;
        let payload_end = payload_start + block_payload_size(block.block_type, block.block_size);
        let payload = &frame[payload_start..payload_end];
        let (literal_section_size, sequence_section_size, sequence_count, modes) =
            if block.block_type == BlockType::Compressed {
                let literals = parse_literals_header(payload);
                let literal_section_size = literals.payload_end();
                let sequence_section_size = payload.len().saturating_sub(literal_section_size);
                (
                    literal_section_size,
                    sequence_section_size,
                    compressed_block_sequence_count(payload),
                    compressed_block_sequence_modes(payload),
                )
            } else {
                (0, 0, 0, None)
            };

        blocks.push(EncodedBlockStats {
            block_type: block.block_type,
            last_block: block.last_block,
            payload_size: payload.len(),
            literal_section_size,
            sequence_section_size,
            sequence_count,
            modes,
        });

        if block.last_block {
            break;
        }
        block_header_offset = payload_end;
    }

    blocks
}

fn first_block_stats_mismatch(
    actual: &[EncodedBlockStats],
    upstream: &[EncodedBlockStats],
) -> Option<BlockStatsMismatch> {
    let mismatch_index = actual
        .iter()
        .zip(upstream.iter())
        .position(|(actual, upstream)| !block_stats_match(*actual, *upstream))
        .or_else(|| (actual.len() != upstream.len()).then_some(actual.len().min(upstream.len())))?;

    Some(BlockStatsMismatch {
        block_index: mismatch_index,
        actual: actual
            .get(mismatch_index)
            .copied()
            .unwrap_or(EncodedBlockStats {
                block_type: BlockType::Raw,
                last_block: false,
                payload_size: 0,
                literal_section_size: 0,
                sequence_section_size: 0,
                sequence_count: 0,
                modes: None,
            }),
        upstream: upstream
            .get(mismatch_index)
            .copied()
            .unwrap_or(EncodedBlockStats {
                block_type: BlockType::Raw,
                last_block: false,
                payload_size: 0,
                literal_section_size: 0,
                sequence_section_size: 0,
                sequence_count: 0,
                modes: None,
            }),
    })
}

fn block_stats_match(actual: EncodedBlockStats, upstream: EncodedBlockStats) -> bool {
    actual.block_type == upstream.block_type
        && actual.last_block == upstream.last_block
        && actual.payload_size == upstream.payload_size
        && actual.literal_section_size == upstream.literal_section_size
        && actual.sequence_section_size == upstream.sequence_section_size
        && actual.sequence_count == upstream.sequence_count
        && actual.modes == upstream.modes
}

fn format_candidate(candidate: Option<usize>) -> String {
    candidate
        .map(|size| size.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn format_modes(modes: Option<zstandard::BlockTraceSequenceModes>) -> String {
    match modes {
        Some(modes) => format!(
            "{}/{}/{}",
            format_mode(modes.literal_lengths),
            format_mode(modes.offsets),
            format_mode(modes.match_lengths),
        ),
        None => "none".to_string(),
    }
}

fn format_average_match(trace: zstandard::BlockTrace) -> String {
    if trace.sequence_count == 0 {
        return "0.00".to_string();
    }
    format!(
        "{:.2}",
        trace.parser_stats.matched_bytes as f64 / trace.sequence_count as f64
    )
}

fn format_repcodes(repcodes: zstandard::BlockTraceRepcodeStats) -> String {
    format!(
        "r1={} r2={} r3={} r1m1={} explicit={}",
        repcodes.rep1,
        repcodes.rep2,
        repcodes.rep3,
        repcodes.rep1_minus1,
        repcodes.explicit_offsets,
    )
}

fn format_regular_match_sources(sources: zstandard::BlockTraceRegularMatchSourceCounts) -> String {
    format!(
        "dict={} prefix={} source={} unknown={}",
        sources.dict, sources.prefix, sources.source, sources.unknown
    )
}

fn format_rep_match_sources(sources: zstandard::BlockTraceRepMatchSourceCounts) -> String {
    format!(
        "dict={} prefix={} source={} unknown={}",
        sources.dict, sources.prefix, sources.source, sources.unknown
    )
}

fn format_average_explicit_offset(stats: zstandard::BlockTraceParserStats) -> String {
    if stats.explicit_offset_count == 0 {
        return "none".to_string();
    }
    format!(
        "{:.2}",
        stats.explicit_offset_sum as f64 / stats.explicit_offset_count as f64
    )
}

fn format_first_match_source(source: Option<zstandard::BlockTraceMatchSource>) -> &'static str {
    match source.unwrap_or(zstandard::BlockTraceMatchSource::Unknown) {
        zstandard::BlockTraceMatchSource::Dict => "dict",
        zstandard::BlockTraceMatchSource::Prefix => "prefix",
        zstandard::BlockTraceMatchSource::Source => "source",
        zstandard::BlockTraceMatchSource::Rep => "rep",
        zstandard::BlockTraceMatchSource::Unknown => "unknown",
    }
}

fn format_row_search_contest(contest: Option<zstandard::BlockTraceRowSearchContest>) -> String {
    let Some(contest) = contest else {
        return "none".to_string();
    };
    format!(
        "{}:{}:{}:{}",
        format_first_match_source(Some(contest.winner)),
        contest.source_length,
        contest.dict_length,
        contest.attempts_left_before_dict
    )
}

fn format_accepted_regular_match(
    accepted: Option<zstandard::BlockTraceAcceptedRegularMatch>,
) -> String {
    let Some(accepted) = accepted else {
        return "none".to_string();
    };
    format!(
        "{}:{}:{}:{}",
        format_first_match_source(Some(accepted.source)),
        accepted.start,
        accepted.length,
        accepted.offset
    )
}

fn format_emitted_match(emitted: Option<zstandard::BlockTraceEmittedMatch>) -> String {
    let Some(emitted) = emitted else {
        return "none".to_string();
    };
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        format_emitted_match_kind(emitted.kind),
        format_first_match_source(Some(emitted.source)),
        emitted.start,
        emitted.literal_length,
        emitted.length,
        emitted.off_base,
        emitted.offset
    )
}

fn format_upstream_sequences(sequences: &[UpstreamSequenceTrace]) -> String {
    if sequences.is_empty() {
        return "none".to_string();
    }
    sequences
        .iter()
        .map(|sequence| {
            format!(
                "{}:{}:{}:{}:{}:{}:{}",
                format_upstream_kind(*sequence),
                format_upstream_source(*sequence),
                sequence.start,
                sequence.literal_length,
                sequence.match_length,
                sequence.off_base,
                sequence.raw_offset
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn format_upstream_kind(sequence: UpstreamSequenceTrace) -> &'static str {
    match sequence.kind {
        upstream_zstd::UpstreamSequenceKind::Regular => "regular",
        upstream_zstd::UpstreamSequenceKind::Rep => "rep",
    }
}

fn format_upstream_source(sequence: UpstreamSequenceTrace) -> &'static str {
    match sequence.source {
        UpstreamSequenceSource::Dict => "dict",
        UpstreamSequenceSource::Prefix => "prefix",
        UpstreamSequenceSource::Source => "source",
    }
}

fn format_emitted_match_kind(kind: zstandard::BlockTraceEmittedMatchKind) -> &'static str {
    match kind {
        zstandard::BlockTraceEmittedMatchKind::Regular => "regular",
        zstandard::BlockTraceEmittedMatchKind::Rep => "rep",
        zstandard::BlockTraceEmittedMatchKind::Unknown => "unknown",
    }
}

fn format_offset_codes(offset_code_counts: &[u32; 32]) -> String {
    let mut parts = Vec::new();
    for (code, &count) in offset_code_counts.iter().enumerate() {
        if count != 0 {
            parts.push(format!("{code}:{count}"));
        }
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(",")
    }
}

fn format_decision(decision: BlockTraceDecision) -> &'static str {
    match decision {
        BlockTraceDecision::Raw => "raw",
        BlockTraceDecision::Rle => "rle",
        BlockTraceDecision::Compressed => "compressed",
    }
}

fn format_upstream_strategy(strategy: zstandard::BlockTraceUpstreamStrategy) -> &'static str {
    match strategy {
        zstandard::BlockTraceUpstreamStrategy::Fast => "fast",
        zstandard::BlockTraceUpstreamStrategy::DoubleFast => "dfast",
        zstandard::BlockTraceUpstreamStrategy::Greedy => "greedy",
        zstandard::BlockTraceUpstreamStrategy::Lazy => "lazy",
        zstandard::BlockTraceUpstreamStrategy::Lazy2 => "lazy2",
        zstandard::BlockTraceUpstreamStrategy::BinaryTreeLazy2 => "btlazy2",
        zstandard::BlockTraceUpstreamStrategy::BinaryTreeOpt => "btopt",
        zstandard::BlockTraceUpstreamStrategy::BinaryTreeUltra => "btultra",
        zstandard::BlockTraceUpstreamStrategy::BinaryTreeUltra2 => "btultra2",
    }
}

fn format_dictionary_mode(mode: zstandard::BlockTraceDictionaryMode) -> &'static str {
    match mode {
        zstandard::BlockTraceDictionaryMode::None => "none",
        zstandard::BlockTraceDictionaryMode::ExtDict => "extdict",
        zstandard::BlockTraceDictionaryMode::DictMatchState => "dictmatch",
    }
}

fn format_parser_strategy(strategy: zstandard::BlockTraceParserStrategy) -> &'static str {
    match strategy {
        zstandard::BlockTraceParserStrategy::Fast => "fast",
        zstandard::BlockTraceParserStrategy::DoubleFast => "double-fast",
        zstandard::BlockTraceParserStrategy::Greedy => "greedy",
        zstandard::BlockTraceParserStrategy::Lazy => "lazy",
        zstandard::BlockTraceParserStrategy::Lazy2 => "lazy2",
        zstandard::BlockTraceParserStrategy::GreedyRow => "greedy-row",
        zstandard::BlockTraceParserStrategy::LazyRow => "lazy-row",
        zstandard::BlockTraceParserStrategy::Lazy2Row => "lazy2-row",
        zstandard::BlockTraceParserStrategy::BinaryTreeLazy2 => "bt-lazy2",
        zstandard::BlockTraceParserStrategy::BinaryTreeOpt => "bt-opt",
        zstandard::BlockTraceParserStrategy::BinaryTreeUltra => "bt-ultra",
        zstandard::BlockTraceParserStrategy::BinaryTreeUltra2 => "bt-ultra2",
    }
}

fn format_row_hash_log(row_hash_log: Option<u32>) -> String {
    row_hash_log
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn format_dictionary_table_source(
    source: zstandard::BlockTraceDictionaryTableSource,
) -> &'static str {
    match source {
        zstandard::BlockTraceDictionaryTableSource::None => "none",
        zstandard::BlockTraceDictionaryTableSource::Prefix => "prefix",
        zstandard::BlockTraceDictionaryTableSource::Prepared => "prepared",
    }
}

fn format_mode(mode: BlockTraceMode) -> &'static str {
    match mode {
        BlockTraceMode::Predefined => "basic",
        BlockTraceMode::Rle => "rle",
        BlockTraceMode::FseCompressed => "compressed",
        BlockTraceMode::Repeat => "repeat",
    }
}

fn format_upstream_modes(modes: Option<(u8, u8, u8)>) -> String {
    let Some((ll, of, ml)) = modes else {
        return "none".to_string();
    };
    format!(
        "{}/{}/{}",
        format_upstream_mode(ll),
        format_upstream_mode(of),
        format_upstream_mode(ml)
    )
}

fn format_block_type(block_type: BlockType) -> &'static str {
    match block_type {
        BlockType::Raw => "raw",
        BlockType::Rle => "rle",
        BlockType::Compressed => "compressed",
    }
}

fn format_first_divergent_block(mismatch: Option<BlockStatsMismatch>) -> String {
    let Some(mismatch) = mismatch else {
        return "none".to_string();
    };
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        mismatch.block_index,
        format_block_type(mismatch.actual.block_type),
        mismatch.actual.payload_size,
        mismatch.actual.literal_section_size,
        mismatch.actual.sequence_section_size,
        mismatch.actual.sequence_count,
        format_upstream_modes(mismatch.actual.modes),
        format_block_type(mismatch.upstream.block_type),
        mismatch.upstream.payload_size,
        mismatch.upstream.literal_section_size,
        mismatch.upstream.sequence_section_size,
        mismatch.upstream.sequence_count,
        format_upstream_modes(mismatch.upstream.modes),
    )
}

fn format_upstream_mode(mode: u8) -> &'static str {
    match mode {
        0 => "basic",
        1 => "rle",
        2 => "compressed",
        3 => "repeat",
        _ => "unknown",
    }
}

fn first_compressed_block_payload(frame: &[u8]) -> Vec<u8> {
    let header = match parse_frame_header(frame).expect("frame header should parse") {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block =
        parse_block_header(&frame[header.header_size..]).expect("block header should parse");
    assert_eq!(block.block_type, BlockType::Compressed);

    let payload_start = header.header_size + 3;
    let payload_end = payload_start + block_payload_size(block.block_type, block.block_size);
    frame[payload_start..payload_end].to_vec()
}

fn block_payload_size(block_type: BlockType, block_size: u32) -> usize {
    match block_type {
        BlockType::Raw | BlockType::Compressed => block_size as usize,
        BlockType::Rle => 1,
    }
}

#[derive(Debug, Clone, Copy)]
struct LiteralsHeader {
    header_size: usize,
    compressed_size: usize,
}

impl LiteralsHeader {
    fn payload_end(self) -> usize {
        self.header_size + self.compressed_size
    }
}

fn parse_literals_header(src: &[u8]) -> LiteralsHeader {
    let header0 = src[0];
    let block_type = header0 & 0x3;
    let size_format = (header0 >> 2) & 0x3;

    let (header_size, compressed_size) = match block_type {
        0 | 1 => match size_format {
            0 | 2 => {
                let size = (header0 >> 3) as usize;
                (1, size)
            }
            1 => {
                let size = ((src[0] as usize) >> 4) | ((src[1] as usize) << 4);
                (2, size)
            }
            3 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                (3, value >> 4)
            }
            _ => unreachable!(),
        },
        2 | 3 => match size_format {
            0 | 1 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                (3, (value >> 14) & 0x03ff)
            }
            2 => {
                let value = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
                (4, (value >> 18) & 0x3fff)
            }
            3 => {
                let value = (src[0] as u64)
                    | ((src[1] as u64) << 8)
                    | ((src[2] as u64) << 16)
                    | ((src[3] as u64) << 24)
                    | ((src[4] as u64) << 32);
                (5, ((value >> 22) & 0x3ffff) as usize)
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    LiteralsHeader {
        header_size,
        compressed_size,
    }
}

fn compressed_block_sequence_count(payload: &[u8]) -> usize {
    let literals = parse_literals_header(payload);
    decode_sequence_count(&payload[literals.payload_end()..])
}

fn compressed_block_sequence_modes(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let literals = parse_literals_header(payload);
    let sequence_section = &payload[literals.payload_end()..];
    let sequence_count = decode_sequence_count(sequence_section);
    if sequence_count == 0 {
        return None;
    }
    let mode_index = if sequence_count < 128 {
        1
    } else if sequence_count < 0x7F00 {
        2
    } else {
        3
    };
    let modes = sequence_section[mode_index];
    Some(((modes >> 6) & 0x3, (modes >> 4) & 0x3, (modes >> 2) & 0x3))
}

fn decode_sequence_count(src: &[u8]) -> usize {
    let byte0 = src[0];
    if byte0 < 128 {
        byte0 as usize
    } else if byte0 < 255 {
        ((byte0 as usize - 128) << 8) + src[1] as usize
    } else {
        0x7F00 + src[1] as usize + ((src[2] as usize) << 8)
    }
}
