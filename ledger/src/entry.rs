//! The `entry` module is a fundamental building block of Proof of History. It contains a
//! unique ID that is the hash of the Entry before it, plus the hash of the
//! transactions within it. Entries cannot be reordered, and its field `num_hashes`
//! represents an approximate amount of time since the last Entry was created.
use crate::poh::Poh;
use dlopen::symbor::{Container, SymBorApi, Symbol};
use dlopen_derive::SymBorApi;
use log::*;
use rand::{thread_rng, Rng};
use rayon::prelude::*;
use rayon::ThreadPool;
use serde::{Deserialize, Serialize};
use solana_measure::measure::Measure;
use solana_merkle_tree::MerkleTree;
use solana_metrics::*;
use solana_perf::cuda_runtime::PinnedVec;
use solana_perf::perf_libs;
use solana_perf::recycler::Recycler;
use solana_rayon_threadlimit::get_thread_count;
use solana_sdk::hash::Hash;
use solana_sdk::timing;
use solana_sdk::transaction::Transaction;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Once;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
use std::{cmp, thread};

thread_local!(static PAR_THREAD_POOL: RefCell<ThreadPool> = RefCell::new(rayon::ThreadPoolBuilder::new()
                    .num_threads(get_thread_count())
                    .thread_name(|ix| format!("entry_{}", ix))
                    .build()
                    .unwrap()));

pub type EntrySender = Sender<Vec<Entry>>;
pub type EntryReceiver = Receiver<Vec<Entry>>;

static mut API: Option<Container<Api>> = None;

pub fn init_poh() {
    init(OsStr::new("libpoh-simd.so"));
}

fn init(name: &OsStr) {
    static INIT_HOOK: Once = Once::new();

    info!("Loading {:?}", name);
    unsafe {
        INIT_HOOK.call_once(|| {
            let path;
            let lib_name = if let Some(perf_libs_path) = solana_perf::perf_libs::locate_perf_libs()
            {
                solana_perf::perf_libs::append_to_ld_library_path(
                    perf_libs_path.to_str().unwrap_or("").to_string(),
                );
                path = perf_libs_path.join(name);
                path.as_os_str()
            } else {
                name
            };

            API = Container::load(lib_name).ok();
        })
    }
}

pub fn api() -> Option<&'static Container<Api<'static>>> {
    {
        static INIT_HOOK: Once = Once::new();
        INIT_HOOK.call_once(|| {
            if std::env::var("TEST_PERF_LIBS").is_ok() {
                init_poh()
            }
        })
    }

    unsafe { API.as_ref() }
}

#[derive(SymBorApi)]
pub struct Api<'a> {
    pub poh_verify_many_simd_avx512skx:
        Symbol<'a, unsafe extern "C" fn(hashes: *mut u8, num_hashes: *const u64)>,
    pub poh_verify_many_simd_avx2:
        Symbol<'a, unsafe extern "C" fn(hashes: *mut u8, num_hashes: *const u64)>,
}

/// Each Entry contains three pieces of data. The `num_hashes` field is the number
/// of hashes performed since the previous entry.  The `hash` field is the result
/// of hashing `hash` from the previous entry `num_hashes` times.  The `transactions`
/// field points to Transactions that took place shortly before `hash` was generated.
///
/// If you divide `num_hashes` by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last Entry. Since processing power increases
/// over time, one should expect the duration `num_hashes` represents to decrease proportionally.
/// An upper bound on Duration can be estimated by assuming each hash was generated by the
/// world's fastest processor at the time the entry was recorded. Or said another way, it
/// is physically not possible for a shorter duration to have occurred if one assumes the
/// hash was computed by the world's fastest processor at that time. The hash chain is both
/// a Verifiable Delay Function (VDF) and a Proof of Work (not to be confused with Proof of
/// Work consensus!)

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct Entry {
    /// The number of hashes since the previous Entry ID.
    pub num_hashes: u64,

    /// The SHA-256 hash `num_hashes` after the previous Entry ID.
    pub hash: Hash,

    /// An unordered list of transactions that were observed before the Entry ID was
    /// generated. They may have been observed before a previous Entry ID but were
    /// pushed back into this list to ensure deterministic interpretation of the ledger.
    pub transactions: Vec<Transaction>,
}

impl Entry {
    /// Creates the next Entry `num_hashes` after `start_hash`.
    pub fn new(prev_hash: &Hash, mut num_hashes: u64, transactions: Vec<Transaction>) -> Self {
        // If you passed in transactions, but passed in num_hashes == 0, then
        // next_hash will generate the next hash and set num_hashes == 1
        if num_hashes == 0 && !transactions.is_empty() {
            num_hashes = 1;
        }

        let hash = next_hash(prev_hash, num_hashes, &transactions);
        Entry {
            num_hashes,
            hash,
            transactions,
        }
    }

    pub fn new_mut(
        start_hash: &mut Hash,
        num_hashes: &mut u64,
        transactions: Vec<Transaction>,
    ) -> Self {
        let entry = Self::new(start_hash, *num_hashes, transactions);
        *start_hash = entry.hash;
        *num_hashes = 0;

        entry
    }

    #[cfg(test)]
    pub fn new_tick(num_hashes: u64, hash: &Hash) -> Self {
        Entry {
            num_hashes,
            hash: *hash,
            transactions: vec![],
        }
    }

    /// Verifies self.hash is the result of hashing a `start_hash` `self.num_hashes` times.
    /// If the transaction is not a Tick, then hash that as well.
    pub fn verify(&self, start_hash: &Hash) -> bool {
        let ref_hash = next_hash(start_hash, self.num_hashes, &self.transactions);
        if self.hash != ref_hash {
            warn!(
                "next_hash is invalid expected: {:?} actual: {:?}",
                self.hash, ref_hash
            );
            return false;
        }
        true
    }

    pub fn is_tick(&self) -> bool {
        self.transactions.is_empty()
    }
}

pub fn hash_transactions(transactions: &[Transaction]) -> Hash {
    // a hash of a slice of transactions only needs to hash the signatures
    let signatures: Vec<_> = transactions
        .iter()
        .flat_map(|tx| tx.signatures.iter())
        .collect();
    let merkle_tree = MerkleTree::new(&signatures);
    if let Some(root_hash) = merkle_tree.get_root() {
        *root_hash
    } else {
        Hash::default()
    }
}

/// Creates the hash `num_hashes` after `start_hash`. If the transaction contains
/// a signature, the final hash will be a hash of both the previous ID and
/// the signature.  If num_hashes is zero and there's no transaction data,
///  start_hash is returned.
pub fn next_hash(start_hash: &Hash, num_hashes: u64, transactions: &[Transaction]) -> Hash {
    if num_hashes == 0 && transactions.is_empty() {
        return *start_hash;
    }

    let mut poh = Poh::new(*start_hash, None);
    poh.hash(num_hashes.saturating_sub(1));
    if transactions.is_empty() {
        poh.tick().unwrap().hash
    } else {
        poh.record(hash_transactions(transactions)).unwrap().hash
    }
}

pub struct GpuVerificationData {
    thread_h: Option<JoinHandle<u64>>,
    hashes: Option<Arc<Mutex<PinnedVec<Hash>>>>,
    tx_hashes: Vec<Option<Hash>>,
}

pub enum DeviceVerificationData {
    CPU(),
    GPU(GpuVerificationData),
}

pub struct EntryVerificationState {
    verification_status: EntryVerificationStatus,
    poh_duration_us: u64,
    transaction_duration_us: u64,
    device_verification_data: DeviceVerificationData,
}

#[derive(Default, Clone)]
pub struct VerifyRecyclers {
    hash_recycler: Recycler<PinnedVec<Hash>>,
    tick_count_recycler: Recycler<PinnedVec<u64>>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum EntryVerificationStatus {
    Failure,
    Success,
    Pending,
}

impl EntryVerificationState {
    pub fn status(&self) -> EntryVerificationStatus {
        self.verification_status
    }

    pub fn poh_duration_us(&self) -> u64 {
        self.poh_duration_us
    }

    pub fn set_transaction_duration_us(&mut self, new: u64) {
        self.transaction_duration_us = new;
    }

    pub fn transaction_duration_us(&self) -> u64 {
        self.transaction_duration_us
    }

    pub fn finish_verify(&mut self, entries: &[Entry]) -> bool {
        match &mut self.device_verification_data {
            DeviceVerificationData::GPU(verification_state) => {
                let gpu_time_us = verification_state.thread_h.take().unwrap().join().unwrap();

                let mut verify_check_time = Measure::start("verify_check");
                let hashes = verification_state.hashes.take().unwrap();
                let hashes = Arc::try_unwrap(hashes)
                    .expect("unwrap Arc")
                    .into_inner()
                    .expect("into_inner");
                let res = PAR_THREAD_POOL.with(|thread_pool| {
                    thread_pool.borrow().install(|| {
                        hashes
                            .into_par_iter()
                            .zip(&verification_state.tx_hashes)
                            .zip(entries)
                            .all(|((hash, tx_hash), answer)| {
                                if answer.num_hashes == 0 {
                                    *hash == answer.hash
                                } else {
                                    let mut poh = Poh::new(*hash, None);
                                    if let Some(mixin) = tx_hash {
                                        poh.record(*mixin).unwrap().hash == answer.hash
                                    } else {
                                        poh.tick().unwrap().hash == answer.hash
                                    }
                                }
                            })
                    })
                });

                verify_check_time.stop();
                self.poh_duration_us += gpu_time_us + verify_check_time.as_us();

                self.verification_status = if res {
                    EntryVerificationStatus::Success
                } else {
                    EntryVerificationStatus::Failure
                };
                res
            }
            DeviceVerificationData::CPU() => {
                self.verification_status == EntryVerificationStatus::Success
            }
        }
    }
}

fn compare_hashes(computed_hash: Hash, ref_entry: &Entry) -> bool {
    if ref_entry.num_hashes == 0 {
        computed_hash == ref_entry.hash
    } else {
        let mut poh = Poh::new(computed_hash, None);
        if ref_entry.transactions.is_empty() {
            poh.tick().unwrap().hash == ref_entry.hash
        } else {
            let tx_hash = hash_transactions(&ref_entry.transactions);
            poh.record(tx_hash).unwrap().hash == ref_entry.hash
        }
    }
}

// an EntrySlice is a slice of Entries
pub trait EntrySlice {
    /// Verifies the hashes and counts of a slice of transactions are all consistent.
    fn verify_cpu(&self, start_hash: &Hash) -> EntryVerificationState;
    fn verify_cpu_generic(&self, start_hash: &Hash) -> EntryVerificationState;
    fn verify_cpu_x86_simd(&self, start_hash: &Hash, simd_len: usize) -> EntryVerificationState;
    fn start_verify(
        &self,
        start_hash: &Hash,
        recyclers: VerifyRecyclers,
        secp256k1_program_enabled: bool,
    ) -> EntryVerificationState;
    fn verify(&self, start_hash: &Hash) -> bool;
    /// Checks that each entry tick has the correct number of hashes. Entry slices do not
    /// necessarily end in a tick, so `tick_hash_count` is used to carry over the hash count
    /// for the next entry slice.
    fn verify_tick_hash_count(&self, tick_hash_count: &mut u64, hashes_per_tick: u64) -> bool;
    /// Counts tick entries
    fn tick_count(&self) -> u64;
    fn verify_transaction_signatures(&self, secp256k1_program_enabled: bool) -> bool;
}

impl EntrySlice for [Entry] {
    fn verify(&self, start_hash: &Hash) -> bool {
        self.start_verify(start_hash, VerifyRecyclers::default(), true)
            .finish_verify(self)
    }

    fn verify_cpu_generic(&self, start_hash: &Hash) -> EntryVerificationState {
        let now = Instant::now();
        let genesis = [Entry {
            num_hashes: 0,
            hash: *start_hash,
            transactions: vec![],
        }];
        let entry_pairs = genesis.par_iter().chain(self).zip(self);
        let res = PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                entry_pairs.all(|(x0, x1)| {
                    let r = x1.verify(&x0.hash);
                    if !r {
                        warn!(
                            "entry invalid!: x0: {:?}, x1: {:?} num txs: {}",
                            x0.hash,
                            x1.hash,
                            x1.transactions.len()
                        );
                    }
                    r
                })
            })
        });

        let poh_duration_us = timing::duration_as_us(&now.elapsed());
        EntryVerificationState {
            verification_status: if res {
                EntryVerificationStatus::Success
            } else {
                EntryVerificationStatus::Failure
            },
            poh_duration_us,
            transaction_duration_us: 0,
            device_verification_data: DeviceVerificationData::CPU(),
        }
    }

    fn verify_cpu_x86_simd(&self, start_hash: &Hash, simd_len: usize) -> EntryVerificationState {
        use solana_sdk::hash::HASH_BYTES;
        let now = Instant::now();
        let genesis = [Entry {
            num_hashes: 0,
            hash: *start_hash,
            transactions: vec![],
        }];

        let aligned_len = ((self.len() + simd_len - 1) / simd_len) * simd_len;
        let mut hashes_bytes = vec![0u8; HASH_BYTES * aligned_len];
        genesis
            .iter()
            .chain(self)
            .enumerate()
            .for_each(|(i, entry)| {
                if i < self.len() {
                    let start = i * HASH_BYTES;
                    let end = start + HASH_BYTES;
                    hashes_bytes[start..end].copy_from_slice(&entry.hash.to_bytes());
                }
            });
        let mut hashes_chunked: Vec<_> = hashes_bytes.chunks_mut(simd_len * HASH_BYTES).collect();

        let mut num_hashes: Vec<u64> = self
            .iter()
            .map(|entry| entry.num_hashes.saturating_sub(1))
            .collect();
        num_hashes.resize(aligned_len, 0);
        let num_hashes: Vec<_> = num_hashes.chunks(simd_len).collect();

        let res = PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                hashes_chunked
                    .par_iter_mut()
                    .zip(num_hashes)
                    .enumerate()
                    .all(|(i, (chunk, num_hashes))| {
                        match simd_len {
                            8 => unsafe {
                                (api().unwrap().poh_verify_many_simd_avx2)(
                                    chunk.as_mut_ptr(),
                                    num_hashes.as_ptr(),
                                );
                            },
                            16 => unsafe {
                                (api().unwrap().poh_verify_many_simd_avx512skx)(
                                    chunk.as_mut_ptr(),
                                    num_hashes.as_ptr(),
                                );
                            },
                            _ => {
                                panic!("unsupported simd len: {}", simd_len);
                            }
                        }
                        let entry_start = i * simd_len;
                        // The last chunk may produce indexes larger than what we have in the reference entries
                        // because it is aligned to simd_len.
                        let entry_end = std::cmp::min(entry_start + simd_len, self.len());
                        self[entry_start..entry_end]
                            .iter()
                            .enumerate()
                            .all(|(j, ref_entry)| {
                                let start = j * HASH_BYTES;
                                let end = start + HASH_BYTES;
                                let hash = Hash::new(&chunk[start..end]);
                                compare_hashes(hash, ref_entry)
                            })
                    })
            })
        });
        let poh_duration_us = timing::duration_as_us(&now.elapsed());
        EntryVerificationState {
            verification_status: if res {
                EntryVerificationStatus::Success
            } else {
                EntryVerificationStatus::Failure
            },
            poh_duration_us,
            transaction_duration_us: 0,
            device_verification_data: DeviceVerificationData::CPU(),
        }
    }

    fn verify_cpu(&self, start_hash: &Hash) -> EntryVerificationState {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let (has_avx2, has_avx512) = (
            is_x86_feature_detected!("avx2"),
            is_x86_feature_detected!("avx512f"),
        );
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        let (has_avx2, has_avx512) = (false, false);

        if api().is_some() {
            if has_avx512 && self.len() >= 128 {
                self.verify_cpu_x86_simd(start_hash, 16)
            } else if has_avx2 && self.len() >= 48 {
                self.verify_cpu_x86_simd(start_hash, 8)
            } else {
                self.verify_cpu_generic(start_hash)
            }
        } else {
            self.verify_cpu_generic(start_hash)
        }
    }

    fn verify_transaction_signatures(&self, secp256k1_program_enabled: bool) -> bool {
        PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                self.par_iter().all(|e| {
                    e.transactions.par_iter().all(|transaction| {
                        let sig_verify = transaction.verify().is_ok();
                        if sig_verify
                            && secp256k1_program_enabled
                            && transaction.verify_precompiles().is_err()
                        {
                            return false;
                        }
                        sig_verify
                    })
                })
            })
        })
    }

    fn start_verify(
        &self,
        start_hash: &Hash,
        recyclers: VerifyRecyclers,
        secp256k1_program_enabled: bool,
    ) -> EntryVerificationState {
        let start = Instant::now();
        let res = self.verify_transaction_signatures(secp256k1_program_enabled);
        let transaction_duration_us = timing::duration_as_us(&start.elapsed());
        if !res {
            return EntryVerificationState {
                verification_status: EntryVerificationStatus::Failure,
                transaction_duration_us,
                poh_duration_us: 0,
                device_verification_data: DeviceVerificationData::CPU(),
            };
        }

        let start = Instant::now();
        let api = perf_libs::api();
        if api.is_none() {
            let mut res: EntryVerificationState = self.verify_cpu(start_hash);
            res.set_transaction_duration_us(transaction_duration_us);
            return res;
        }
        let api = api.unwrap();
        inc_new_counter_info!("entry_verify-num_entries", self.len() as usize);

        let genesis = [Entry {
            num_hashes: 0,
            hash: *start_hash,
            transactions: vec![],
        }];

        let hashes: Vec<Hash> = genesis
            .iter()
            .chain(self)
            .map(|entry| entry.hash)
            .take(self.len())
            .collect();

        let mut hashes_pinned = recyclers.hash_recycler.allocate("poh_verify_hash");
        hashes_pinned.set_pinnable();
        hashes_pinned.resize(hashes.len(), Hash::default());
        hashes_pinned.copy_from_slice(&hashes);

        let mut num_hashes_vec = recyclers
            .tick_count_recycler
            .allocate("poh_verify_num_hashes");
        num_hashes_vec.reserve_and_pin(cmp::max(1, self.len()));
        for entry in self {
            num_hashes_vec.push(entry.num_hashes.saturating_sub(1));
        }

        let length = self.len();
        let hashes = Arc::new(Mutex::new(hashes_pinned));
        let hashes_clone = hashes.clone();

        let gpu_verify_thread = thread::spawn(move || {
            let mut hashes = hashes_clone.lock().unwrap();
            let gpu_wait = Instant::now();
            let res;
            unsafe {
                res = (api.poh_verify_many)(
                    hashes.as_mut_ptr() as *mut u8,
                    num_hashes_vec.as_ptr(),
                    length,
                    1,
                );
            }
            if res != 0 {
                panic!("GPU PoH verify many failed");
            }
            inc_new_counter_info!(
                "entry_verify-gpu_thread",
                timing::duration_as_us(&gpu_wait.elapsed()) as usize
            );
            timing::duration_as_us(&gpu_wait.elapsed())
        });

        let tx_hashes = PAR_THREAD_POOL.with(|thread_pool| {
            thread_pool.borrow().install(|| {
                self.into_par_iter()
                    .map(|entry| {
                        if entry.transactions.is_empty() {
                            None
                        } else {
                            Some(hash_transactions(&entry.transactions))
                        }
                    })
                    .collect()
            })
        });

        let device_verification_data = DeviceVerificationData::GPU(GpuVerificationData {
            thread_h: Some(gpu_verify_thread),
            tx_hashes,
            hashes: Some(hashes),
        });
        EntryVerificationState {
            verification_status: EntryVerificationStatus::Pending,
            poh_duration_us: timing::duration_as_us(&start.elapsed()),
            transaction_duration_us,
            device_verification_data,
        }
    }

    fn verify_tick_hash_count(&self, tick_hash_count: &mut u64, hashes_per_tick: u64) -> bool {
        // When hashes_per_tick is 0, hashing is disabled.
        if hashes_per_tick == 0 {
            return true;
        }

        for entry in self {
            *tick_hash_count += entry.num_hashes;
            if entry.is_tick() {
                if *tick_hash_count != hashes_per_tick {
                    warn!(
                        "invalid tick hash count!: entry: {:#?}, tick_hash_count: {}, hashes_per_tick: {}",
                        entry,
                        tick_hash_count,
                        hashes_per_tick
                    );
                    return false;
                }
                *tick_hash_count = 0;
            }
        }
        *tick_hash_count < hashes_per_tick
    }

    fn tick_count(&self) -> u64 {
        self.iter().filter(|e| e.is_tick()).count() as u64
    }
}

pub fn next_entry_mut(start: &mut Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    let entry = Entry::new(&start, num_hashes, transactions);
    *start = entry.hash;
    entry
}

#[allow(clippy::same_item_push)]
pub fn create_ticks(num_ticks: u64, hashes_per_tick: u64, mut hash: Hash) -> Vec<Entry> {
    let mut ticks = Vec::with_capacity(num_ticks as usize);
    for _ in 0..num_ticks {
        let new_tick = next_entry_mut(&mut hash, hashes_per_tick, vec![]);
        ticks.push(new_tick);
    }

    ticks
}

#[allow(clippy::same_item_push)]
pub fn create_random_ticks(num_ticks: u64, max_hashes_per_tick: u64, mut hash: Hash) -> Vec<Entry> {
    let mut ticks = Vec::with_capacity(num_ticks as usize);
    for _ in 0..num_ticks {
        let hashes_per_tick = thread_rng().gen_range(1, max_hashes_per_tick);
        let new_tick = next_entry_mut(&mut hash, hashes_per_tick, vec![]);
        ticks.push(new_tick);
    }

    ticks
}

/// Creates the next Tick or Transaction Entry `num_hashes` after `start_hash`.
pub fn next_entry(prev_hash: &Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    assert!(num_hashes > 0 || transactions.is_empty());
    Entry {
        num_hashes,
        hash: next_hash(prev_hash, num_hashes, &transactions),
        transactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use chrono::prelude::Utc;
    use solana_budget_program::budget_instruction;
    use solana_sdk::{
        hash::{hash, Hash},
        message::Message,
        signature::{Keypair, Signer},
        system_transaction,
        transaction::Transaction,
    };

    fn create_sample_payment(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let budget_contract = Keypair::new();
        let budget_pubkey = budget_contract.pubkey();
        let ixs = budget_instruction::payment(&pubkey, &pubkey, &budget_pubkey, 1);
        let message = Message::new(&ixs, Some(&pubkey));
        Transaction::new(&[keypair, &budget_contract], message, hash)
    }

    fn create_sample_timestamp(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let ix = budget_instruction::apply_timestamp(&pubkey, &pubkey, &pubkey, Utc::now());
        let message = Message::new(&[ix], Some(&pubkey));
        Transaction::new(&[keypair], message, hash)
    }

    fn create_sample_apply_signature(keypair: &Keypair, hash: Hash) -> Transaction {
        let pubkey = keypair.pubkey();
        let ix = budget_instruction::apply_signature(&pubkey, &pubkey, &pubkey);
        let message = Message::new(&[ix], Some(&pubkey));
        Transaction::new(&[keypair], message, hash)
    }

    #[test]
    fn test_entry_verify() {
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(Entry::new_tick(0, &zero).verify(&zero)); // base case, never used
        assert!(!Entry::new_tick(0, &zero).verify(&one)); // base case, bad
        assert!(next_entry(&zero, 1, vec![]).verify(&zero)); // inductive step
        assert!(!next_entry(&zero, 1, vec![]).verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_transaction_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, zero);
        let tx1 = system_transaction::transfer(&keypair, &keypair.pubkey(), 1, zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_transaction_signing() {
        use solana_sdk::signature::Signature;
        let zero = Hash::default();

        let keypair = Keypair::new();
        let tx0 = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, zero);
        let tx1 = system_transaction::transfer(&keypair, &keypair.pubkey(), 1, zero);

        // Verify entry with 2 transactions
        let mut e0 = vec![Entry::new(&zero, 0, vec![tx0, tx1])];
        assert!(e0.verify(&zero));

        // Clear signature of the first transaction, see that it does not verify
        let orig_sig = e0[0].transactions[0].signatures[0];
        e0[0].transactions[0].signatures[0] = Signature::default();
        assert!(!e0.verify(&zero));

        // restore original signature
        e0[0].transactions[0].signatures[0] = orig_sig;
        assert!(e0.verify(&zero));

        // Resize signatures and see verification fails.
        let len = e0[0].transactions[0].signatures.len();
        e0[0].transactions[0]
            .signatures
            .resize(len - 1, Signature::default());
        assert!(!e0.verify(&zero));

        // Pass an entry with no transactions
        let e0 = vec![Entry::new(&zero, 0, vec![])];
        assert!(e0.verify(&zero));
    }

    #[test]
    fn test_witness_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = create_sample_timestamp(&keypair, zero);
        let tx1 = create_sample_apply_signature(&keypair, zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two witness transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_next_entry() {
        let zero = Hash::default();
        let tick = next_entry(&zero, 1, vec![]);
        assert_eq!(tick.num_hashes, 1);
        assert_ne!(tick.hash, zero);

        let tick = next_entry(&zero, 0, vec![]);
        assert_eq!(tick.num_hashes, 0);
        assert_eq!(tick.hash, zero);

        let keypair = Keypair::new();
        let tx0 = create_sample_timestamp(&keypair, zero);
        let entry0 = next_entry(&zero, 1, vec![tx0.clone()]);
        assert_eq!(entry0.num_hashes, 1);
        assert_eq!(entry0.hash, next_hash(&zero, 1, &[tx0]));
    }

    #[test]
    #[should_panic]
    fn test_next_entry_panic() {
        let zero = Hash::default();
        let keypair = Keypair::new();
        let tx = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, zero);
        next_entry(&zero, 0, vec![tx]);
    }

    #[test]
    fn test_verify_slice1() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert_eq!(vec![][..].verify(&zero), true); // base case
        assert_eq!(vec![Entry::new_tick(0, &zero)][..].verify(&zero), true); // singleton case 1
        assert_eq!(vec![Entry::new_tick(0, &zero)][..].verify(&one), false); // singleton case 2, bad
        assert_eq!(
            vec![next_entry(&zero, 0, vec![]); 2][..].verify(&zero),
            true
        ); // inductive step

        let mut bad_ticks = vec![next_entry(&zero, 0, vec![]); 2];
        bad_ticks[1].hash = one;
        assert_eq!(bad_ticks.verify(&zero), false); // inductive step, bad
    }

    #[test]
    fn test_verify_slice_with_hashes1() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        let two = hash(&one.as_ref());
        assert_eq!(vec![][..].verify(&one), true); // base case
        assert_eq!(vec![Entry::new_tick(1, &two)][..].verify(&one), true); // singleton case 1
        assert_eq!(vec![Entry::new_tick(1, &two)][..].verify(&two), false); // singleton case 2, bad

        let mut ticks = vec![next_entry(&one, 1, vec![])];
        ticks.push(next_entry(&ticks.last().unwrap().hash, 1, vec![]));
        assert_eq!(ticks.verify(&one), true); // inductive step

        let mut bad_ticks = vec![next_entry(&one, 1, vec![])];
        bad_ticks.push(next_entry(&bad_ticks.last().unwrap().hash, 1, vec![]));
        bad_ticks[1].hash = one;
        assert_eq!(bad_ticks.verify(&one), false); // inductive step, bad
    }

    #[test]
    fn test_verify_slice_with_hashes_and_transactions() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        let two = hash(&one.as_ref());
        let alice_pubkey = Keypair::new();
        let tx0 = create_sample_payment(&alice_pubkey, one);
        let tx1 = create_sample_timestamp(&alice_pubkey, one);
        assert_eq!(vec![][..].verify(&one), true); // base case
        assert_eq!(
            vec![next_entry(&one, 1, vec![tx0.clone()])][..].verify(&one),
            true
        ); // singleton case 1
        assert_eq!(
            vec![next_entry(&one, 1, vec![tx0.clone()])][..].verify(&two),
            false
        ); // singleton case 2, bad

        let mut ticks = vec![next_entry(&one, 1, vec![tx0.clone()])];
        ticks.push(next_entry(
            &ticks.last().unwrap().hash,
            1,
            vec![tx1.clone()],
        ));
        assert_eq!(ticks.verify(&one), true); // inductive step

        let mut bad_ticks = vec![next_entry(&one, 1, vec![tx0])];
        bad_ticks.push(next_entry(&bad_ticks.last().unwrap().hash, 1, vec![tx1]));
        bad_ticks[1].hash = one;
        assert_eq!(bad_ticks.verify(&one), false); // inductive step, bad
    }

    #[test]
    fn test_verify_tick_hash_count() {
        let hashes_per_tick = 10;
        let tx = Transaction::default();
        let tx_entry = Entry::new(&Hash::default(), 1, vec![tx]);
        let full_tick_entry = Entry::new_tick(hashes_per_tick, &Hash::default());
        let partial_tick_entry = Entry::new_tick(hashes_per_tick - 1, &Hash::default());
        let no_hash_tick_entry = Entry::new_tick(0, &Hash::default());
        let single_hash_tick_entry = Entry::new_tick(1, &Hash::default());

        let no_ticks = vec![];
        let mut tick_hash_count = 0;
        assert!(no_ticks.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        // validation is disabled when hashes_per_tick == 0
        let no_hash_tick = vec![no_hash_tick_entry.clone()];
        assert!(no_hash_tick.verify_tick_hash_count(&mut tick_hash_count, 0));
        assert_eq!(tick_hash_count, 0);

        // validation is disabled when hashes_per_tick == 0
        let tx_and_no_hash_tick = vec![tx_entry.clone(), no_hash_tick_entry];
        assert!(tx_and_no_hash_tick.verify_tick_hash_count(&mut tick_hash_count, 0));
        assert_eq!(tick_hash_count, 0);

        let single_tick = vec![full_tick_entry];
        assert!(single_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);
        assert!(!single_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick - 1));
        assert_eq!(tick_hash_count, hashes_per_tick);
        tick_hash_count = 0;

        let ticks_and_txs = vec![tx_entry.clone(), partial_tick_entry.clone()];
        assert!(ticks_and_txs.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        let partial_tick = vec![partial_tick_entry];
        assert!(!partial_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, hashes_per_tick - 1);
        tick_hash_count = 0;

        let tx_entries: Vec<Entry> = (0..hashes_per_tick - 1).map(|_| tx_entry.clone()).collect();
        let tx_entries_and_tick = [tx_entries, vec![single_hash_tick_entry]].concat();
        assert!(tx_entries_and_tick.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, 0);

        let too_many_tx_entries: Vec<Entry> =
            (0..hashes_per_tick).map(|_| tx_entry.clone()).collect();
        assert!(!too_many_tx_entries.verify_tick_hash_count(&mut tick_hash_count, hashes_per_tick));
        assert_eq!(tick_hash_count, hashes_per_tick);
    }

    #[test]
    fn test_poh_verify_fuzz() {
        solana_logger::setup();
        for _ in 0..100 {
            let mut time = Measure::start("ticks");
            let num_ticks = thread_rng().gen_range(1, 100);
            info!("create {} ticks:", num_ticks);
            let mut entries = create_random_ticks(num_ticks, 100, Hash::default());
            time.stop();

            let mut modified = false;
            if thread_rng().gen_ratio(1, 2) {
                modified = true;
                let modify_idx = thread_rng().gen_range(0, num_ticks) as usize;
                entries[modify_idx].hash = hash(&[1, 2, 3]);
            }

            info!("done.. {}", time);
            let mut time = Measure::start("poh");
            let res = entries.verify(&Hash::default());
            assert_eq!(res, !modified);
            time.stop();
            info!("{} {}", time, res);
        }
    }
}
