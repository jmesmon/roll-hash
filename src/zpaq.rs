use std::fmt;
use std::num::Wrapping;
use std::convert::TryInto;

use crate::{Chunk, ChunkIncr, RangeExt, Splitter};
use std::ops::Bound;
use std::ops::RangeBounds;

/**
 * A splitter used in go 'dedup' and zpaq that does not require looking back in the source
 * data to update
 *
 * PDF: ??
 *
 * Note: go-dedup & zpaq calculate the relationship between their parameters slightly differently.
 * We support both of these (via the seperate with_*() constructors, but it'd be nice to clarify
 * why they differ and what affect the differences have.
 *
 * References:
 *
 *  - http://encode.ru/threads/456-zpaq-updates?p=45192&viewfull=1#post45192
 *  - https://github.com/klauspost/dedup/blob/master/writer.go#L668, 'zpaqWriter'
 *  - https://github.com/zpaq/zpaq/blob/master/zpaq.cpp
 *
 * Parameters:
 *
 *  - fragment (aka average_size_base_2): average size = 2**fragment KiB
 *      in Zpaq (the compressor), this defaults to 6
 *  - min_size, max_size: additional bounds on the blocks. Not technically needed for the algorithm
 *      to function
 *
 *  In Zpaq-compressor, min & max size are calculated using the fragment value
 *  In go's dedup, fragment is calculated using a min & max size
 *
 * In-block state:
 *
 *  - hash: u32, current hash
 *  - last_byte: u8, previous byte read
 *  - predicted_byte: array of 256 u8's.
 *
 * Between-block state:
 *
 *  - None
 */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zpaq {
    range: (Bound<u64>, Bound<u64>),
    fragment: u8,
    max_hash: u32,
}

impl Zpaq {
    /* this is taken from go-dedup */
    fn fragment_ave_from_max(max: u64) -> u8 {
        /* TODO: convert this to pure integer math */
        (max as f64 / (64f64 * 64f64)).log2() as u8
    }

    /* these are based on the zpaq (not go-dedup) calculations */
    fn fragment_ave_from_range<T: RangeBounds<u64>>(range: T) -> u8 {
        let v = match range.end_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => *i - 1,
            Bound::Unbounded => {
                /* try to guess based on first */
                64 * match range.start_bound() {
                    Bound::Included(i) => *i,
                    Bound::Excluded(i) => *i + 1,
                    Bound::Unbounded => {
                        /* welp, lets use the default */
                        return 6;
                    }
                }
            }
        };

        Self::fragment_ave_from_max(v)
    }

    /* these are based on the zpaq (not go-dedup) calculations */
    fn range_from_fragment_ave(fragment_ave: u8) -> impl RangeBounds<u64> {
        assert!(fragment_ave <= 22);
        64 << fragment_ave..8128 << fragment_ave
    }

    fn range_from_max(max: u64) -> impl RangeBounds<u64> {
        max / 64..max
    }

    fn max_hash_from_fragment_ave(fragment_ave: u8) -> u32 {
        1 << (22 - fragment_ave)
        /*
         * go-dedup does this:
         * (22f64 - fragment_ave).exp2() as u32
         *
         * Which should be equivalent to the integer math above (which is used by zpaq).
         */
    }

    /**
     * Create a splitter using the range of output block sizes.
     *
     * The average block size will be the max block size (if any) divided by 4, using the same
     * algorithm to calculate it as go-dedup.
     */
    pub fn with_range(range: impl RangeBounds<u64> + Clone) -> Self {
        let f = Self::fragment_ave_from_range(range.clone());
        Self::with_average_and_range(f, range)
    }

    /**
     * Create a splitter using the defaults from Zpaq (the compressor) given a average size is base
     * 2 (zpaq argument "-fragment")
     */
    pub fn with_average_size(average_size_base_2: u8) -> Self {
        let r = Self::range_from_fragment_ave(average_size_base_2);
        Self::with_average_and_range(average_size_base_2, r)
    }

    /**
     * Use the defaults from go-dedup to generate a splitter given the max size of a split.
     *
     * The average block size will be the max block size (if any) divided by 4, using the same
     * algorithm to calculate it as go-dedup.
     */
    pub fn with_max_size(max: u64) -> Self {
        Self::with_average_and_range(Self::fragment_ave_from_max(max), Self::range_from_max(max))
    }

    /**
     * Create a splitter with control of all parameters
     *
     * All the other constructors use this internally
     */
    pub fn with_average_and_range(average_size_base_2: u8, range: impl RangeBounds<u64>) -> Self {
        Zpaq {
            range: range.into_tuple(),
            fragment: average_size_base_2,
            max_hash: Self::max_hash_from_fragment_ave(average_size_base_2),
        }
    }

    fn average_block_size(&self) -> u64 {
        /* I don't know If i really trust this, do some more confirmation */
        1024 << self.fragment
    }

    fn split_here(&self, hash: u32, index: u64) -> bool {
        (hash < self.max_hash && !self.range.under_min(&index)) || self.range.exceeds_max(&index)
    }
}

impl Default for Zpaq {
    /**
     * Create a splitter using the defaults from Zpaq (the compressor)
     *
     * Average size is 65536 bytes (64KiB), max is 520192 bytes (508KiB), min is 4096 bytes (4KiB)
     */
    fn default() -> Self {
        Self::with_average_size(6)
    }
}

#[derive(Debug)]
pub struct ZpaqIncr {
    params: Zpaq,
    state: ZpaqHash,
    idx: u64,
}

#[derive(Default, Debug)]
pub struct ZpaqSearchState {
    state: ZpaqHash,
    offset: usize,
}

impl ZpaqSearchState {
    fn feed(&mut self, v: u8) -> u32 {
        self.offset += 1;
        self.state.feed(v)
    }
}

impl Chunk for Zpaq {
    type SearchState = ZpaqSearchState;
    type Incr = ZpaqIncr;

    fn find_chunk_edge(
        &self,
        state: Option<Self::SearchState>,
        data: &[u8],
    ) -> Result<usize, Self::SearchState> {
        let mut hs = match state {
            Some(v) => v,
            None => Self::SearchState::default(),
        };

        for i in hs.offset..data.len() {
            let h = hs.feed(data[i]);
            if self.split_here(h, (i + 1) as u64) {
                return Ok(i + 1);
            }
        }

        hs.offset = data.len();
        Err(hs)
    }

    fn incrimental(&self) -> Self::Incr {
        From::from(self.clone())
    }
}

impl ZpaqIncr {
    fn feed(&mut self, v: u8) -> u32 {
        self.idx += 1;
        self.state.feed(v) 
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.state = Default::default();
    }
}

impl ChunkIncr for ZpaqIncr {
    fn push(&mut self, data: &[u8]) -> Option<usize> {
        for (i, &v) in data.iter().enumerate() {
            let h = self.feed(v);
            if self.params.split_here(h, self.idx) {
                self.reset();
                return Some(i + 1);
            }
        }

        None
    }
}

impl From<Zpaq> for ZpaqIncr {
    fn from(params: Zpaq) -> Self {
        Self {
            params,
            state: Default::default(),
            idx: 0,
        }
    }
}

impl Splitter for Zpaq {
    fn find_chunk_edge<'b>(&self, data: &'b [u8]) -> usize {
        let mut s = ZpaqHash::default();
        let mut l = 0;
        for (i, &v) in data.iter().enumerate() {
            if self.split_here(s.feed(v), (i + 1) as u64) {
                l = i + 1;
                break;
            }
        }

        l
    }

    fn next_iter<T: Iterator<Item = u8>>(&self, iter: T) -> Option<Vec<u8>> {
        let a = self.average_block_size();
        /* FIXME: ideally we'd allocate enough capacity to contain a large percentage of the
         * blocks. Just doing average probably will net us ~50% of blocks not needing additional
         * allocation. We really need to know the PDF (and standard-deviation) to make a better
         * prediction here. That said, even with additional data, this is a trade off with extra
         * space consumed vs number of allocations/reallocations
         */
        let mut w = Vec::with_capacity((a + a / 2).try_into().unwrap());
        let mut s = ZpaqHash::default();
        for v in iter {
            w.push(v);
            if self.split_here(s.feed(v), w.len() as u64) {
                return Some(w);
            }
        }

        if w.is_empty() {
            None
        } else {
            Some(w)
        }
    }
}

/**
 * The rolling hash component of the zpaq splitter
 */
#[derive(Clone)]
pub struct ZpaqHash {
    hash: Wrapping<u32>,
    last_byte: u8,
    predicted_byte: [u8; 256],
}

impl PartialEq for ZpaqHash {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.last_byte == other.last_byte &&
            &self.predicted_byte[..] == &other.predicted_byte[..]
    }
}

impl Eq for ZpaqHash {}

impl fmt::Debug for ZpaqHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("ZpaqHash")
            .field("hash", &self.hash)
            .field("last_byte", &self.last_byte)
            .field("predicted_byte", &fmt_extra::Hs(&self.predicted_byte[..]))
            .finish()
    }
}

impl Default for ZpaqHash {
    fn default() -> Self {
        ZpaqHash {
            hash: Wrapping(0),
            last_byte: 0,
            predicted_byte: [0; 256],
        }
    }
}

impl ZpaqHash {
    /*
     * we can only get away with this because Zpaq doesn't need to look at old data to make it's
     * splitting decision, it only examines it's state + current value (and the state is
     * relatively large, but isn't a window into past data).
     */
    fn feed(&mut self, c: u8) -> u32 {
        self.hash = if c == self.predicted_byte[self.last_byte as usize] {
            (self.hash + Wrapping(c as u32) + Wrapping(1)) * Wrapping(314159265)
        } else {
            (self.hash + Wrapping(c as u32) + Wrapping(1)) * Wrapping(271828182)
        };

        self.predicted_byte[self.last_byte as usize] = c;
        self.last_byte = c;
        self.hash.0
    }
}
