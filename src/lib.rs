#[macro_use]
extern crate lazy_static;
extern crate blas;
extern crate byteorder;
extern crate fst;
extern crate libc;
extern crate openblas_src;
#[macro_use]
extern crate serde_derive;
extern crate flate2;
extern crate fnv;
extern crate fs2;
extern crate memmap;
extern crate num;
extern crate pbr;
extern crate rand;
extern crate rayon;
extern crate regex;
extern crate serde_json;

use fnv::{FnvHashMap, FnvHashSet};
use std::cmp::{Ordering, PartialOrd};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};
use fst::raw::{Fst, MmapReadOnly};
use fst::Map;
use memmap::Mmap;

use fst::Error;

#[macro_use]
pub mod util;
pub mod builder;
pub mod config;
pub mod merge;
pub mod ngrams;
pub mod shard;
pub mod stopwords;
pub mod stringvec;
pub mod synonyms;
pub mod toponyms;
pub mod word_vec;

use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use util::{BRED, BYELL, ECOL};
use word_vec::WordVecs;

macro_rules! make_static_var_and_getter {
    ($fn_name: ident, $var_name: ident, $t: ty) => {
        static mut $var_name: Option<$t> = None;
        #[inline]
        fn $fn_name() -> &'static $t {
            unsafe {
                match $var_name {
                    Some(ref n) => n,
                    None => std::process::exit(1),
                }
            }
        }
    };
}

macro_rules! impl_partial_ord {
    ($struct: ty, $eq_var: ident, $cmp_var: ident) => {
        impl PartialOrd for $struct {
            #[inline]
            fn partial_cmp(&self, other: &$struct) -> Option<Ordering> {
                if self.eq(&other) {
                    self.$eq_var.partial_cmp(&other.$eq_var)
                } else {
                    self.$cmp_var.partial_cmp(&other.$cmp_var)
                }
            }
        }

        impl PartialEq for $struct {
            #[inline]
            fn eq(&self, other: &$struct) -> bool {
                self.$cmp_var == other.$cmp_var
            }
        }
    };
}

pub const DIST_THRESH: f32 = 0.951; // take only queries with smaller distance [0, 1]
pub const FETCH_MIN: usize = 200; // get at least this many keyword matched results

make_static_var_and_getter!(_get_shard_size, SHARD_SIZE, usize);

#[inline]
fn read_bucket(mmap: &memmap::Mmap, addr: usize, len: usize, id_size: usize) -> Vec<(u32, u8, u8)> {
    let buf = &mmap[addr..addr + len * id_size];
    (0..len)
        .map(|i| {
            let j = i * id_size;
            (
                LittleEndian::read_u32(&buf[j..j + 4]),
                buf[j + 4],
                buf[j + 5],
            )
        })
        .collect::<Vec<(u32, u8, u8)>>()
}

// reading part
#[inline]
fn get_addr_and_len(ngram: &str, map: &fst::Map) -> Option<(u64, u64)> {
    match map.get(ngram) {
        Some(val) => return Some(util::elegant_pair_inv(val)),
        None => return None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Distance {
    pub query_id: u64,
    pub keyword: f32,
    pub cosine: Option<f32>,
}

impl PartialOrd for Distance {
    #[inline]
    fn partial_cmp(&self, other: &Distance) -> Option<Ordering> {
        if self.eq(&other) {
            self.query_id.partial_cmp(&other.query_id)
        } else {
            if self.cosine == None || other.cosine == None {
                self.keyword.partial_cmp(&other.keyword)
            } else {
                self.cosine.partial_cmp(&other.cosine)
            }
        }
    }
}

impl PartialEq for Distance {
    #[inline]
    fn eq(&self, other: &Distance) -> bool {
        if self.cosine == None || other.cosine == None {
            self.keyword == other.keyword
        } else {
            self.cosine == other.cosine
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DistanceResult {
    pub query: String,
    pub dist: Distance,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchShardResult {
    pub query_id: u64,       // query id unique globally
    pub shard_id: u8,        // id of the _query_ shard (i2q, not ngram shard)
    pub shard_query_id: u32, // query id unique on a shard level
    pub ngram_idx: usize,    // index of an i-th ngram in a query
    pub ngram_rel: f32,      // relevance of an i-th ngram: [∑₁_ₙ (query_word_relₖ)] * IDFᵢ
    pub weight_rel: f32,     // weight coefficient for a word relevance, relative to the query
    pub query_ngram_rel: f32,
    pub ngram: String,
    pub query: Option<String>,
}

impl SearchShardResult {
    #[inline]
    pub fn new(
        shard_id: u8,
        shard_query_id: u32,
        shard_num: usize,
        ngram_rel: u8,
        query_ngram_rel: f32,
        ngram_idx: usize,
        ngram: String,
        query: Option<String>,
        with_tfidf: bool,
    ) -> Self {
        let query_id = util::shard_id_2_query_id(shard_query_id as u64, shard_id, shard_num);
        let ngram_rel = ngram_rel as f32 / 100.0;

        let weight_rel: f32;
        if with_tfidf {
            weight_rel = util::min(ngram_rel, query_ngram_rel) / query_ngram_rel;
        } else {
            weight_rel = 1.0;
        }

        SearchShardResult {
            query_id: query_id,
            shard_id: shard_id,
            shard_query_id: shard_query_id,
            ngram_rel: ngram_rel,
            weight_rel: weight_rel,
            query_ngram_rel: query_ngram_rel,
            ngram_idx: ngram_idx,
            ngram: ngram,
            query: query,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub query_id: u64, // query id unique globally
    pub query: Option<String>,
    pub dist: Distance,
}
impl_partial_ord!(SearchResult, query_id, dist);

struct ShardResults {
    results: Vec<SearchShardResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeywordMatchResult {
    pub query_id: u64, // query id unique globally
    pub dist: f32,
}
impl_partial_ord!(KeywordMatchResult, query_id, dist);

#[inline]
fn _get_idfs(ngrams: &Vec<(String, usize)>, map: &fst::Map) -> FnvHashMap<String, (usize, f32)> {
    let mut idfs: FnvHashMap<String, (usize, f32)> = FnvHashMap::default();
    let n = *_get_shard_size() as f32;
    for (ngram, ngram_idx) in ngrams {
        // IDF score for the ngram
        let idf: f32;
        match get_addr_and_len(ngram, &map) {
            // returns physical memory address and length of the vector (not a number of bytes)
            Some((_addr, len)) => {
                // IDF for existing ngram
                idf = (n / len as f32).log(2.0);
            }
            None => {
                // IDF ngram that occurs for the 1st time
                idf = n.log(2.0);
            }
        }
        idfs.insert(ngram.to_string(), (*ngram_idx, idf));
    }

    return idfs;
}

#[inline]
fn get_shard_results(
    ngrams: &Vec<(String, usize)>,
    trs: &Vec<f32>,
    map: &fst::Map,
    ifd: &memmap::Mmap,
    id_size: usize,
    shard_num: usize,
    with_tfidf: bool,
) -> Result<ShardResults, Error> {
    let mut sres: Vec<SearchShardResult> = vec![];
    for (ngram, ngram_idx) in ngrams {
        if let Some((addr, len)) = get_addr_and_len(ngram, &map) {
            // returns physical memory address and length of the vector (not a number of bytes)
            let mem_addr = addr as usize * id_size;

            for &(shard_query_id, shard_id, ngram_rel) in
                read_bucket(ifd, mem_addr, len as usize, id_size).iter()
            {
                sres.push(SearchShardResult::new(
                    shard_id,
                    shard_query_id,
                    shard_num,
                    ngram_rel,
                    trs[*ngram_idx],
                    *ngram_idx,
                    ngram.to_string(),
                    None,
                    with_tfidf,
                ));
            }
        }
    }

    Ok(ShardResults { results: sres })
}

#[inline]
fn index_words(
    words: &Vec<String>,
    synonyms: &FnvHashMap<usize, String>,
) -> (FnvHashMap<String, usize>, FnvHashSet<String>) {
    let mut words_set: FnvHashSet<String> = FnvHashSet::default();
    let mut words_index = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            words_set.insert(w.to_string());

            (w.to_string(), i)
        })
        .collect::<FnvHashMap<String, usize>>();

    for (word_idx, syn) in synonyms {
        // add synonyms to the index, but not to the words set
        words_index.insert(syn.to_string(), *word_idx);
    }

    (words_index, words_set)
}

pub struct Qpick<'a> {
    path: String,
    config: config::Config,
    synonyms: Option<FnvHashMap<String, String>>,
    toponyms: Option<fst::Set>,
    stopwords: FnvHashSet<String>,
    terms_relevance: fst::Map,
    shards: Arc<Vec<Shard>>,
    shard_range: Range<u32>,
    id_size: usize,
    i2q_loaded: bool,
    shard_num: usize,
    word_vecs: Option<WordVecs<'a>>,
}

pub struct Shard {
    map: fst::Map,
    shard: Mmap,
    i2q: Option<stringvec::StrVec>,
}

#[derive(Debug)]
pub struct DistResults {
    pub items_iter: std::vec::IntoIter<DistanceResult>,
}

impl DistResults {
    pub fn new(items_iter: std::vec::IntoIter<DistanceResult>) -> DistResults {
        DistResults {
            items_iter: items_iter,
        }
    }

    pub fn next(&mut self) -> Option<DistanceResult> {
        <std::vec::IntoIter<DistanceResult> as std::iter::Iterator>::next(&mut self.items_iter)
    }
}

#[derive(Debug)]
pub struct SearchResults {
    pub items_iter: std::vec::IntoIter<SearchResult>,
}

impl SearchResults {
    pub fn new(items_iter: std::vec::IntoIter<SearchResult>) -> SearchResults {
        SearchResults {
            items_iter: items_iter,
        }
    }

    pub fn next(&mut self) -> Option<SearchResult> {
        <std::vec::IntoIter<SearchResult> as std::iter::Iterator>::next(&mut self.items_iter)
    }
}

impl<'a> Qpick<'a> {
    fn new(path: String, shard_range_opt: Option<Range<u32>>) -> Qpick<'a> {
        let c = config::Config::init(path.clone());
        let id_size = c.id_size;
        unsafe {
            SHARD_SIZE = Some(c.shard_size);
        }

        let shard_num = c.nr_shards;
        let shard_range = shard_range_opt.unwrap_or(0..c.nr_shards as u32);

        let stopwords_path = &format!("{}/{}", path, c.stopwords_file);
        let stopwords = match stopwords::load(stopwords_path) {
            Ok(stopwords) => stopwords,
            Err(_) => panic!([
                BYELL,
                "No such file or directory: ",
                ECOL,
                BRED,
                stopwords_path,
                ECOL
            ]
            .join("")),
        };

        let synonyms_path = PathBuf::from(&path).join(&c.synonyms_file);
        let synonyms = synonyms::load(&synonyms_path);

        let toponyms_path = PathBuf::from(&path).join(&c.toponyms_file);
        let toponyms = toponyms::load(&toponyms_path);

        let terms_relevance_path = &format!("{}/{}", path, c.terms_relevance_file);
        let terms_relevance = match Map::from_path(terms_relevance_path) {
            Ok(terms_relevance) => terms_relevance,
            Err(_) => panic!([
                BYELL,
                "No such file or directory: ",
                ECOL,
                BRED,
                terms_relevance_path,
                ECOL
            ]
            .join("")),
        };

        let shard_indexes: Vec<u32> = (shard_range.start..shard_range.end).collect();
        let shards: Vec<(bool, Shard)> = shard_indexes
            .par_iter()
            .map(|i| {
                let map_path = format!("{}/map.{}", path, i);

                // advice OS on random access to the map file and create Fst object from it
                let map_file = MmapReadOnly::open_path(&map_path).unwrap();
                unsafe {
                    util::advise_ram(map_file.as_slice())
                        .expect(&format!("Advisory failed for map {}", i))
                };
                let map = match Fst::from_mmap(map_file) {
                    Ok(fst) => Map::from(fst),
                    Err(_) => panic!("Failed to load index map: {}!", &map_path),
                };

                let shard_name = format!("{}/shard.{}", path, i);
                let shard_file = OpenOptions::new().read(true).open(shard_name).unwrap();
                let shard = unsafe { Mmap::map(&shard_file).unwrap() };

                util::advise_ram(&shard[..]).expect(&format!("Advisory failed for shard {}", i));

                let i2q_path = PathBuf::from(&path).join(&format!("{}.{}", c.i2q_file, i));
                let i2q = if i2q_path.is_file() {
                    Some(stringvec::StrVec::load(&i2q_path))
                } else {
                    None
                };

                (
                    !i2q.is_none(),
                    Shard {
                        shard: shard,
                        map: map,
                        i2q: i2q,
                    },
                )
            })
            .collect();

        let i2q_loaded = shards
            .iter()
            .fold(true, |b, (is_loaded, _)| b && *is_loaded);
        let shards = shards.into_iter().map(|(_, s)| s).collect();

        let mut word_vecs = None;
        if c.use_word_vectors {
            let words_path = PathBuf::from(&c.words_file);
            let word_vecs_path = PathBuf::from(&c.word_vecs_file);
            if words_path.is_file() && word_vecs_path.is_file() {
                word_vecs = Some(WordVecs::load(&words_path, &word_vecs_path));
            }
        }

        Qpick {
            config: c,
            path: path,
            synonyms: synonyms,
            toponyms: toponyms,
            stopwords: stopwords,
            terms_relevance: terms_relevance,
            shards: Arc::new(shards),
            shard_range: shard_range,
            id_size: id_size,
            i2q_loaded: i2q_loaded,
            shard_num: shard_num,
            word_vecs: word_vecs,
        }
    }

    pub fn i2q_is_loaded(&self) -> bool {
        self.i2q_loaded
    }

    pub fn from_path(path: String) -> Self {
        Qpick::new(path, None)
    }

    pub fn from_path_with_shard_range(path: String, shard_range: Range<u32>) -> Self {
        Qpick::new(path, Some(shard_range))
    }

    #[inline]
    fn shard_ngrams(&self, ngrams: &Vec<String>) -> FnvHashMap<usize, Vec<(String, usize)>> {
        let mut shards_ngrams: FnvHashMap<usize, Vec<(String, usize)>> = FnvHashMap::default();
        for (ngram_idx, ngram) in ngrams.iter().enumerate() {
            let shard_id = util::jump_consistent_hash_str(ngram, self.config.nr_shards as u32);

            if shard_id >= self.shard_range.end || shard_id < self.shard_range.start {
                continue;
            }

            let sh_ngrams = shards_ngrams.entry(shard_id as usize).or_insert(vec![]);
            sh_ngrams.push((ngram.to_string(), ngram_idx));
        }

        return shards_ngrams;
    }

    fn get_matches(
        &self,
        ngrams: Vec<String>,
        trs: Vec<f32>,
        ngrams_ids: FnvHashMap<String, Vec<usize>>,
        words: Vec<String>,
        wrs: Vec<f32>,
        must_have: Vec<usize>,
        synonyms: FnvHashMap<usize, String>,
        count: Option<usize>,
        with_tfidf: bool,
    ) -> Result<Vec<SearchResult>, Error> {
        let shard_ngrams = self.shard_ngrams(&ngrams);
        let shard_results: Vec<ShardResults> = shard_ngrams
            .iter()
            .map(|(shard_id, ngrams)| {
                get_shard_results(
                    ngrams,
                    &trs,
                    &self.shards[*shard_id].map,
                    &self.shards[*shard_id].shard,
                    self.id_size,
                    self.shard_num,
                    with_tfidf,
                )
                .unwrap()
            })
            .collect();

        // query_id -> (shard_query_id, shard_id)
        let mut ids_map: HashMap<u64, (u32, u8)> = HashMap::new();

        // query_id -> [ngram_rel_0, ngram_rel_1, ..., ngram_rel_n]
        let vec_len = words.len();
        let mut res_data: FnvHashMap<u64, Vec<f32>> = FnvHashMap::default();
        for sh_res in shard_results.iter() {
            for r in sh_res.results.iter() {
                let ref mut words_rel_vec =
                    *res_data.entry(r.query_id).or_insert(vec![0.0; vec_len]);

                for word_idx in ngrams_ids.get(&r.ngram).unwrap_or(&vec![]) {
                    if words_rel_vec[*word_idx] == 0.0 {
                        words_rel_vec[*word_idx] = wrs[*word_idx] * r.weight_rel;
                    }
                }

                ids_map
                    .entry(r.query_id)
                    .or_insert((r.shard_query_id, r.shard_id));
            }
        }

        let mut keyword_matches: Vec<KeywordMatchResult> = res_data
            .into_iter()
            .filter(|(_, words_rel_vec)| {
                must_have.is_empty() || must_have.iter().all(|i| words_rel_vec[*i] > 0.0)
            })
            .map(|(query_id, words_rel_vec)| {
                let similarity = words_rel_vec.iter().fold(0.0, |mut sum, &x| {
                    sum += x;
                    sum
                });

                (query_id, util::max(1.0 - similarity, 0.0), words_rel_vec)
            })
            .filter(|(_, dist, _)| *dist < DIST_THRESH)
            .map(|(query_id, dist, _)| KeywordMatchResult {
                query_id: query_id,
                dist: dist,
            })
            .collect::<Vec<KeywordMatchResult>>();
        keyword_matches.sort_by(|a, b| a.partial_cmp(&b).unwrap_or(Ordering::Less));

        let (words_index, words_set) = index_words(&words, &synonyms);

        let cand_synonyms: FnvHashMap<String, String> = synonyms
            .iter()
            .map(|(wid, syn)| (syn.to_string(), words[*wid].to_string()))
            .collect();

        let mut search_results: Vec<SearchResult> = keyword_matches
            .into_iter()
            .take(util::max(count.unwrap_or(FETCH_MIN), FETCH_MIN))
            .map(|m| {
                let (sh_qid, sh_id) = ids_map.get(&m.query_id).unwrap();
                let cand_query = self.shards[*sh_id as usize]
                    .i2q
                    .as_ref()
                    .map(|i2q| i2q[*sh_qid as usize].to_string())
                    .unwrap_or(String::from(""));

                let (cand_words, match_words, miss_words, excess_words) =
                    ngrams::match_queries(&cand_query, &words_set, &cand_synonyms);

                // check excess words and update keyword score
                let mut keyword_dist = m.dist;
                for eword in &excess_words {
                    if let Some(word_idx) = words_index.get(eword) {
                        keyword_dist = util::max(keyword_dist - (m.dist * wrs[*word_idx]), 0.0);
                    }
                }

                let cosine_dist = self.cosine_diff_distance(
                    &words,
                    &cand_words,
                    &match_words,
                    &miss_words,
                    &excess_words,
                    keyword_dist,
                );

                let dist = Distance {
                    query_id: m.query_id,
                    keyword: keyword_dist,
                    cosine: cosine_dist,
                };

                SearchResult {
                    query_id: m.query_id,
                    dist: dist,
                    query: Some(cand_query),
                }
            })
            .collect();
        search_results.sort_by(|a, b| a.partial_cmp(&b).unwrap_or(Ordering::Less));
        search_results.truncate(count.unwrap_or(FETCH_MIN));

        Ok(search_results)
    }

    #[inline]
    fn cosine_diff_distance(
        &self,
        words: &Vec<String>,
        cand_words: &Vec<String>,
        match_words: &Vec<String>,
        miss_words: &Vec<String>,
        excess_words: &Vec<String>,
        keyword_dist: f32,
    ) -> Option<f32> {
        if self.word_vecs.is_none() {
            return None;
        }

        let match_len = match_words.len();
        let (mut rhs_match_vec, nf_match, _nf_match_words) = self
            .word_vecs
            .as_ref()
            .unwrap()
            .get_combined_vec(&match_words, &self.terms_relevance, &self.stopwords);

        let missing_len = miss_words.len();
        let excess_len = excess_words.len();

        if miss_words.is_empty() && excess_words.is_empty() {
            return Some(0.0);
        }

        let (mut missing_vec, nf_miss, _nf_miss_words) = self
            .word_vecs
            .as_ref()
            .unwrap()
            .get_combined_vec(&miss_words, &self.terms_relevance, &self.stopwords);
        let (mut excess_vec, nf_excs, _nf_excs_words) = self
            .word_vecs
            .as_ref()
            .unwrap()
            .get_combined_vec(&excess_words, &self.terms_relevance, &self.stopwords);

        // either no match words or none of them are found
        if nf_match == match_len {
            // no missing words or none of them found OR
            // no excess words or none of them found
            if nf_miss == missing_len || nf_excs == excess_len {
                return None;
            }

            word_vec::normalize(&mut excess_vec[..]);
            word_vec::normalize(&mut missing_vec[..]);
            let cos_dist = word_vec::cosine_distance(&excess_vec, &missing_vec);

            if missing_len > match_len || excess_len > match_len {
                return Some(util::min(cos_dist * (1.0 + keyword_dist), 1.0));
            }

            return Some(word_vec::cosine_distance(&excess_vec, &missing_vec));
        }

        // there are at least some match words at this point

        // if both, missing AND excess words are not found, we can't calculate cosine
        if nf_miss == missing_len && nf_excs == excess_len {
            return None;
        }

        // if no missing words, cosine dist
        if nf_miss == missing_len {
            if let Some(cos_dist) = self.cosine_distance(words, &cand_words) {
                let nf = (nf_excs + nf_match + nf_miss) as f32;
                let nr_miss = (missing_len + excess_len) as f32;

                if match_len >= 2 && missing_len == 0 && excess_len < 2 && keyword_dist < 0.3 {
                    return Some(cos_dist * keyword_dist);
                }

                if match_len >= 2
                    && match_len > missing_len
                    && match_len > excess_len
                    && keyword_dist < 0.45
                    && nf <= 1.0
                {
                    return Some(cos_dist * (keyword_dist / (keyword_dist + cos_dist)));
                }

                return Some(util::min(cos_dist + nf * keyword_dist / nr_miss, 1.0));
            }

            return None;
        }

        if nf_excs == excess_len {
            if let Some(cos_dist) = self.cosine_distance(words, &cand_words) {
                let nf = (nf_excs + nf_match + nf_miss) as f32;
                let nr_miss = (missing_len + excess_len) as f32;

                if match_len >= 2 && missing_len < 2 && excess_len == 0 && keyword_dist < 0.3 {
                    return Some(cos_dist * keyword_dist);
                }

                if match_len >= 2
                    && match_len > excess_len
                    && match_len > missing_len
                    && keyword_dist < 0.45
                    && nf <= 1.0
                {
                    return Some(cos_dist * (keyword_dist / (keyword_dist + cos_dist)));
                }

                return Some(util::min(cos_dist + nf * keyword_dist / nr_miss, 1.0));
            }

            return None;
        }

        let mut lhs_match_vec = rhs_match_vec.clone();

        if missing_len > 0 {
            word_vec::subtract(&mut lhs_match_vec, &missing_vec);
        }

        if excess_len > 0 {
            word_vec::subtract(&mut rhs_match_vec, &excess_vec);
        }

        word_vec::normalize(&mut missing_vec[..]);
        word_vec::normalize(&mut excess_vec[..]);
        let me_cos_dist = word_vec::cosine_distance(&missing_vec, &excess_vec);

        word_vec::normalize(&mut lhs_match_vec[..]);
        word_vec::normalize(&mut rhs_match_vec[..]);

        let cos_dist = word_vec::cosine_distance(&lhs_match_vec, &rhs_match_vec);

        let thresh: f32;
        if missing_len > 1 {
            if excess_len < 2 {
                thresh = 0.5;
            } else {
                thresh = 0.45;
            };
        } else {
            if excess_len < 2 {
                thresh = 0.55;
            } else {
                thresh = 0.65;
            };
        };

        // missing and excess words and keyword distance is too large
        if excess_len >= 1 && missing_len >= 1 && match_len <= 2 && keyword_dist > 0.45 {
            return Some(1.3 * cos_dist + util::max(0.0, me_cos_dist - thresh));
        }

        // more than one excess, at most one missing, keyword distance is not so big
        if excess_len <= 2 && missing_len <= 2 && match_len > excess_len && keyword_dist <= 0.45 {
            return Some(0.75 * cos_dist + util::max(0.0, me_cos_dist - thresh));
        }

        Some(cos_dist + util::max(0.0, me_cos_dist - thresh))
    }

    #[inline]
    fn cosine_distance(&self, query_words: &Vec<String>, cand_words: &Vec<String>) -> Option<f32> {
        let (mut query_vec, nf, _nf_query_words) = self
            .word_vecs
            .as_ref()
            .unwrap()
            .get_combined_vec(&query_words, &self.terms_relevance, &self.stopwords);

        if nf == query_words.len() {
            return None;
        }

        let (mut cand_vec, nf, _nf_cand_words) = self.word_vecs.as_ref().unwrap().get_combined_vec(
            &cand_words,
            &self.terms_relevance,
            &self.stopwords,
        );

        if nf == cand_words.len() {
            return None;
        }

        word_vec::normalize(&mut query_vec[..]);
        word_vec::normalize(&mut cand_vec[..]);

        Some(word_vec::cosine_distance(&query_vec, &cand_vec))
    }

    pub fn get_distances(&self, query: &str, candidates: &Vec<String>) -> Vec<DistanceResult> {
        if query == "" {
            return vec![];
        }

        let mut dist_results: Vec<DistanceResult> = vec![];
        let (_, _, _, words, wrs, _, word_syns) = ngrams::parse(
            &query,
            &self.synonyms,
            &self.toponyms,
            &self.stopwords,
            &self.terms_relevance,
            ngrams::ParseMode::Search,
        );

        let (words_index, words_set) = index_words(&words, &word_syns);

        let cand_synonyms: FnvHashMap<String, String> = word_syns
            .iter()
            .map(|(wid, syn)| (syn.to_string(), words[*wid].to_string()))
            .collect();

        for (cid, cand_query) in candidates.into_iter().enumerate() {
            let (_, _, _, cand_words, cand_wrs, _, _) = ngrams::parse(
                &cand_query,
                &self.synonyms,
                &self.toponyms,
                &self.stopwords,
                &self.terms_relevance,
                ngrams::ParseMode::Search,
            );

            let mut words_rel_vec = vec![0.0; words.len()];
            for (cword_idx, cword) in cand_words.iter().enumerate() {
                if let Some(word_idx) = words_index.get(cword) {
                    words_rel_vec[*word_idx] += util::min(wrs[*word_idx], cand_wrs[cword_idx]);
                }
            }

            let sim = words_rel_vec.iter().fold(0.0, |mut sum, &x| {
                sum += x;
                sum
            });
            let keyword_dist = util::max(1.0 - sim, 0.0);

            let (cand_words, match_words, miss_words, excess_words) =
                ngrams::match_queries(cand_query, &words_set, &cand_synonyms);

            let cosine_dist = self.cosine_diff_distance(
                &words,
                &cand_words,
                &match_words,
                &miss_words,
                &excess_words,
                keyword_dist,
            );

            dist_results.push(DistanceResult {
                query: cand_query.to_string(),
                dist: Distance {
                    query_id: cid as u64,
                    keyword: keyword_dist,
                    cosine: cosine_dist,
                },
            });
        }

        return dist_results;
    }

    pub fn get(&self, query: &str, count: u32, with_tfidf: bool) -> Vec<SearchResult> {
        if query == "" || count == 0 {
            return vec![];
        }

        let (ngrams, trs, ngrams_ids, words, wrs, must_have, synonyms) = ngrams::parse(
            &query,
            &self.synonyms,
            &self.toponyms,
            &self.stopwords,
            &self.terms_relevance,
            ngrams::ParseMode::Search,
        );

        match self.get_matches(
            ngrams,
            trs,
            ngrams_ids,
            words,
            wrs,
            must_have,
            synonyms,
            Some(count as usize),
            with_tfidf,
        ) {
            Ok(ids) => ids,
            Err(err) => {
                println!("Search error {:?}", err);

                vec![]
            }
        }
    }

    pub fn get_search_results_as_string(
        &self,
        query: &str,
        count: u32,
        with_tfidf: bool,
    ) -> String {
        let mut res: Vec<(u64, Distance, String)> = self
            .get(query, 30 * count, with_tfidf)
            .into_iter()
            .map(|r| (r.query_id, r.dist, r.query.unwrap_or("".to_string())))
            .collect();
        res.truncate(count as usize);

        serde_json::to_string(&res).unwrap()
    }

    pub fn merge(&self) -> Result<(), Error> {
        println!("Merging index maps from: {:?}", &self.path);
        merge::merge(&self.path, self.config.nr_shards as usize)
    }

    pub fn get_search_results(&self, query: &str, count: u32, with_tfidf: bool) -> SearchResults {
        SearchResults::new(self.get(query, count, with_tfidf).into_iter())
    }

    pub fn get_dist_results(&self, query: &str, candidates: &Vec<String>) -> DistResults {
        DistResults::new(self.get_distances(query, candidates).into_iter())
    }
}

#[allow(dead_code)]
fn main() {}
