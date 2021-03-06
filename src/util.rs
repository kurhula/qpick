use std::cmp::PartialOrd;
use std::error;
use std::fmt;
use std::io;

extern crate seahash;

#[macro_export]
macro_rules! remove_file_if_exists {
    ($fn_name: ident) => {
        match fs::remove_file($fn_name) {
            Err(err) => println!("Failed to delete previous {}, err: {:?}", $fn_name, err),
            Ok(_) => println!("Deleted previous file {}", $fn_name),
        };
    };
}

/// An error that occurred while computing elegant pair.
#[derive(Debug)]
pub enum ElegantPairError {
    /// The numbers given when paired exceed u64
    NumbersTooBig(u64, u64),
}

impl fmt::Display for ElegantPairError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ElegantPairError::*;
        match *self {
            NumbersTooBig(x, y) => write!(f, "{} and {} give a pair number bigger than 2^64", x, y),
        }
    }
}

impl error::Error for ElegantPairError {
    fn description(&self) -> &str {
        use self::ElegantPairError::*;
        match *self {
            NumbersTooBig(_, _) => "Numbers are too big to be paired",
        }
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

pub type Result<T> = ::std::result::Result<T, ElegantPairError>;

#[inline]
pub fn query_id_2_shard_id(qid: u64, nr_shards: usize) -> (u64, u8) {
    assert!(nr_shards < 256);
    (
        qid >> (nr_shards as f32).log(2.0) as u64,
        (qid % nr_shards as u64) as u8,
    )
}

#[inline]
pub fn shard_id_2_query_id(pqid: u64, reminder: u8, nr_shards: usize) -> u64 {
    (pqid << (nr_shards as f32).log(2.0) as u64) + reminder as u64
}

#[inline]
pub fn as_ptr<T>(v: &[T]) -> *const T {
    v as *const [T] as *const T
}

/*
   Elegant pairing function http://szudzik.com/ElegantPairing.pdf
   TODO implement with bignum, otherwise might overflow!
*/
#[inline]
pub fn elegant_pair(x: u64, y: u64) -> Result<u64> {
    let z: u64 = match x >= y {
        true => x * x + x + y,
        false => y * y + x,
    };

    if elegant_pair_inv(z) != (x, y) {
        return Err(ElegantPairError::NumbersTooBig(x, y).into());
    }

    Ok(z)
}

/*
    Inverse elegant pairing function http://szudzik.com/ElegantPairing.pdf
    TODO implement with bignum or f128, otherwise might overflow!
*/
#[inline]
pub fn elegant_pair_inv(z: u64) -> (u64, u64) {
    let q = z as f64;
    let w = (q.sqrt()).floor() as u64;
    let t = (w * w) as u64;
    if (z - t) >= w {
        (w, z - t - w)
    } else {
        (z - t, w)
    }
}

#[inline]
pub fn max<T: PartialOrd>(a: T, b: T) -> T {
    if a > b {
        a
    } else {
        b
    }
}

#[inline]
pub fn min<T: PartialOrd>(a: T, b: T) -> T {
    if a < b {
        a
    } else {
        b
    }
}

// A Fast, Minimal Memory, Consistent Hash Algorithm by John Lamping and Eric Veach:
// https://arxiv.org/pdf/1406.2294.pdf
// It outputs a bucket number in the range [0, num_buckets).
#[inline]
pub fn jump_consistent_hash(mut key: u64, num_buckets: u32) -> u32 {
    assert!(num_buckets > 0);

    let mut b: i64 = -1;
    let mut j: i64 = 0;

    while j < num_buckets as i64 {
        b = j;
        key = key.wrapping_mul(2862933555777941757).wrapping_add(1);
        j = ((b.wrapping_add(1) as f64) * ((1i64 << 31) as f64)
            / ((key >> 33).wrapping_add(1) as f64)) as i64;
    }

    b as u32
}

#[inline]
pub fn jump_consistent_hash_str(key: &str, num_buckets: u32) -> u32 {
    //
    jump_consistent_hash(seahash::hash(key.as_bytes()), num_buckets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_consistent_hash_str_test() {
        assert_eq!(
            3,
            jump_consistent_hash_str("how to put on thai fishing pants", 32)
        );
    }

    #[test]
    fn jump_consistent_hash_test() {
        assert_eq!(7, jump_consistent_hash(1000011111111, 32));
        assert_eq!(0, jump_consistent_hash(1000011111111, 1));
    }

    #[test]
    #[should_panic]
    fn jump_consistent_hash_panic_test() {
        assert_eq!(7, jump_consistent_hash(1000011111111, 0));
    }

    #[test]
    #[should_panic]
    fn query_id_2_shard_id_and_inv_test() {
        assert_eq!(
            499998000,
            shard_id_2_query_id(
                query_id_2_shard_id(499998000, 32).0,
                query_id_2_shard_id(499998000, 32).1,
                32
            )
        );
        assert_eq!(
            499998001,
            shard_id_2_query_id(
                query_id_2_shard_id(499998001, 57).0,
                query_id_2_shard_id(499998001, 57).1,
                57
            )
        );
        assert_eq!(
            499998011,
            shard_id_2_query_id(
                query_id_2_shard_id(499998011, 73).0,
                query_id_2_shard_id(499998011, 73).1,
                73
            )
        );
        assert_eq!(
            499998111,
            shard_id_2_query_id(
                query_id_2_shard_id(499998111, 60).0,
                query_id_2_shard_id(499998111, 60).1,
                60
            )
        );
    }
}

pub static BRED: &str = "\x1B[0;31m";
pub static BYELL: &str = "\x1B[0;33m";
pub static ECOL: &str = "\x1B[0m";

// tmp file
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::env::temp_dir;
use std::path::PathBuf;
use std::str;

pub fn tmp_file_path(prefix: &str, suffix: &str, rand_len: usize) -> PathBuf {
    let mut buf = String::with_capacity(prefix.len() + suffix.len() + rand_len);
    buf.push_str(prefix);
    unsafe {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(rand_len)
            .for_each(|b| buf.push_str(str::from_utf8_unchecked(&[b as u8])))
    }
    buf.push_str(suffix);

    temp_dir().join(buf)
}

// Advise the OS on the random access pattern of data.
// Taken from https://docs.rs/crate/madvise/0.1.0
#[cfg(unix)]
pub fn advise_ram(data: &[u8]) -> io::Result<()> {
    unsafe {
        let result = libc::madvise(
            as_ptr(data) as *mut libc::c_void,
            data.len(),
            libc::MADV_RANDOM as libc::c_int,
        );

        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
