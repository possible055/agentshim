use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rustc_hash::FxHashMap;
use sha2::{Digest as _, Sha256};

use crate::{CounterLimits, O200kCounter};

const RANKS: &[u8; 3_613_922] = include_bytes!("../assets/o200k_base.tiktoken");
const RANKS_SHA256: [u8; 32] = [
    0x44, 0x6a, 0x95, 0x38, 0xcb, 0x6c, 0x34, 0x8e, 0x35, 0x16, 0x12, 0x0d, 0x7c, 0x08, 0xb0, 0x9f,
    0x57, 0xc3, 0x64, 0x95, 0xe2, 0xac, 0xff, 0xfe, 0x59, 0xa5, 0xbf, 0x8b, 0x0c, 0xfb, 0x1a, 0x2d,
];
const MERGEABLE_RANKS: usize = 199_998;

pub(crate) struct Model {
    pub(crate) merges: FxHashMap<(u32, u32), u32>,
    pub(crate) byte_ranks: [u32; 256],
    pub(crate) max_token_bytes: usize,
}

/// Immutable `o200k_base` tables shared by every counter.
#[derive(Clone)]
pub struct O200kPrototype {
    pub(crate) model: Arc<Model>,
}

impl O200kPrototype {
    /// Load and verify the embedded `o200k_base` mergeable ranks.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded digest, dense rank sequence, base64,
    /// byte vocabulary, or reconstructed merge graph is invalid.
    pub fn load_embedded() -> Result<Self, LoadError> {
        Self::load(RANKS)
    }

    pub(crate) fn load(ranks: &[u8]) -> Result<Self, LoadError> {
        if Sha256::digest(ranks).as_slice() != RANKS_SHA256 {
            return Err(LoadError::RanksHash);
        }
        let vocabulary = parse_ranks(ranks)?;
        let model = reconstruct_model(&vocabulary)?;
        Ok(Self {
            model: Arc::new(model),
        })
    }

    /// Create one counter with independent bounded cache and scratch state.
    ///
    /// # Errors
    ///
    /// Returns an error when a counter bound is zero or the short-cache slot
    /// count is not a power of two.
    pub fn fork_counter(&self, limits: CounterLimits) -> Result<O200kCounter, LoadError> {
        limits.validate()?;
        Ok(O200kCounter::new(Arc::clone(&self.model), limits))
    }
}

fn parse_ranks(ranks: &[u8]) -> Result<Vec<Vec<u8>>, LoadError> {
    let text = std::str::from_utf8(ranks).map_err(|_| LoadError::RanksUtf8)?;
    let mut vocabulary = Vec::with_capacity(MERGEABLE_RANKS);
    for (line_number, line) in text.lines().enumerate() {
        let (encoded, rank) = line
            .split_once(' ')
            .ok_or(LoadError::InvalidRankLine(line_number))?;
        let rank = rank
            .trim()
            .parse::<usize>()
            .map_err(|_| LoadError::InvalidRankLine(line_number))?;
        if rank != line_number {
            return Err(LoadError::NonDenseRank {
                line: line_number,
                rank,
            });
        }
        let token = BASE64_STANDARD
            .decode(encoded)
            .map_err(|_| LoadError::InvalidBase64(line_number))?;
        if token.is_empty() {
            return Err(LoadError::EmptyToken(line_number));
        }
        vocabulary.push(token);
    }
    if vocabulary.len() != MERGEABLE_RANKS {
        return Err(LoadError::RankCount(vocabulary.len()));
    }
    Ok(vocabulary)
}

fn reconstruct_model(vocabulary: &[Vec<u8>]) -> Result<Model, LoadError> {
    let mut inverse = FxHashMap::default();
    for (rank, token) in vocabulary.iter().enumerate() {
        let rank = u32::try_from(rank).map_err(|_| LoadError::RankOverflow)?;
        if inverse.insert(token.as_slice(), rank).is_some() {
            return Err(LoadError::DuplicateToken(rank));
        }
    }

    let mut byte_ranks = [u32::MAX; 256];
    for byte in u8::MIN..=u8::MAX {
        byte_ranks[usize::from(byte)] = *inverse
            .get(std::slice::from_ref(&byte))
            .ok_or(LoadError::MissingByte(byte))?;
    }

    let mut merges = FxHashMap::default();
    let mut symbols = Vec::new();
    let mut max_token_bytes = 1;
    for (rank, token) in vocabulary.iter().enumerate() {
        max_token_bytes = max_token_bytes.max(token.len());
        if token.len() < 2 {
            continue;
        }
        symbols.clear();
        symbols.extend(token.iter().map(|byte| byte_ranks[usize::from(*byte)]));
        merge_symbols(&mut symbols, &merges);
        if symbols.len() != 2 {
            return Err(LoadError::InvalidMerge(rank));
        }
        let rank = u32::try_from(rank).map_err(|_| LoadError::RankOverflow)?;
        if merges.insert((symbols[0], symbols[1]), rank).is_some() {
            return Err(LoadError::DuplicateMerge(rank));
        }
    }

    Ok(Model {
        merges,
        byte_ranks,
        max_token_bytes,
    })
}

fn merge_symbols(symbols: &mut Vec<u32>, merges: &FxHashMap<(u32, u32), u32>) {
    loop {
        let Some((index, merged_rank)) = symbols
            .windows(2)
            .enumerate()
            .filter_map(|(index, pair)| merges.get(&(pair[0], pair[1])).map(|rank| (index, *rank)))
            .min_by_key(|(index, rank)| (*rank, *index))
        else {
            return;
        };
        symbols[index] = merged_rank;
        symbols.remove(index + 1);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("embedded o200k_base ranks SHA-256 does not match the pinned digest")]
    RanksHash,
    #[error("o200k_base ranks are not UTF-8")]
    RanksUtf8,
    #[error("invalid o200k_base rank line {0}")]
    InvalidRankLine(usize),
    #[error("o200k_base rank {rank} is out of sequence at line {line}")]
    NonDenseRank { line: usize, rank: usize },
    #[error("invalid base64 at o200k_base rank {0}")]
    InvalidBase64(usize),
    #[error("o200k_base rank {0} is empty")]
    EmptyToken(usize),
    #[error("o200k_base has {0} mergeable ranks instead of 199998")]
    RankCount(usize),
    #[error("o200k_base rank does not fit u32")]
    RankOverflow,
    #[error("o200k_base contains duplicate token rank {0}")]
    DuplicateToken(u32),
    #[error("o200k_base is missing the single-byte token {0}")]
    MissingByte(u8),
    #[error("o200k_base rank {0} does not reconstruct to one merge pair")]
    InvalidMerge(usize),
    #[error("o200k_base contains duplicate merge rank {0}")]
    DuplicateMerge(u32),
    #[error("short_cache_slots must be a nonzero power of two")]
    InvalidShortCacheSlots,
    #[error("counter limits must be nonzero")]
    InvalidCounterLimits,
}
