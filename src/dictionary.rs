use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    encode::{EncoderOptions, compression_parameters_for_options},
    entropy::{
        fse,
        huff0::{CTableX1, HuffmanDTable, read_ctable_x1_with_repeat_validity, read_dtable_x2},
        mem::highbit32,
    },
    error::{Error, Result},
    frame::read_u32_le,
    literals::LiteralsState,
    sequence::{
        RepeatOffsets, SequenceEncodingPartState, SequenceEncodingState, SequenceEncodingTable,
        SequenceTablesState, repeat_mode_for_normalized_counts,
    },
    window::{
        DictionaryPriceSeed, MatchFinderParameters, PreparedDictionaryMatchState,
        PreparedDictionaryMatchStateKey, build_prepared_dictionary_match_state,
    },
};

pub(crate) const ZSTD_DICTIONARY_MAGIC: u32 = 0xEC30_A437;
const OFFSET_MAX_SYMBOL_VALUE: u32 = 31;
const OFFSET_MAX_TABLE_LOG: usize = 8;
const MATCH_LENGTH_MAX_SYMBOL_VALUE: u32 = 52;
const MATCH_LENGTH_MAX_TABLE_LOG: usize = 9;
const LITERAL_LENGTH_MAX_SYMBOL_VALUE: u32 = 35;
const LITERAL_LENGTH_MAX_TABLE_LOG: usize = 9;

/// A dictionary parsed for compression and reusable across many encodes.
///
/// Separate from [`DecoderDictionary`] because the two directions need
/// different tables built from the same bytes, and neither should carry the
/// other's. A formatted dictionary's encoding tables are 11 KiB and its
/// decoding tables are 22.5 KiB; fusing them made every dictionary 33.6 KiB
/// whichever way it was used. Upstream draws the same line, as `ZSTD_CDict`
/// and `ZSTD_DDict`.
///
/// To use one dictionary in both directions, build both types over the same
/// `Arc<[u8]>`. The content bytes are then stored once and only the tables
/// differ:
///
/// ```
/// use std::sync::Arc;
/// use zstandard::{
///     DecoderDictionary, EncoderDictionary, decode_all_with_prepared_dict,
///     encode_all_with_prepared_dict,
/// };
///
/// let bytes: Arc<[u8]> = Arc::from(b"shared dictionary content".as_slice());
/// let encoding = EncoderDictionary::from_shared(Arc::clone(&bytes))?;
/// let decoding = DecoderDictionary::from_shared(bytes)?;
///
/// let frame = encode_all_with_prepared_dict(b"dictionary content here", &encoding)?;
/// assert_eq!(decode_all_with_prepared_dict(&frame, &decoding)?, b"dictionary content here");
/// # Ok::<(), zstandard::Error>(())
/// ```
///
/// Cloning is cheap: the parsed tables sit behind an `Arc`, as do the
/// parser-built match-state tables, so a clone shares both rather than
/// copying them.
#[derive(Clone)]
pub struct EncoderDictionary<'a> {
    dictionary: Dictionary<'a>,
    prepared_match_states:
        Arc<Mutex<HashMap<PreparedDictionaryMatchStateKey, Arc<PreparedDictionaryMatchState>>>>,
}

/// A dictionary parsed for decompression and reusable across many decodes.
///
/// The decoding half of the pair [`EncoderDictionary`] documents, holding the
/// Huffman and FSE *decoding* tables and none of the encoding ones. Cloning is
/// cheap for the same reason.
#[derive(Clone)]
pub struct DecoderDictionary<'a> {
    dictionary: Dictionary<'a>,
}

impl<'a> EncoderDictionary<'a> {
    /// Parse a dictionary from `src`. Accepts both formatted Zstandard
    /// dictionaries (introduced by the dictionary magic number) and raw
    /// content dictionaries (any other byte slice, treated as content prefix
    /// only).
    pub fn new(src: &'a [u8]) -> Result<Self> {
        Ok(Self {
            dictionary: Dictionary::parse(src, TableDirection::Encoding)?,
            prepared_match_states: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Parse a dictionary from bytes this crate keeps alive itself, yielding an
    /// `EncoderDictionary<'static>`.
    ///
    /// [`Self::new`] borrows, which is the cheaper choice whenever the caller
    /// can keep the buffer alive. This one exists for when it cannot: a
    /// dictionary stored in a struct, or shared between threads, cannot be a
    /// borrow of something the same struct owns. Without this the only way to
    /// hold one was to reparse per call, which rebuilds the entropy tables and
    /// throws away the parser-built match-state tables that
    /// [`Self::new`]'s cache exists to keep -- exactly the cost a prepared
    /// dictionary is for.
    ///
    /// Accepts anything that becomes an `Arc<[u8]>`, so a `Vec<u8>` is moved in
    /// rather than copied, and an existing `Arc<[u8]>` is shared with whatever
    /// else holds it -- including a [`DecoderDictionary`] over the same bytes.
    ///
    /// ```
    /// use zstandard::{EncoderDictionary, encode_all_with_prepared_dict};
    ///
    /// // Owns its bytes, so it can live in a struct or move across threads.
    /// let dict: EncoderDictionary<'static> =
    ///     EncoderDictionary::from_shared(b"shared dictionary content".to_vec())?;
    ///
    /// let frame = encode_all_with_prepared_dict(b"dictionary content here", &dict)?;
    /// # assert!(!frame.is_empty());
    /// # Ok::<(), zstandard::Error>(())
    /// ```
    pub fn from_shared(bytes: impl Into<Arc<[u8]>>) -> Result<EncoderDictionary<'static>> {
        Ok(EncoderDictionary {
            dictionary: Dictionary::parse_shared(bytes.into(), TableDirection::Encoding)?,
            prepared_match_states: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Build the match-state tables `options` will need now, instead of on the
    /// first compression that needs them.
    ///
    /// Parsing a dictionary reads its entropy tables; it does not index its
    /// content for matching. That indexing is what the parser actually spends
    /// its dictionary time on, and it is sized by the compression parameters,
    /// which parsing does not yet know. So it happens lazily, on first use, and
    /// is cached from then on — which leaves the first compression after a
    /// parse paying for every later one. Upstream splits the same work the same
    /// way and resolves it in the other direction: `ZSTD_createCDict` takes a
    /// compression level and indexes the content immediately.
    ///
    /// This is that. Call it once, off whatever path is latency-sensitive, and
    /// every compression under these `options` finds the tables waiting.
    /// Skipping it costs nothing but the ordering: the first compression builds
    /// exactly the same tables and caches them the same way.
    ///
    /// The tables are keyed by geometry rather than by source length, so one
    /// call covers every input size at these `options` — pinned by
    /// `one_preparation_covers_every_source_size`. Two levels that resolve to
    /// the same geometry share one entry; two that do not each want their own
    /// call. Nothing here is discarded, so preparing many levels against one
    /// dictionary holds a set of tables for each.
    ///
    /// A raw-content dictionary has nothing to prepare and this does nothing:
    /// raw content is matched against directly, without the built tables a
    /// formatted dictionary gets.
    ///
    /// ```
    /// use zstandard::{
    ///     CompressionLevel, EncoderDictionary, EncoderOptions,
    ///     encode_all_with_prepared_dict_and_options,
    /// };
    ///
    /// let dictionary = EncoderDictionary::from_shared(b"shared dictionary content".to_vec())?;
    /// let options = EncoderOptions {
    ///     compression_level: CompressionLevel::BETTER,
    ///     ..Default::default()
    /// };
    ///
    /// // Pay for the tables here, once, rather than inside the first request.
    /// dictionary.prepare(options);
    ///
    /// let frame = encode_all_with_prepared_dict_and_options(
    ///     b"dictionary content here",
    ///     &dictionary,
    ///     options,
    /// )?;
    /// # assert!(!frame.is_empty());
    /// # Ok::<(), zstandard::Error>(())
    /// ```
    pub fn prepare(&self, options: EncoderOptions) {
        // `None` rather than a source length: the key covers table geometry
        // only, and the geometry a dictionary's tables are built with comes
        // from parameters resolved with the source size left unknown. Passing a
        // length here would resolve the same key by a longer route.
        let params = compression_parameters_for_options(options, None, Some(&self.dictionary));
        self.prepared_match_state(params.match_finder);
    }

    /// Treat `src` as raw content unconditionally, without inspecting it for
    /// the dictionary magic number.
    ///
    /// Dictionary training needs this. It measures candidate content by
    /// compressing samples against it, and that content is a slice of the
    /// samples themselves, which may begin with any four bytes at all. Letting
    /// [`Self::new`] decide would reparse such a candidate as a formatted
    /// dictionary and measure something other than the candidate.
    pub(crate) fn raw_content(src: &'a [u8]) -> Self {
        Self {
            dictionary: Dictionary::raw(src),
            prepared_match_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Dictionary id encoded in the formatted dictionary header, or `0` for raw-content dictionaries.
    pub fn id(&self) -> u32 {
        self.dictionary.id()
    }

    /// `true` if the source bytes were treated as raw content rather than a formatted dictionary.
    pub fn is_raw_content(&self) -> bool {
        self.dictionary.is_raw_content()
    }

    pub(crate) fn as_inner(&self) -> &Dictionary<'a> {
        &self.dictionary
    }

    pub(crate) fn prepared_match_state(
        &self,
        params: MatchFinderParameters,
    ) -> Option<Arc<PreparedDictionaryMatchState>> {
        if self.dictionary.is_raw_content() || self.dictionary.content().is_empty() {
            return None;
        }

        let key = PreparedDictionaryMatchStateKey::new(params);
        if let Some(state) = self
            .prepared_match_states
            .lock()
            .expect("prepared dictionary cache lock poisoned")
            .get(&key)
            .cloned()
        {
            return Some(state);
        }

        let state = Arc::new(build_prepared_dictionary_match_state(
            self.dictionary.matching_content(),
            params,
        )?);
        self.prepared_match_states
            .lock()
            .expect("prepared dictionary cache lock poisoned")
            .insert(key, state.clone());
        Some(state)
    }

    /// Every geometry this dictionary currently holds tables for.
    ///
    /// The cache is otherwise invisible, and its *size* is the thing worth
    /// asserting on: a key that splits on something the tables do not depend on
    /// still compresses correctly, and shows up only as a second entry holding
    /// a duplicate of the first.
    #[cfg(test)]
    fn cached_geometries(&self) -> Vec<PreparedDictionaryMatchStateKey> {
        self.prepared_match_states
            .lock()
            .expect("prepared dictionary cache lock poisoned")
            .keys()
            .copied()
            .collect()
    }
}

impl<'a> DecoderDictionary<'a> {
    /// Parse a dictionary from `src`. Accepts both formatted Zstandard
    /// dictionaries and raw content dictionaries, on the same rule
    /// [`EncoderDictionary::new`] uses.
    pub fn new(src: &'a [u8]) -> Result<Self> {
        Ok(Self {
            dictionary: Dictionary::parse(src, TableDirection::Decoding)?,
        })
    }

    /// Parse a dictionary from bytes this crate keeps alive itself, yielding a
    /// `DecoderDictionary<'static>`.
    ///
    /// The decoding counterpart of [`EncoderDictionary::from_shared`], and it
    /// exists for the same reason: a dictionary held in a struct or shared
    /// between threads cannot be a borrow. Passing an existing `Arc<[u8]>`
    /// shares the content bytes with whatever else holds them.
    ///
    /// ```
    /// use zstandard::{DecoderDictionary, EncoderDictionary, decode_all_with_prepared_dict,
    ///     encode_all_with_prepared_dict};
    ///
    /// let dict: DecoderDictionary<'static> =
    ///     DecoderDictionary::from_shared(b"shared dictionary content".to_vec())?;
    ///
    /// # let encoding = EncoderDictionary::from_shared(b"shared dictionary content".to_vec())?;
    /// # let frame = encode_all_with_prepared_dict(b"dictionary content here", &encoding)?;
    /// assert_eq!(decode_all_with_prepared_dict(&frame, &dict)?, b"dictionary content here");
    /// # Ok::<(), zstandard::Error>(())
    /// ```
    pub fn from_shared(bytes: impl Into<Arc<[u8]>>) -> Result<DecoderDictionary<'static>> {
        Ok(DecoderDictionary {
            dictionary: Dictionary::parse_shared(bytes.into(), TableDirection::Decoding)?,
        })
    }

    /// Dictionary id encoded in the formatted dictionary header, or `0` for raw-content dictionaries.
    pub fn id(&self) -> u32 {
        self.dictionary.id()
    }

    /// `true` if the source bytes were treated as raw content rather than a formatted dictionary.
    pub fn is_raw_content(&self) -> bool {
        self.dictionary.is_raw_content()
    }

    pub(crate) fn as_inner(&self) -> &Dictionary<'a> {
        &self.dictionary
    }
}

impl<'a> TryFrom<&'a [u8]> for EncoderDictionary<'a> {
    type Error = Error;

    fn try_from(value: &'a [u8]) -> Result<Self> {
        Self::new(value)
    }
}

impl<'a> TryFrom<&'a [u8]> for DecoderDictionary<'a> {
    type Error = Error;

    fn try_from(value: &'a [u8]) -> Result<Self> {
        Self::new(value)
    }
}

/// Where a dictionary's bytes live.
///
/// A borrowed dictionary stays zero-copy, which is what every caller that can
/// keep the bytes alive itself should use. The shared variant exists because
/// the parsed form is worth reusing and a borrow cannot outlive the buffer it
/// came from: a caller holding a dictionary in a struct, or sharing one across
/// threads, would otherwise have to reparse per call and rebuild the
/// match-state tables with it. This is upstream's `ZSTD_createCDict` against
/// `ZSTD_createCDict_byReference`.
#[derive(Clone)]
enum DictionaryBytes<'a> {
    Borrowed(&'a [u8]),
    Shared(Arc<[u8]>),
}

impl DictionaryBytes<'_> {
    #[inline]
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

/// Which half of a formatted dictionary's entropy tables to build.
///
/// The serialized dictionary carries one description of each table, and the
/// two directions need it in different shapes: the decoder wants Huffman and
/// FSE *decoding* tables, the encoder wants *encoding* tables built from the
/// same normalized counts. Building both is what [`EncoderDictionary`] and
/// [`DecoderDictionary`] exist to avoid.
///
/// **Both directions accept and reject the same bytes, and it is worth knowing
/// why, because it is not guaranteed by construction.** What validates a
/// dictionary is the read rather than the build: the Huffman header parse and
/// `fse::read_ncount`, both of which run in either arm. `read_ncount` bounds
/// the symbol value and the table log itself, so everything `build_dtable`
/// checks before building is already settled by the time either arm branches.
///
/// The builders are *not* symmetric, which is the part to watch. `build_ctable`
/// rejects a normalized count below `-1`; `build_dtable` has no equivalent
/// check. Nothing was found that reaches it -- `read_ncount` appears to emit
/// counts at or above `-1` by construction -- but "appears to" is the strength
/// of the claim, so it is held by test rather than by argument:
/// `the_dictionary_directions_agree_on_what_parses` in `tests/property.rs`
/// drives both directions over mutations of a real dictionary, and the unit
/// test below covers the truncation boundaries deterministically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TableDirection {
    Encoding,
    Decoding,
}

/// The entropy tables only the decoder reads: 22.5 KiB.
///
/// Behind an `Arc` in [`Dictionary`] rather than inline for two reasons. An
/// encoder must not carry it, which is the whole point of the split. And a
/// dictionary is cloned per context, so inline tables made every clone a
/// 33.6 KiB memcpy while the doc claimed cloning was cheap; an `Arc` makes
/// that claim true and shares one copy across every clone.
///
/// This does not contradict the note on `HuffmanDTable` about boxing being the
/// wrong trade. That is about the per-*frame* value on the decode path, where
/// the allocation would recur once per frame. These are built once per
/// dictionary, and the single pointer chase they add lands in `literals_state`,
/// which already copies the whole 16 KiB table out by value.
pub(crate) struct DecoderTables {
    huffman_table: Option<HuffmanDTable>,
    offset_table: fse::DTable,
    match_length_table: fse::DTable,
    literal_length_table: fse::DTable,
}

/// The entropy tables only the encoder reads: 11 KiB. `Arc` for the same two
/// reasons as [`DecoderTables`].
pub(crate) struct EncoderTables {
    huffman_encoding_table: CTableX1,
    huffman_repeat_valid: bool,
    offset_encoding_state: SequenceEncodingPartState,
    match_length_encoding_state: SequenceEncodingPartState,
    literal_length_encoding_state: SequenceEncodingPartState,
}

/// Read through a direction's accessor on a dictionary parsed for the other
/// one. Unreachable through the public API, where the direction is the type.
const DECODER_TABLES_MISSING: &str =
    "decode path read a dictionary parsed for encoding; it needs a DecoderDictionary";
const ENCODER_TABLES_MISSING: &str =
    "encode path read a dictionary parsed for decoding; it needs an EncoderDictionary";

#[derive(Clone)]
pub(crate) struct Dictionary<'a> {
    id: u32,
    source_size: usize,
    source: DictionaryBytes<'a>,
    /// Where the content begins inside `source`, rather than a second slice of
    /// it. Two slices cannot both borrow from an `Arc` this struct also owns.
    content_start: usize,
    /// Explicit rather than derived from the tables being absent, which is how
    /// this used to be answered. Once a parse builds one direction and not the
    /// other, "no tables" stops meaning "raw content" and starts meaning "not
    /// this direction" -- and the two must not be confused, because raw content
    /// legitimately has no tables while the other is a bug.
    raw_content: bool,
    decoder_tables: Option<Arc<DecoderTables>>,
    encoder_tables: Option<Arc<EncoderTables>>,
    repeat_offsets: RepeatOffsets,
}

impl<'a> Dictionary<'a> {
    pub(crate) fn parse(src: &'a [u8], direction: TableDirection) -> Result<Self> {
        if src.len() < 8 || read_u32_le(src) != ZSTD_DICTIONARY_MAGIC {
            return Ok(Self::raw(src));
        }

        let mut cursor = 8usize;
        let dictionary_id = read_u32_le(&src[4..8]);

        // A dictionary's decoding table is always built double-symbol, with no
        // cost model: C's `ZSTD_loadDEntropy` calls `HUF_readDTableX2_wksp`
        // unconditionally. It has to commit to one shape, because the literals
        // this table will decode are in blocks it has not seen — and the build
        // cost is paid once per dictionary rather than once per block, which is
        // the term that makes the selector prefer the narrower table.
        //
        // Both arms read the same description and report the same size, which
        // is what lets the cursor advance from either;
        // `the_two_directions_agree_on_where_content_starts` is the test that
        // holds them to it, and stands in for the `debug_assert_eq!` that
        // compared the two sizes back when every parse computed both.
        let mut huffman_table = None;
        let mut huffman_encoding = None;
        let huffman_size = match direction {
            TableDirection::Decoding => read_dtable_x2(
                &src[cursor..],
                HuffmanDTable::double_slot(&mut huffman_table),
            )?,
            TableDirection::Encoding => {
                let mut table = CTableX1::default();
                let (size, repeat_valid) =
                    read_ctable_x1_with_repeat_validity(&src[cursor..], &mut table)?;
                huffman_encoding = Some((table, repeat_valid));
                size
            }
        };
        cursor += huffman_size;

        let (offset_counts, offset_size) = read_sequence_counts(
            &src[cursor..],
            OFFSET_MAX_SYMBOL_VALUE,
            OFFSET_MAX_TABLE_LOG,
        )?;
        cursor += offset_size;

        let (match_length_counts, match_length_size) = read_sequence_counts(
            &src[cursor..],
            MATCH_LENGTH_MAX_SYMBOL_VALUE,
            MATCH_LENGTH_MAX_TABLE_LOG,
        )?;
        cursor += match_length_size;

        let (literal_length_counts, literal_length_size) = read_sequence_counts(
            &src[cursor..],
            LITERAL_LENGTH_MAX_SYMBOL_VALUE,
            LITERAL_LENGTH_MAX_TABLE_LOG,
        )?;
        cursor += literal_length_size;

        let reps_end = cursor.checked_add(12).ok_or(Error::OutputSizeOverflow)?;
        if reps_end > src.len() {
            return Err(Error::Corruption("dictionary is truncated"));
        }
        let content = &src[reps_end..];
        let rep1 = read_u32_le(&src[cursor..cursor + 4]);
        let rep2 = read_u32_le(&src[cursor + 4..cursor + 8]);
        let rep3 = read_u32_le(&src[cursor + 8..reps_end]);
        for rep in [rep1, rep2, rep3] {
            if rep == 0 || rep as usize > content.len() {
                return Err(Error::Corruption(
                    "dictionary repeat offset exceeds dictionary content",
                ));
            }
        }

        let offset_required_max_symbol = dictionary_offset_required_max_symbol(content.len());

        let (decoder_tables, encoder_tables) = match direction {
            TableDirection::Decoding => (
                Some(Arc::new(DecoderTables {
                    huffman_table,
                    offset_table: offset_counts.dtable()?,
                    match_length_table: match_length_counts.dtable()?,
                    literal_length_table: literal_length_counts.dtable()?,
                })),
                None,
            ),
            TableDirection::Encoding => {
                let (huffman_encoding_table, huffman_repeat_valid) = huffman_encoding
                    .expect("the encoding arm above sets this before the cursor advances");
                (
                    None,
                    Some(Arc::new(EncoderTables {
                        huffman_encoding_table,
                        huffman_repeat_valid,
                        offset_encoding_state: offset_counts
                            .encoding_state(offset_required_max_symbol)?,
                        match_length_encoding_state: match_length_counts
                            .encoding_state(MATCH_LENGTH_MAX_SYMBOL_VALUE)?,
                        literal_length_encoding_state: literal_length_counts
                            .encoding_state(LITERAL_LENGTH_MAX_SYMBOL_VALUE)?,
                    })),
                )
            }
        };

        Ok(Self {
            id: dictionary_id,
            source_size: src.len(),
            source: DictionaryBytes::Borrowed(src),
            content_start: src.len() - content.len(),
            raw_content: false,
            decoder_tables,
            encoder_tables,
            repeat_offsets: RepeatOffsets::from_values([rep1, rep2, rep3]),
        })
    }

    /// [`Self::parse`] against bytes this dictionary then owns.
    ///
    /// Parsed through a borrow of `bytes` and rebuilt around it: everything
    /// `parse` produces apart from the source slice is owned already -- the
    /// built tables, the repeat offsets, the content offset -- so the borrow
    /// ends with the destructuring and the buffer moves in behind it.
    fn parse_shared(bytes: Arc<[u8]>, direction: TableDirection) -> Result<Dictionary<'static>> {
        let Dictionary {
            id,
            source_size,
            source: _,
            content_start,
            raw_content,
            decoder_tables,
            encoder_tables,
            repeat_offsets,
        } = Dictionary::parse(&bytes, direction)?;
        Ok(Dictionary {
            id,
            source_size,
            source: DictionaryBytes::Shared(bytes),
            content_start,
            raw_content,
            decoder_tables,
            encoder_tables,
            repeat_offsets,
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn frame_dictionary_id(&self) -> Option<u32> {
        (self.id != 0).then_some(self.id)
    }

    pub(crate) fn source_size(&self) -> usize {
        self.source_size
    }

    pub(crate) fn is_raw_content(&self) -> bool {
        self.raw_content
    }

    pub(crate) fn content(&self) -> &[u8] {
        &self.source.as_slice()[self.content_start..]
    }

    pub(crate) fn matching_content(&self) -> &[u8] {
        if self.is_raw_content() || self.content().is_empty() {
            return self.content();
        }

        let content_start = self.content_start;
        // Upstream copied-CDict extdict windows expose two bytes ahead of the parsed
        // content start. That keeps the source-side virtual indices aligned with the
        // prepared match tables when we reuse them for large formatted dictionaries.
        let match_start = content_start.saturating_sub(2);
        &self.source.as_slice()[match_start..]
    }

    /// The decoder tables, or `None` for a raw-content dictionary, which has
    /// none to build.
    ///
    /// Panics rather than returning `None` when a formatted dictionary was
    /// parsed for the other direction. The two cases are not interchangeable:
    /// absent tables on raw content are correct and decoding proceeds without
    /// them, while absent tables on a formatted dictionary mean a decode path
    /// was handed an [`EncoderDictionary`], and continuing would decode against
    /// no tables instead of the dictionary's own.
    fn decoder_tables(&self) -> Option<&DecoderTables> {
        if self.raw_content {
            return None;
        }
        Some(
            self.decoder_tables
                .as_deref()
                .expect(DECODER_TABLES_MISSING),
        )
    }

    /// The encoder tables, or `None` for a raw-content dictionary. Panics on
    /// the wrong direction, for the reason [`Self::decoder_tables`] gives.
    fn encoder_tables(&self) -> Option<&EncoderTables> {
        if self.raw_content {
            return None;
        }
        Some(
            self.encoder_tables
                .as_deref()
                .expect(ENCODER_TABLES_MISSING),
        )
    }

    pub(crate) fn literals_state(&self) -> LiteralsState {
        LiteralsState::with_huffman_table(
            self.decoder_tables()
                .and_then(|tables| tables.huffman_table),
        )
    }

    pub(crate) fn huffman_encoding_table(&self) -> Option<&CTableX1> {
        self.encoder_tables()
            .map(|tables| &tables.huffman_encoding_table)
    }

    pub(crate) fn huffman_repeat_valid(&self) -> bool {
        self.encoder_tables()
            .is_some_and(|tables| tables.huffman_repeat_valid)
    }

    pub(crate) fn sequence_tables(&self) -> SequenceTablesState {
        match self.decoder_tables() {
            Some(tables) => SequenceTablesState::with_tables(
                Some(tables.literal_length_table.clone()),
                Some(tables.offset_table.clone()),
                Some(tables.match_length_table.clone()),
            ),
            None => SequenceTablesState::with_tables(None, None, None),
        }
    }

    pub(crate) fn sequence_encoding_state(&self) -> SequenceEncodingState {
        match self.encoder_tables() {
            Some(tables) => SequenceEncodingState::with_states(
                Some(tables.literal_length_encoding_state.clone()),
                Some(tables.offset_encoding_state.clone()),
                Some(tables.match_length_encoding_state.clone()),
            ),
            None => SequenceEncodingState::with_states(None, None, None),
        }
    }

    pub(crate) fn repeat_offsets(&self) -> RepeatOffsets {
        self.repeat_offsets
    }

    /// Derive optimal parser pricing frequencies from this dictionary's
    /// Huffman and FSE encoding tables.  Returns `None` for raw dictionaries
    /// or dictionaries without valid encoding tables.
    ///
    /// Matches C's `ZSTD_rescaleFreqs` dictionary initialization path
    /// (zstd_opt.c lines 158-210) where initial frequencies are derived
    /// from the dictionary's actual entropy tables via bit-cost inversion.
    pub(crate) fn optimal_price_seed(&self) -> Option<DictionaryPriceSeed> {
        let tables = self.encoder_tables()?;
        if !tables.huffman_repeat_valid {
            return None;
        }
        let huf = &tables.huffman_encoding_table;
        let ll_state = &tables.literal_length_encoding_state;
        let ml_state = &tables.match_length_encoding_state;
        let of_state = &tables.offset_encoding_state;

        // Literals: scaleLog = 11 (scale to 2K)
        let mut lit_freq = [0u32; 256];
        let mut lit_sum = 0u32;
        for lit in 0u16..256 {
            let bit_cost = huf.symbol_nb_bits(lit as u8) as u32;
            let freq = if bit_cost > 0 {
                1u32 << (11u32.saturating_sub(bit_cost))
            } else {
                1
            };
            lit_freq[lit as usize] = freq;
            lit_sum += freq;
        }

        // LL codes: scaleLog = 10 (scale to 1K)
        let ll_ct = ll_state.fse_ctable();
        let mut ll_freq = [0u32; 36];
        let mut ll_sum = 0u32;
        for ll in 0..36 {
            let bit_cost = ll_ct.max_nb_bits(ll);
            let freq = if bit_cost > 0 {
                1u32 << (10u32.saturating_sub(bit_cost))
            } else {
                1
            };
            ll_freq[ll] = freq;
            ll_sum += freq;
        }

        // ML codes: scaleLog = 10
        let ml_ct = ml_state.fse_ctable();
        let mut ml_freq = [0u32; 53];
        let mut ml_sum = 0u32;
        for ml in 0..53 {
            let bit_cost = ml_ct.max_nb_bits(ml);
            let freq = if bit_cost > 0 {
                1u32 << (10u32.saturating_sub(bit_cost))
            } else {
                1
            };
            ml_freq[ml] = freq;
            ml_sum += freq;
        }

        // OF codes: scaleLog = 10
        let of_ct = of_state.fse_ctable();
        let mut of_freq = [0u32; 32];
        let mut of_sum = 0u32;
        for of in 0..32 {
            let bit_cost = of_ct.max_nb_bits(of);
            let freq = if bit_cost > 0 {
                1u32 << (10u32.saturating_sub(bit_cost))
            } else {
                1
            };
            of_freq[of] = freq;
            of_sum += freq;
        }

        Some(DictionaryPriceSeed {
            lit_freq,
            lit_sum,
            ll_freq,
            ll_sum,
            ml_freq,
            ml_sum,
            of_freq,
            of_sum,
        })
    }

    fn raw(content: &'a [u8]) -> Self {
        Self {
            id: 0,
            source_size: content.len(),
            source: DictionaryBytes::Borrowed(content),
            content_start: 0,
            raw_content: true,
            decoder_tables: None,
            encoder_tables: None,
            repeat_offsets: RepeatOffsets::default(),
        }
    }
}

/// One sequence table's normalized counts, read and validated but not yet
/// built into either direction's table.
///
/// The serialized form is direction-neutral -- it is the same counts whichever
/// table gets built from them -- so reading stops here and the caller builds
/// the one it needs. Keeping the counts rather than the table is what lets
/// both directions share a single parse and a single validation.
struct ParsedCounts {
    normalized: [i16; fse::SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
    table_log: u32,
}

impl ParsedCounts {
    fn dtable(&self) -> Result<fse::DTable> {
        let mut dtable = fse::DTable::default();
        fse::build_dtable(
            &mut dtable,
            &self.normalized,
            self.max_symbol_value,
            self.table_log,
        )?;
        Ok(dtable)
    }

    fn encoding_state(&self, required_max_symbol: u32) -> Result<SequenceEncodingPartState> {
        let ctable = SequenceEncodingTable::from_normalized_counts(
            &self.normalized,
            self.max_symbol_value,
            self.table_log,
        )?;
        Ok(SequenceEncodingPartState::new(
            ctable,
            repeat_mode_for_normalized_counts(
                &self.normalized,
                self.max_symbol_value,
                required_max_symbol,
            ),
        ))
    }
}

fn read_sequence_counts(
    src: &[u8],
    max_symbol_value: u32,
    max_table_log: usize,
) -> Result<(ParsedCounts, usize)> {
    let mut normalized = [0i16; fse::SYMBOLVALUE_MAX + 1];
    let mut max_symbol_value = max_symbol_value;
    let mut table_log = 0u32;
    let consumed = fse::read_ncount(
        &mut normalized,
        &mut max_symbol_value,
        &mut table_log,
        src,
        max_table_log,
    )?;
    Ok((
        ParsedCounts {
            normalized,
            max_symbol_value,
            table_log,
        },
        consumed,
    ))
}

fn dictionary_offset_required_max_symbol(content_len: usize) -> u32 {
    if content_len <= (u32::MAX as usize).saturating_sub(128 * 1024) {
        let max_offset = content_len as u32 + 128 * 1024;
        highbit32(max_offset).min(OFFSET_MAX_SYMBOL_VALUE)
    } else {
        OFFSET_MAX_SYMBOL_VALUE
    }
}

impl std::fmt::Debug for EncoderDictionary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncoderDictionary")
            .field("id", &self.id())
            .field("content_len", &self.as_inner().content().len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DecoderDictionary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderDictionary")
            .field("id", &self.id())
            .field("content_len", &self.as_inner().content().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records with enough shared structure to train a dictionary that actually
    /// carries content, so the tests below are not measuring an empty one.
    fn training_samples() -> Vec<Vec<u8>> {
        (0..64)
            .map(|index| {
                format!(
                    "{{\"region\":\"us-east-{}\",\"tier\":\"gold\",\"amount\":{}}}\n",
                    index % 4,
                    index * 7,
                )
                .into_bytes()
            })
            .collect()
    }

    fn trained_dictionary_bytes() -> Vec<u8> {
        let owned = training_samples();
        let samples: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        crate::train_dictionary(&samples, 4 * 1024)
            .expect("training a dictionary from these samples must succeed")
    }

    fn body_of(len: usize) -> Vec<u8> {
        training_samples()
            .concat()
            .into_iter()
            .cycle()
            .take(len)
            .collect()
    }

    fn options_at(level: i32) -> EncoderOptions {
        EncoderOptions {
            compression_level: crate::CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        }
    }

    /// One `prepare` covers every source size at those options.
    ///
    /// A dictionary's tables are sized from the dictionary's own geometry,
    /// which does not move with the source, so a single cached entry has to
    /// serve inputs of any length. It did not. The key carried
    /// `secondary_hash_bits`, which sizes the *source* side of DoubleFast and
    /// no dictionary table at all, and below the attach cutoff that value
    /// tracks the input size -- so a spread of small inputs keyed several ways
    /// and rebuilt identical tables under each.
    ///
    /// Asserting on the whole key set rather than a count: a stale entry
    /// evicted and replaced by a fresh one would hold the count at 1.
    #[test]
    fn one_preparation_covers_every_source_size() {
        let dictionary = EncoderDictionary::from_shared(trained_dictionary_bytes()).unwrap();
        let options = options_at(5);

        dictionary.prepare(options);
        let prepared = dictionary.cached_geometries();
        assert_eq!(
            prepared.len(),
            1,
            "preparing one level should hold tables for one geometry, got {prepared:?}"
        );

        // Straddling the attach cutoff, which is where the resolved parameters
        // move most: 32 KiB for this level's strategy.
        for size in [1usize, 64, 700, 4096, 8 * 1024, 40_000, 300_000] {
            crate::encode_all_with_prepared_dict_and_options(&body_of(size), &dictionary, options)
                .expect("encoding against a prepared dictionary must succeed");
            assert_eq!(
                dictionary.cached_geometries(),
                prepared,
                "a {size}-byte source wanted a geometry the preparation did not cover"
            );
        }

        // Anti-vacuity. A raw or empty dictionary caches nothing whatever the
        // key looks like, and the loop above would pass on an empty set.
        assert!(
            !dictionary.is_raw_content(),
            "training produced raw content, so no tables were ever built"
        );
    }

    /// Preparing moves when the tables are built and nothing else.
    #[test]
    fn preparing_does_not_change_the_frame() {
        let bytes = trained_dictionary_bytes();
        let body = body_of(20_000);

        for level in [1i32, 3, 5, 9, 12, 19] {
            let options = options_at(level);
            let lazy = EncoderDictionary::new(&bytes).unwrap();
            let eager = EncoderDictionary::new(&bytes).unwrap();
            eager.prepare(options);

            assert_eq!(
                crate::encode_all_with_prepared_dict_and_options(&body, &lazy, options).unwrap(),
                crate::encode_all_with_prepared_dict_and_options(&body, &eager, options).unwrap(),
                "preparing at level {level} changed the frame",
            );
        }
    }

    /// Two levels that resolve to different table geometry each get their own
    /// entry, and preparing one does not serve the other.
    #[test]
    fn preparing_one_level_does_not_cover_a_different_geometry() {
        let dictionary = EncoderDictionary::from_shared(trained_dictionary_bytes()).unwrap();

        dictionary.prepare(options_at(5));
        let after_first = dictionary.cached_geometries();
        assert_eq!(after_first.len(), 1);

        // Level 19 is a binary-tree strategy where level 5 is lazy, so the
        // tables are a different kind, not merely a different size.
        dictionary.prepare(options_at(19));
        let after_second = dictionary.cached_geometries();
        assert_eq!(
            after_second.len(),
            2,
            "a second geometry should be held alongside the first, got {after_second:?}"
        );
        assert!(
            after_second.contains(&after_first[0]),
            "preparing a second level discarded the first level's tables"
        );
    }

    /// Raw content is matched against directly, with none of the built tables a
    /// formatted dictionary gets, so there is nothing to prepare.
    #[test]
    fn preparing_a_raw_dictionary_builds_nothing() {
        let dictionary = EncoderDictionary::new(b"raw dictionary content").unwrap();
        dictionary.prepare(EncoderOptions::default());

        assert!(dictionary.is_raw_content());
        assert!(dictionary.cached_geometries().is_empty());
    }

    /// A shared dictionary is the borrowed one with its buffer moved inside,
    /// so it has to parse to the same thing and compress to the same bytes.
    ///
    /// The byte comparison is the part that matters. `from_shared` rebuilds the
    /// struct field by field around a new source, and a mistake in the content
    /// offset -- the one derived value in that rebuild -- would leave a
    /// dictionary that still parses, still compresses, and quietly matches
    /// against the wrong bytes. Only comparing output catches that.
    #[test]
    fn a_shared_dictionary_matches_the_borrowed_one() {
        // Formatted rather than raw content: raw takes an early return in
        // `matching_content` and skips the offset arithmetic entirely.
        let owned: Vec<Vec<u8>> = (0..64)
            .map(|index| {
                format!(
                    "{{\"region\":\"us-east-{}\",\"tier\":\"gold\",\"amount\":{}}}\n",
                    index % 4,
                    index * 7,
                )
                .into_bytes()
            })
            .collect();
        let samples: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let trained = crate::train_dictionary(&samples, 4 * 1024)
            .expect("training a dictionary from these samples must succeed");
        let bytes = &trained[..];

        let borrowed = EncoderDictionary::new(bytes).unwrap();
        let shared = EncoderDictionary::from_shared(bytes.to_vec()).unwrap();

        assert_eq!(borrowed.id(), shared.id());
        assert_eq!(borrowed.is_raw_content(), shared.is_raw_content());
        assert_eq!(
            borrowed.as_inner().content(),
            shared.as_inner().content(),
            "the content offset did not survive the rebuild"
        );
        assert_eq!(
            borrowed.as_inner().matching_content(),
            shared.as_inner().matching_content(),
            "the two-byte match-start bias did not survive the rebuild"
        );

        let body = samples.concat();
        let from_borrowed = crate::encode_all_with_prepared_dict(&body, &borrowed).unwrap();
        let from_shared = crate::encode_all_with_prepared_dict(&body, &shared).unwrap();
        assert_eq!(
            from_borrowed, from_shared,
            "the same dictionary compressed the same body to different bytes"
        );
        let decoding = DecoderDictionary::from_shared(bytes.to_vec()).unwrap();
        assert_eq!(
            crate::decode_all_with_prepared_dict(&from_shared, &decoding).unwrap(),
            body,
        );

        // Anti-vacuity, both halves. A dictionary that contributed nothing
        // would compare equal here however badly it was rebuilt, and a raw one
        // would take `matching_content`'s early return and never reach the
        // offset arithmetic this is here to check.
        assert!(
            !borrowed.as_inner().content().is_empty(),
            "the trained dictionary carried no content, so nothing above is a test"
        );
        assert!(
            !borrowed.is_raw_content(),
            "training produced raw content, so the content offset was never exercised"
        );
    }

    /// The point of owning the bytes: the dictionary outlives the buffer it was
    /// built from and can cross a thread boundary. This is a compile-time claim
    /// as much as a runtime one -- it would not build if the type still
    /// borrowed.
    #[test]
    // wasip1 without the threads proposal cannot spawn; the `Send + Sync +
    // 'static` half above is where the claim actually lives.
    #[cfg_attr(target_family = "wasm", ignore = "no thread spawn on wasm")]
    fn a_shared_dictionary_is_static_and_sendable() {
        fn assert_static_send_sync<T: Send + Sync + 'static>(_: &T) {}

        let dictionary = {
            let bytes = b"raw shared dictionary content".to_vec();
            EncoderDictionary::from_shared(bytes).unwrap()
        };
        assert_static_send_sync(&dictionary);

        let cloned = dictionary.clone();
        let echoed = std::thread::spawn(move || cloned.as_inner().content().to_vec())
            .join()
            .expect("the dictionary must survive the move onto another thread");
        assert_eq!(echoed, b"raw shared dictionary content");
    }

    #[test]
    fn parses_raw_content_dictionaries() {
        let dictionary = Dictionary::parse(b"raw dictionary", TableDirection::Decoding).unwrap();

        assert_eq!(dictionary.id(), 0);
        assert_eq!(dictionary.content(), b"raw dictionary");
        assert!(dictionary.decoder_tables.is_none());
        assert!(dictionary.is_raw_content());
    }

    #[test]
    fn prepares_raw_content_dictionaries() {
        let dictionary = EncoderDictionary::new(b"raw dictionary").unwrap();

        assert_eq!(dictionary.id(), 0);
        assert!(dictionary.is_raw_content());
    }

    #[test]
    fn rejects_bogus_formatted_dictionaries() {
        let mut dictionary = Vec::new();
        dictionary.extend_from_slice(&ZSTD_DICTIONARY_MAGIC.to_le_bytes());
        dictionary.extend_from_slice(&7u32.to_le_bytes());
        dictionary.push(0x82);
        dictionary.push(0x11);
        dictionary.extend_from_slice(&[0x24, 0x25, 0x25]);
        dictionary.extend_from_slice(&[0x24, 0x25, 0x25]);
        dictionary.extend_from_slice(&[0x24, 0x25, 0x25]);
        dictionary.extend_from_slice(&9u32.to_le_bytes());
        dictionary.extend_from_slice(&4u32.to_le_bytes());
        dictionary.extend_from_slice(&8u32.to_le_bytes());
        dictionary.extend_from_slice(b"content");

        assert!(Dictionary::parse(&dictionary, TableDirection::Decoding).is_err());
        assert!(Dictionary::parse(&dictionary, TableDirection::Encoding).is_err());
    }

    /// Both directions read the same serialized description and must land the
    /// content in the same place. They no longer check each other at runtime:
    /// a single parse used to build both halves and `debug_assert_eq!` the two
    /// Huffman sizes against each other, and splitting the parse took that
    /// comparison away. This is where it went.
    ///
    /// A disagreement here would not fail loudly. It would hand one direction a
    /// content slice offset by the difference, so the encoder would match
    /// against bytes the decoder does not have, and the damage would surface as
    /// a corrupt frame far from its cause.
    #[test]
    fn the_two_directions_agree_on_where_content_starts() {
        let bytes = trained_dictionary_bytes();

        let encoding = Dictionary::parse(&bytes, TableDirection::Encoding).unwrap();
        let decoding = Dictionary::parse(&bytes, TableDirection::Decoding).unwrap();

        assert_eq!(encoding.content_start, decoding.content_start);
        assert_eq!(encoding.content(), decoding.content());
        assert_eq!(encoding.id(), decoding.id());
        assert_eq!(encoding.repeat_offsets(), decoding.repeat_offsets());
        assert!(
            !encoding.content().is_empty(),
            "the fixture must carry content"
        );
    }

    /// The point of the split, asserted structurally rather than by measuring
    /// a heap.
    ///
    /// Before the split every parsed dictionary carried both halves inline:
    /// 22,544 bytes of decoding tables and 11,034 of encoding tables, 33,640
    /// in total, so an encode-only caller held 67% dead weight and a
    /// decode-only caller 33%. Re-fusing the parse would not fail any other
    /// test in this file -- both directions would still be correct, merely
    /// twice the size -- so this is the only thing standing between that
    /// regression and nobody noticing.
    #[test]
    fn each_direction_builds_only_its_own_tables() {
        let bytes = trained_dictionary_bytes();

        let encoding = Dictionary::parse(&bytes, TableDirection::Encoding).unwrap();
        assert!(encoding.encoder_tables.is_some());
        assert!(
            encoding.decoder_tables.is_none(),
            "an encoding parse built decoding tables nothing on that path can read"
        );

        let decoding = Dictionary::parse(&bytes, TableDirection::Decoding).unwrap();
        assert!(decoding.decoder_tables.is_some());
        assert!(
            decoding.encoder_tables.is_none(),
            "a decoding parse built encoding tables nothing on that path can read"
        );

        // Raw content has neither to build, and must not be confused with a
        // formatted dictionary parsed for the other direction -- which is the
        // distinction `raw_content` exists to carry.
        let raw = Dictionary::raw(b"raw content");
        assert!(raw.is_raw_content());
        assert!(raw.encoder_tables.is_none() && raw.decoder_tables.is_none());
        assert!(!encoding.is_raw_content() && !decoding.is_raw_content());
    }

    /// A dictionary must be accepted or refused on what it is, not on which
    /// direction asked.
    ///
    /// **This case list is the weak half of that check and should not be read
    /// as the proof.** It sweeps truncations at the section boundaries and
    /// single-byte corruptions near the front, which is worth having because it
    /// is deterministic and fast, but it reaches none of the entropy-table
    /// shapes where the two builders actually differ: removing the validation
    /// this was first written to guard moved not one of these cases. The oracle
    /// with teeth is `the_dictionary_directions_agree_on_what_parses` in
    /// `tests/property.rs`.
    #[test]
    fn the_two_directions_accept_and_reject_the_same_dictionaries() {
        let valid = trained_dictionary_bytes();

        let mut cases: Vec<Vec<u8>> = vec![
            valid.clone(),
            b"raw content, no magic".to_vec(),
            Vec::new(),
            ZSTD_DICTIONARY_MAGIC.to_le_bytes().to_vec(),
        ];
        // Truncations walk the cursor through every section in turn, so a
        // direction that mis-advances it is caught rather than merely a
        // direction that mis-builds a table.
        for cut in [8, 9, 12, 20, 40, 80, valid.len() / 2, valid.len() - 1] {
            if cut < valid.len() {
                cases.push(valid[..cut].to_vec());
            }
        }
        // Corruptions inside the entropy description, which is the half the
        // two directions read differently.
        for at in [8, 9, 10, 16, 24] {
            if at < valid.len() {
                let mut broken = valid.clone();
                broken[at] ^= 0xff;
                cases.push(broken);
            }
        }

        let mut disagreements = Vec::new();
        for (index, case) in cases.iter().enumerate() {
            let encoding = Dictionary::parse(case, TableDirection::Encoding).is_ok();
            let decoding = Dictionary::parse(case, TableDirection::Decoding).is_ok();
            if encoding != decoding {
                disagreements.push(format!(
                    "case {index} ({} bytes): encoding accepted={encoding}, decoding accepted={decoding}",
                    case.len()
                ));
            }
        }

        assert!(
            disagreements.is_empty(),
            "the two directions disagreed on {} of {} dictionaries:\n{}",
            disagreements.len(),
            cases.len(),
            disagreements.join("\n"),
        );
    }
}
