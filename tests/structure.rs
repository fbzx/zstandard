//! **Layer 2 of `docs/ORACLE_PLAN.md`: does the parse still agree where the
//! encoding does not?**
//!
//! Byte-identical literals are the sharp instrument here. The literals section
//! is the input minus the matched regions, so if it matches upstream's byte for
//! byte then the match/literal partition agrees, which pins every match's
//! position and length without asserting anything at all about how the offsets
//! were coded. Add an equal sequence count and equal compression modes and the
//! only remaining freedom is the sequence bitstream. That is enough to localise
//! a defect to one side of the parse/encode line in a single test run.
//!
//! **This is a diagnostic, not a gate.** It references upstream's *choices*, so
//! it decays as this crate deliberately improves on them. Written as a gate it
//! would fail on success: a parse that finds a longer match than upstream's is
//! a better parse and different literals. So every row is classified and the
//! classification recorded, and a row is allowed to move toward *weaker*
//! agreement only by editing the record, which is where the reviewing happens.
//!
//! Deliberately an integration test rather than a `#[cfg(test)]` one in
//! `src/encode.rs`, where its ancestor lived. `SequencePlan::trace_enabled`
//! defaults to `cfg!(test)`, so a unit test runs the *tracing* planner copies
//! and an integration test runs the `_no_trace` copies that ship. A structural
//! comparison is worth much more against the code that ships.

use std::collections::BTreeMap;

#[allow(dead_code)]
#[path = "../src/support/corpora.rs"]
mod benchmark_corpora;
#[allow(dead_code)]
#[path = "../src/support/upstream_zstd.rs"]
mod upstream_trace_helper;

use upstream_trace_helper::UpstreamFirstBlockSections;
use zstandard::{
    BlockType, CompressionLevel, EncoderOptions, FrameHeader, encode_all_with_options,
    parse_block_header, parse_frame_header,
};

/// Whether a frame's first block is a compressed one, which is the precondition
/// for having sections to compare at all.
///
/// Checked rather than assumed because it is not always true: on
/// `pseudorandom` no parse pays for itself and both sides emit a raw block. The
/// section parser panics on those, and a `continue` past them would be exactly
/// the silent coverage loss this layer was rewritten to avoid, so the condition
/// is classified instead.
fn first_block_is_compressed(frame: &[u8]) -> bool {
    let header = match parse_frame_header(frame).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    parse_block_header(&frame[header.header_size..])
        .unwrap()
        .block_type
        == BlockType::Compressed
}

/// How much of upstream's first block this crate reproduced.
///
/// Ordered from strongest to weakest, and the ordering is the point: a row may
/// move up it silently, and may only move down it by an edit to
/// [`FIRST_BLOCK_AGREEMENT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Agreement {
    /// Every section byte-identical. Byte parity, at block granularity.
    Identical,
    /// Byte-identical literals, equal sequence count and equal compression
    /// modes, differing only in the sequence bitstream. The parse agrees and
    /// the encoding does not, which is what an offset-coding divergence like
    /// the repcode substitution looks like.
    SameParseDifferentEncoding,
    /// The literals, the sequence count or the modes differ, so the two sides
    /// partitioned the input differently. Not a defect on its own -- a longer
    /// match is a better parse and different literals -- but it is the point at
    /// which this layer stops being able to say anything.
    DifferentParse,
    /// One side or both emitted a raw or RLE first block, so there is no parse
    /// to compare. Recorded rather than skipped, because a row that quietly
    /// stopped being compared reads exactly like a row that agrees.
    NoCompressedBlock,
}

impl Agreement {
    fn classify(ours: &UpstreamFirstBlockSections, theirs: &UpstreamFirstBlockSections) -> Self {
        if ours == theirs {
            return Self::Identical;
        }
        if ours.literals == theirs.literals
            && ours.sequence_count == theirs.sequence_count
            && ours.sequence_modes == theirs.sequence_modes
            && ours.last_block == theirs.last_block
        {
            return Self::SameParseDifferentEncoding;
        }
        Self::DifferentParse
    }

    fn label(self) -> &'static str {
        match self {
            Self::Identical => "identical",
            Self::SameParseDifferentEncoding => "same-parse",
            Self::DifferentParse => "different-parse",
            Self::NoCompressedBlock => "no-compressed-block",
        }
    }
}

/// One level per parser strategy *at this sweep's input size*, which is not the
/// same list as at another size: `clevels.h` selects a different cparams row per
/// size class, so a level's strategy is a function of both.
const LEVELS: [i32; 9] = [1, 3, 5, 6, 8, 13, 16, 18, 22];

const SIZE: usize = 512 * 1024;
const BLOCK: usize = 128 * 1024;

/// Every row that is not [`Agreement::Identical`], with the class it is in.
///
/// **This table is expected to shrink from the bottom and grow at the top.**
/// A row leaving it entirely, or moving from `different-parse` to `same-parse`,
/// is this crate agreeing with upstream about more than it used to. A row
/// moving the other way is this crate's parse diverging, which may well be an
/// improvement but is never automatic: it costs an edit here, and the edit is
/// where somebody has to say which it was.
///
/// Rows absent from this table are asserted byte-identical: 51 of the 81.
///
/// **The `same-parse` rows are one divergence wearing seventeen hats.** Levels
/// 5, 6 and 8 run the row match finder and level 13 runs btlazy2, and all four
/// substitute a repcode when a regular match's distance is still live, which
/// C's lazy family never does. That every one of these rows has byte-identical
/// literals *and* an identical sequence count is what says so: the parse is
/// untouched and only the offset coding differs. If the substitution had
/// perturbed a single parse, that row would be `different-parse` instead, and
/// none is.
const FIRST_BLOCK_AGREEMENT: &[(&str, i32, Agreement)] = &[
    ("small-alphabet", 6, Agreement::SameParseDifferentEncoding),
    ("small-alphabet", 8, Agreement::SameParseDifferentEncoding),
    ("small-alphabet", 13, Agreement::DifferentParse),
    ("json-records", 5, Agreement::SameParseDifferentEncoding),
    ("json-records", 6, Agreement::SameParseDifferentEncoding),
    ("json-records", 8, Agreement::SameParseDifferentEncoding),
    ("json-records", 13, Agreement::SameParseDifferentEncoding),
    ("log-lines", 5, Agreement::SameParseDifferentEncoding),
    ("log-lines", 6, Agreement::SameParseDifferentEncoding),
    ("log-lines", 8, Agreement::SameParseDifferentEncoding),
    ("log-lines", 13, Agreement::SameParseDifferentEncoding),
    ("mixed-entropy", 13, Agreement::SameParseDifferentEncoding),
    ("wikipedia", 6, Agreement::SameParseDifferentEncoding),
    ("wikipedia", 13, Agreement::SameParseDifferentEncoding),
    ("tabular-csv", 1, Agreement::DifferentParse),
    ("tabular-csv", 5, Agreement::SameParseDifferentEncoding),
    ("tabular-csv", 6, Agreement::SameParseDifferentEncoding),
    ("tabular-csv", 8, Agreement::SameParseDifferentEncoding),
    ("tabular-csv", 13, Agreement::SameParseDifferentEncoding),
    ("tabular-csv", 18, Agreement::DifferentParse),
    ("tabular-csv", 22, Agreement::DifferentParse),
    // No parse to compare: the body is incompressible, so both sides emit a raw
    // first block at every level. Listed row by row rather than special-cased on
    // the corpus name, so that a level which *started* finding a compressed
    // block would show up as a change instead of being absorbed.
    ("pseudorandom", 1, Agreement::NoCompressedBlock),
    ("pseudorandom", 3, Agreement::NoCompressedBlock),
    ("pseudorandom", 5, Agreement::NoCompressedBlock),
    ("pseudorandom", 6, Agreement::NoCompressedBlock),
    ("pseudorandom", 8, Agreement::NoCompressedBlock),
    ("pseudorandom", 13, Agreement::NoCompressedBlock),
    ("pseudorandom", 16, Agreement::NoCompressedBlock),
    ("pseudorandom", 18, Agreement::NoCompressedBlock),
    ("pseudorandom", 22, Agreement::NoCompressedBlock),
];

/// Rows where this crate's whole frame is larger than upstream's, with the two
/// sizes as measured. One-directional, like every size bound in this tree: a
/// row that gets smaller than upstream is not a failure and is not listed.
///
/// Three entries, all small, and the split between them is the useful part.
/// `tabular-csv` at 22 is a btultra2 parse that diverges from upstream's in the
/// very first block and lands 15 bytes worse on 68 KB. The two `mixed-entropy`
/// rows are the opposite shape: their first block is byte-identical, so
/// whatever costs those 9 bytes happens in a *later* block and this sweep
/// cannot see it. That is the honest limit of a first-block comparator, and the
/// reason the size bound here is taken on the whole frame.
///
/// All three are under 0.03% and are recorded rather than chased. The entries
/// exist so that none of them can grow silently.
const FIRST_BLOCK_SIZE_GAPS: &[(&str, i32, usize, usize)] = &[
    ("mixed-entropy", 18, 172476, 172467),
    ("mixed-entropy", 22, 172477, 172468),
    ("tabular-csv", 22, 68675, 68660),
];

fn agreement_record() -> BTreeMap<(&'static str, i32), Agreement> {
    FIRST_BLOCK_AGREEMENT
        .iter()
        .map(|&(corpus, level, agreement)| ((corpus, level), agreement))
        .collect()
}

#[test]
fn the_parse_agrees_with_upstream_where_it_is_recorded_to() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    let expected = agreement_record();
    let mut observed = Vec::new();
    let mut compared = 0usize;
    let mut failures = Vec::new();
    let mut size_gaps_hit = Vec::new();

    for case in benchmark_corpora::benchmark_report_cases(SIZE) {
        if case.dict_kind != benchmark_corpora::DictKind::None {
            continue;
        }
        for level in LEVELS {
            let options = EncoderOptions {
                block_size: BLOCK,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let our_frame = encode_all_with_options(&case.input, options).unwrap();
            let their_frame = upstream_trace_helper::compress_once(
                helper,
                "compress-regular-configured",
                level,
                false,
                &case.input,
            );
            compared += 1;

            let comparable =
                first_block_is_compressed(&our_frame) && first_block_is_compressed(&their_frame);
            let sections = comparable.then(|| {
                (
                    upstream_trace_helper::parse_first_block_sections(&our_frame),
                    upstream_trace_helper::parse_first_block_sections(&their_frame),
                )
            });
            let agreement = match &sections {
                Some((ours, theirs)) => Agreement::classify(ours, theirs),
                None => Agreement::NoCompressedBlock,
            };
            if agreement != Agreement::Identical {
                observed.push((case.name, level, agreement));
            }

            let recorded = expected
                .get(&(case.name, level))
                .copied()
                .unwrap_or(Agreement::Identical);
            if agreement > recorded {
                let detail = match &sections {
                    Some((ours, theirs)) => format!(
                        " (ours {} literal bytes / {} sequences, upstream {} / {})",
                        ours.literals.regenerated_size,
                        ours.sequence_count,
                        theirs.literals.regenerated_size,
                        theirs.sequence_count,
                    ),
                    None => String::new(),
                };
                failures.push(format!(
                    "{} L{level}: agreement weakened from {} to {}{detail}",
                    case.name,
                    recorded.label(),
                    agreement.label(),
                ));
            } else if agreement < recorded {
                failures.push(format!(
                    "{} L{level}: agreement strengthened from {} to {}; record it",
                    case.name,
                    recorded.label(),
                    agreement.label(),
                ));
            }

            // Durable regardless of which class the row is in, and the reason
            // this test is still worth running on a row whose parse has left
            // the comparison: whatever we did instead, the frame did not grow.
            // Taken on the whole frame rather than the first block so that a
            // `no-compressed-block` row is bounded too.
            if our_frame.len() > their_frame.len() {
                let row = (case.name, level, our_frame.len(), their_frame.len());
                if FIRST_BLOCK_SIZE_GAPS.contains(&row) {
                    size_gaps_hit.push(row);
                } else {
                    failures.push(format!(
                        "{} L{level}: frame is {} bytes against upstream's {}",
                        case.name,
                        our_frame.len(),
                        their_frame.len(),
                    ));
                }
            }
        }
    }

    // Asserted rather than assumed. A corpus list that silently shrank would
    // otherwise leave this passing on a fraction of what it claims to cover.
    assert_eq!(
        compared,
        9 * LEVELS.len(),
        "the structural sweep did not cover its grid",
    );

    // An exception that stopped applying is an exception that should be
    // deleted. Leaving it in place would silently re-permit the gap later.
    for row in FIRST_BLOCK_SIZE_GAPS {
        if !size_gaps_hit.contains(row) {
            failures.push(format!(
                "{} L{}: recorded size gap of {} against {} no longer applies; delete it",
                row.0, row.1, row.2, row.3,
            ));
        }
    }

    if !failures.is_empty() {
        let report = observed
            .iter()
            .map(|(corpus, level, agreement)| {
                format!("    (\"{corpus}\", {level}, Agreement::{agreement:?}),")
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} structural row(s) moved:\n{}\n\nobserved table:\n{report}",
            failures.len(),
            failures.join("\n"),
        );
    }
}
