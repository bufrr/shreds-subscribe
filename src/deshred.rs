use crate::rpc::Subscription;
use dashmap::DashMap;
use itertools::Itertools;
use solana_ledger::blockstore::MAX_DATA_SHREDS_PER_SLOT;
use solana_ledger::shred::{
    ReedSolomonCache, ShredType, Shredder,
    merkle::{Shred, ShredCode},
};
use solana_sdk::clock::Slot;
use solana_streamer::packet::PacketBatch;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::hash::Hash;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, info, trace, warn};
// Fixed identifiers for structured trace logs
const CHAIN_ID: u32 = 501;
const PROCESS: u32 = 10010;
const PROCESS_WORD: &str = "node_e2e_shreds_parse";

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
enum ShredStatus {
    #[default]
    Unknown,
    // Shred that is **not** marked as [ShredFlags::DATA_COMPLETE_SHRED]
    NotDataComplete,
    /// Shred that is marked as [ShredFlags::DATA_COMPLETE_SHRED]
    DataComplete,
}

/// Tracks per-FEC-set shred information for data shreds
#[derive(Debug)]
pub struct SlotStateTracker {
    /// Compact status of each data shred for fast iteration.
    data_status: Vec<ShredStatus>,
    /// Data shreds received for the slot (not coding!)
    data_shreds: Vec<Option<Shred>>,
    /// array of bools that track which FEC set indexes have been already recovered
    already_recovered_fec_sets: Vec<bool>,
    /// array of bools that track which data shred indexes have been already deshredded
    already_deshredded: Vec<bool>,
}
impl Default for SlotStateTracker {
    fn default() -> Self {
        Self {
            data_status: vec![ShredStatus::Unknown; MAX_DATA_SHREDS_PER_SLOT],
            data_shreds: vec![None; MAX_DATA_SHREDS_PER_SLOT],
            already_recovered_fec_sets: vec![false; MAX_DATA_SHREDS_PER_SLOT],
            already_deshredded: vec![false; MAX_DATA_SHREDS_PER_SLOT],
        }
    }
}

pub async fn reconstruct_shreds_server(
    mut shutdown_rx: broadcast::Receiver<()>,
    reconstruct_tx: crossbeam_channel::Receiver<PacketBatch>,
    subscriptions: Arc<DashMap<String, Subscription>>,
    trace_log_path: Option<String>,
) -> anyhow::Result<()> {
    let exit = Arc::new(AtomicBool::new(false));

    // Setup log file if provided
    let trace_log_file = if let Some(ref path) = trace_log_path {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Some(Arc::new(Mutex::new(file)))
    } else {
        None
    };

    let handle = thread::spawn({
        let exit_clone = Arc::clone(&exit);
        move || {
            let mut all_slots = ahash::HashMap::<
                Slot,
                (
                    ahash::HashMap<u32, HashSet<ComparableShred>>,
                    SlotStateTracker,
                ),
            >::default();
            let mut slot_fec_indexes_to_iterate = Vec::<(Slot, u32)>::new();
            let mut highest_slot_seen: Slot = 0;
            let rs_cache = ReedSolomonCache::default();

            while !exit_clone.load(Ordering::Relaxed) {
                if let Ok(packet_batch) = reconstruct_tx.recv_timeout(Duration::from_millis(100)) {
                    reconstruct_shreds(
                        packet_batch,
                        &mut all_slots,
                        &mut slot_fec_indexes_to_iterate,
                        &mut highest_slot_seen,
                        &rs_cache,
                        &subscriptions,
                        &trace_log_file,
                    );
                }
            }
        }
    });

    shutdown_rx.recv().await?;
    exit.store(true, Ordering::Relaxed);

    handle.join().unwrap();
    Ok(())
}
const SLOT_LOOKBACK: Slot = 10;
pub fn reconstruct_shreds(
    packet_batch: PacketBatch,
    all_slots: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32 /* fec_set_index */, HashSet<ComparableShred>>,
            SlotStateTracker,
        ),
    >,
    slot_fec_indexes_set: &mut Vec<(Slot, u32)>,
    highest_slot_seen: &mut Slot,
    rs_cache: &ReedSolomonCache,
    subscriptions: &Arc<DashMap<String, Subscription>>,
    trace_log_file: &Option<Arc<Mutex<std::fs::File>>>,
) -> usize {
    slot_fec_indexes_set.clear();
    // ingest all packets
    for packet in packet_batch.iter() {
        // Use the exact packet payload length; Packet buffers are larger than the
        // actual UDP payload and using the full buffer can corrupt parsing.
        let Some(packet_data) = packet.data(..) else {
            continue;
        };
        let size = packet.meta().size.min(packet_data.len());
        if size == 0 {
            continue;
        }
        match solana_ledger::shred::Shred::new_from_serialized_shred(packet_data[..size].to_vec())
            .and_then(Shred::try_from)
        {
            Ok(shred) => {
                let slot = shred.common_header().slot;
                let index = shred.index() as usize;
                let fec_set_index = shred.fec_set_index();
                let (slot_map, state_tracker) = all_slots.entry(slot).or_default();
                if highest_slot_seen.saturating_sub(SLOT_LOOKBACK) > slot {
                    debug!(
                        "Old shred slot: {slot}, fec_set_index: {fec_set_index}, index: {index}"
                    );
                    continue;
                }
                if state_tracker.already_recovered_fec_sets[fec_set_index as usize]
                    || state_tracker.already_deshredded[index]
                {
                    debug!(
                        "Already completed slot: {slot}, fec_set_index: {fec_set_index}, index: {index}"
                    );
                    continue;
                }
                let Some(_shred_index) = update_state_tracker(&shred, state_tracker) else {
                    continue;
                };

                slot_map
                    .entry(fec_set_index)
                    .or_default()
                    .insert(ComparableShred(shred));

                slot_fec_indexes_set.push((slot, fec_set_index)); // use Vec so we can sort to make sure if any earlier FEC sets have DATA_SHRED_COMPLETE, later entries can use the flag to find the bounds
                *highest_slot_seen = std::cmp::max(*highest_slot_seen, slot);
            }
            Err(e) => {
                warn!("Failed to decode shred. Err: {e:?}");
            }
        }
    }
    slot_fec_indexes_set.sort_unstable();
    slot_fec_indexes_set.dedup();

    // try recovering by FEC set
    // already checked if FEC set is completed or deserialized
    let mut total_recovered_count = 0;
    for (slot, fec_set_index) in slot_fec_indexes_set.iter() {
        let (slot_map, state_tracker) = all_slots.entry(*slot).or_default();
        let fec_shreds = slot_map.entry(*fec_set_index).or_default();
        let (
            num_expected_data_shreds,
            num_expected_coding_shreds,
            num_data_shreds,
            num_coding_shreds,
        ) = get_data_shred_info(fec_shreds);

        // haven't received last data shred, haven't seen any coding shreds, so wait until more arrive
        let min_shreds_needed_to_recover = num_expected_data_shreds as usize;
        if num_expected_data_shreds == 0
            || fec_shreds.len() < min_shreds_needed_to_recover
            || num_data_shreds == num_expected_data_shreds
        {
            continue;
        }

        // try to recover if we have enough shreds in the FEC set
        // Sort with data shreds first, then coding shreds, and by index within type
        let merkle_shreds = fec_shreds
            .iter()
            .sorted_by_key(|s| (s.shred_type() as u8, s.index()))
            .map(|s| s.0.clone())
            .collect_vec();
        let recovered = match solana_ledger::shred::merkle::recover(merkle_shreds, rs_cache) {
            Ok(r) => r, // data shreds followed by code shreds (whatever was missing from to_deshred_payload)
            Err(e) => {
                warn!(
                    "Failed to recover shreds for slot {slot} fec_set_index {fec_set_index}. num_expected_data_shreds: {num_expected_data_shreds}, num_data_shreds: {num_data_shreds} num_expected_coding_shreds: {num_expected_coding_shreds} num_coding_shreds: {num_coding_shreds} Err: {e}",
                );
                continue;
            }
        };

        let mut fec_set_recovered_count = 0;
        for shred in recovered {
            match shred {
                Ok(shred) => {
                    if update_state_tracker(&shred, state_tracker).is_none() {
                        continue; // already seen before in state tracker
                    }
                    // shreds.insert(ComparableShred(shred)); // optional since all data shreds are in state_tracker
                    total_recovered_count += 1;
                    fec_set_recovered_count += 1;
                }
                Err(e) => warn!(
                    "Failed to recover shred for slot {slot}, fec set: {fec_set_index}. Err: {e}"
                ),
            }
        }

        if fec_set_recovered_count > 0 {
            debug!(
                "recovered slot: {slot}, fec_index: {fec_set_index}, recovered count: {fec_set_recovered_count}"
            );
            state_tracker.already_recovered_fec_sets[*fec_set_index as usize] = true;
            fec_shreds.clear();
        }
    }

    // deshred and bincode deserialize
    for (slot, fec_set_index) in slot_fec_indexes_set.iter() {
        let (_, slot_state_tracker) = all_slots.entry(*slot).or_default();
        let Some((start_data_complete_idx, end_data_complete_idx, unknown_start)) =
            get_indexes(slot_state_tracker, *fec_set_index as usize)
        else {
            continue;
        };
        // If the left boundary is unknown, we may be starting mid-entry; skip until we
        // have a confirmed left boundary to avoid noisy decode failures.
        if unknown_start {
            debug!(
                "Skipping deshred for slot {slot} fec_set_index {fec_set_index} due to unknown left boundary"
            );
            continue;
        }

        let to_deshred =
            &slot_state_tracker.data_shreds[start_data_complete_idx..=end_data_complete_idx];
        let deshredded_payload = match Shredder::deshred(
            to_deshred.iter().map(|s| s.as_ref().unwrap().payload()),
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "slot {slot} failed to deshred slot: {slot}, start_data_complete_idx: {start_data_complete_idx}, end_data_complete_idx: {end_data_complete_idx}. Err: {e}"
                );
                continue;
            }
        };

        let entries = match bincode::deserialize::<Vec<solana_entry::entry::Entry>>(
            &deshredded_payload,
        ) {
            Ok(entries) => entries,
            Err(e) => {
                warn!(
                    "slot {slot} failed to deserialize entries. start_idx: {start_data_complete_idx}, end_idx: {end_data_complete_idx}. Err: {e}"
                );
                continue;
            }
        };
        // Capture timestamp when we first receive transactions from shreds
        let shred_received_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        for entry in &entries {
            for tx in &entry.transactions {
                let tx_sig = if !tx.signatures.is_empty() {
                    tx.signatures[0].to_string()
                } else {
                    continue;
                };

                // Check if anyone is waiting for this transaction
                if let Some((_, subscription)) = subscriptions.remove(&tx_sig) {
                    // Compute latency metrics using shred received timestamp
                    let total_latency_ms =
                        shred_received_at_ms.saturating_sub(subscription.client_timestamp_ms);
                    let rpc_latency_ms = subscription
                        .rpc_received_ms
                        .saturating_sub(subscription.client_timestamp_ms);
                    let after_rpc_latency_ms =
                        shred_received_at_ms.saturating_sub(subscription.rpc_received_ms);

                    info!(
                        "Found transaction {} in slot {} (total: {}ms, rpc: {}ms, after_rpc: {}ms, client: {}ms, rpc_recv: {}ms, found: {}ms)",
                        tx_sig,
                        *slot,
                        total_latency_ms,
                        rpc_latency_ms,
                        after_rpc_latency_ms,
                        subscription.client_timestamp_ms,
                        subscription.rpc_received_ms,
                        shred_received_at_ms
                    );

                    // Write structured log to file if provided (use shred received timestamp)
                    if let Some(log_file) = trace_log_file {
                        // Format: chain,hash,status,serviceName,business,client,chainId,process,processWord,index,innerIndex,currentTime,referId,contractAddress,blockHeight
                        // Provided: chain=SOL, hash=tx_sig, chainId=CHAIN_ID, process=PROCESS, processWord=PROCESS_WORD, currentTime=shred_received_at_ms; others empty
                        let log_entry = format!(
                            "SOL,{},,,,{},{},{},,{},,,\n",
                            tx_sig, CHAIN_ID, PROCESS, PROCESS_WORD, shred_received_at_ms
                        );

                        if let Ok(mut file) = log_file.lock() {
                            if let Err(e) = file.write_all(log_entry.as_bytes()) {
                                warn!("Failed to write to trace log file: {}", e);
                            } else if let Err(e) = file.flush() {
                                warn!("Failed to flush trace log file: {}", e);
                            }
                        }
                    }

                    // Also use trace logging for the structured format (use shred received timestamp)
                    trace!(
                        "SOL,{},,,,{},{},{},,{},,,",
                        tx_sig, CHAIN_ID, PROCESS, PROCESS_WORD, shred_received_at_ms
                    );
                }
            }
        }

        let txn_count: u64 = entries.iter().map(|e| e.transactions.len() as u64).sum();
        debug!(
            "Successfully decoded slot: {slot} start_data_complete_idx: {start_data_complete_idx} end_data_complete_idx: {end_data_complete_idx} with entry count: {}, txn count: {txn_count}",
            entries.len(),
        );

        to_deshred.iter().for_each(|shred| {
            let Some(shred) = shred.as_ref() else {
                return;
            };
            slot_state_tracker.already_recovered_fec_sets[shred.fec_set_index() as usize] = true;
            slot_state_tracker.already_deshredded[shred.index() as usize] = true;
        })
    }

    if all_slots.len() > SLOT_LOOKBACK as usize {
        let slot_threshold = highest_slot_seen.saturating_sub(SLOT_LOOKBACK);
        let mut incomplete_fec_sets = ahash::HashMap::<Slot, Vec<_>>::default();
        let mut incomplete_fec_sets_count = 0;
        all_slots.retain(|slot, (fec_sets, slot_state_tracker)| {
            if *slot >= slot_threshold {
                return true;
            }

            // count missing fec sets before clearing
            for (fec_set_index, fec_set_shreds) in fec_sets.iter() {
                if slot_state_tracker.already_recovered_fec_sets[*fec_set_index as usize] {
                    continue;
                }
                let (num_expected_data_shreds, _, _, _) = get_data_shred_info(fec_set_shreds);

                incomplete_fec_sets_count += 1;
                incomplete_fec_sets
                    .entry(*slot)
                    .and_modify(|fec_set_data| {
                        fec_set_data.push((
                            *fec_set_index,
                            num_expected_data_shreds,
                            fec_set_shreds.len(),
                        ))
                    })
                    .or_insert_with(|| {
                        vec![(
                            *fec_set_index,
                            num_expected_data_shreds,
                            fec_set_shreds.len(),
                        )]
                    });
            }

            false
        });
        if incomplete_fec_sets_count > 0 {
            incomplete_fec_sets
                .iter_mut()
                .for_each(|(_slot, fec_set_indexes)| fec_set_indexes.sort_unstable());

            for (slot, fec_infos) in &incomplete_fec_sets {
                info!(
                    "Slot {} has {} incomplete FEC sets: {:?}",
                    slot,
                    fec_infos.len(),
                    fec_infos
                );
            }
        }
    }

    total_recovered_count
}

/// Test-only helper: scan a slice of entries for subscribed transactions
/// and remove matched subscriptions. Returns the list of matched signatures.
/// This bypasses UDP/shred decoding and is used by integration tests.
#[cfg(test)]
pub fn scan_entries_for_subscriptions_for_test(
    entries: &[solana_entry::entry::Entry],
    subscriptions: &DashMap<String, Subscription>,
) -> Vec<String> {
    let shred_received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut matched = Vec::new();
    for entry in entries {
        for tx in &entry.transactions {
            if tx.signatures.is_empty() {
                continue;
            }
            let tx_sig = tx.signatures[0].to_string();
            if let Some((_, subscription)) = subscriptions.remove(&tx_sig) {
                let _total_latency_ms =
                    shred_received_at_ms.saturating_sub(subscription.client_timestamp_ms);
                matched.push(tx_sig);
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::scan_entries_for_subscriptions_for_test;
    use crate::rpc::{Rpc, RpcImpl, Subscription};
    use dashmap::DashMap;
    use jsonrpc_core::Params;
    use serde_json::{Value, json};

    use solana_sdk::{
        hash::Hash,
        instruction::Instruction,
        message::Message,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        transaction::Transaction,
    };
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[test]
    fn subscribe_and_detect_sol_transfer() {
        // Construct a simple SOL transfer transaction
        let from = Keypair::new();
        let to = Keypair::new();
        // Create a simple transfer instruction manually
        // System program ID: 11111111111111111111111111111111
        let system_program_id = Pubkey::new_from_array([0u8; 32]);
        let ix = Instruction::new_with_bincode(
            system_program_id,
            &(2u32, from.pubkey(), to.pubkey(), 1u64), // SystemInstruction::Transfer
            vec![
                solana_sdk::instruction::AccountMeta::new(from.pubkey(), true),
                solana_sdk::instruction::AccountMeta::new(to.pubkey(), false),
            ],
        );
        let message = Message::new(&[ix], Some(&from.pubkey()));
        let recent_blockhash = Hash::new_unique();
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(&[&from], recent_blockhash).expect("sign");
        let tx_sig_str = tx.signatures[0].to_string();

        // Build a minimal entry containing the transaction
        let prev_hash = Hash::new_unique();
        let entry = solana_entry::entry::Entry::new(&prev_hash, 0, vec![tx.clone()]);
        let entries = vec![entry];

        // Create RPC with shared subscriptions map
        let subscriptions = Arc::new(DashMap::<String, Subscription>::new());
        let rpc = RpcImpl {
            subscriptions: subscriptions.clone(),
        };

        // jsonrpc-derive unwraps array-of-one on the wire and passes a Map here
        let params_map: Value = json!({
            "tx_sig": tx_sig_str,
            "timestamp": now_ms(),
        });
        let params = match params_map {
            Value::Object(map) => Params::Map(map),
            _ => unreachable!(),
        };
        let resp = rpc.subscribe_tx(params).expect("rpc subscribe ok");
        assert_eq!(resp.status, "subscribed");
        assert!(subscriptions.contains_key(&tx_sig_str));

        // Feed the entries into the scan helper to simulate detection
        let matched = scan_entries_for_subscriptions_for_test(&entries, &subscriptions);
        assert!(matched.contains(&tx_sig_str));
        assert!(!subscriptions.contains_key(&tx_sig_str));
    }
}

fn get_data_shred_info(
    shreds: &HashSet<ComparableShred>,
) -> (
    u16, /* num_expected_data_shreds */
    u16, /* num_expected_coding_shreds */
    u16, /* num_data_shreds */
    u16, /* num_coding_shreds */
) {
    let mut num_expected_data_shreds = 0;
    let mut num_expected_coding_shreds = 0;
    let mut num_data_shreds = 0;
    let mut num_coding_shreds = 0;
    for shred in shreds {
        match &shred.0 {
            Shred::ShredCode(s) => {
                num_coding_shreds += 1;
                num_expected_data_shreds = s.coding_header.num_data_shreds;
                num_expected_coding_shreds = s.coding_header.num_coding_shreds;
            }
            Shred::ShredData(s) => {
                num_data_shreds += 1;
                if num_expected_data_shreds == 0 && (s.data_complete() || s.last_in_slot()) {
                    num_expected_data_shreds =
                        (shred.0.index() - shred.0.fec_set_index()) as u16 + 1;
                }
            }
        }
    }
    (
        num_expected_data_shreds,
        num_expected_coding_shreds,
        num_data_shreds,
        num_coding_shreds,
    )
}

fn update_state_tracker(shred: &Shred, state_tracker: &mut SlotStateTracker) -> Option<usize> {
    let index = shred.index() as usize;
    if state_tracker.already_recovered_fec_sets[shred.fec_set_index() as usize] {
        return None;
    }
    if shred.shred_type() == ShredType::Data
        && (state_tracker.data_shreds[index].is_some()
            || !matches!(state_tracker.data_status[index], ShredStatus::Unknown))
    {
        return None;
    }
    if let Shred::ShredData(s) = &shred {
        state_tracker.data_shreds[index] = Some(shred.clone());
        if s.data_complete() || s.last_in_slot() {
            state_tracker.data_status[index] = ShredStatus::DataComplete;
        } else {
            state_tracker.data_status[index] = ShredStatus::NotDataComplete;
        }
    };
    Some(index)
}

fn get_indexes(
    tracker: &SlotStateTracker,
    index: usize,
) -> Option<(
    usize, /* start_data_complete_idx */
    usize, /* end_data_complete_idx */
    bool,  /* unknown start index */
)> {
    if index >= tracker.data_status.len() {
        return None;
    }

    // find the right boundary (first DataComplete ≥ index)
    let mut end = index;
    while end < tracker.data_status.len() {
        if tracker.already_deshredded[end] {
            return None;
        }
        match &tracker.data_status[end] {
            ShredStatus::Unknown => return None,
            ShredStatus::DataComplete => break,
            ShredStatus::NotDataComplete => end += 1,
        }
    }
    if end == tracker.data_status.len() {
        return None; // never saw a DataComplete
    }

    if end == 0 {
        return Some((0, 0, false)); // the vec *starts* with DataComplete
    }
    if index == 0 {
        return Some((0, end, false));
    }

    // find the left boundary (prev DataComplete + 1)
    let mut start = index;
    let mut next = start - 1;
    loop {
        match tracker.data_status[next] {
            ShredStatus::NotDataComplete => {
                if tracker.already_deshredded[next] {
                    return None; // already covered by some other iteration
                }
                if next == 0 {
                    return Some((0, end, false)); // no earlier DataComplete
                }
                start = next;
                next -= 1;
            }
            ShredStatus::DataComplete => return Some((start, end, false)),
            ShredStatus::Unknown => return Some((start, end, true)), // sometimes we don't have the previous starting shreds, make best guess
        }
    }
}

#[derive(Clone, Debug, Eq)]
pub struct ComparableShred(Shred);

impl std::ops::Deref for ComparableShred {
    type Target = Shred;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Hash for ComparableShred {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.0 {
            Shred::ShredCode(s) => {
                s.common_header.hash(state);
                s.coding_header.hash(state);
            }
            Shred::ShredData(s) => {
                s.common_header.hash(state);
                s.data_header.hash(state);
            }
        }
    }
}

impl PartialEq for ComparableShred {
    // Custom comparison to avoid random bytes that are part of payload
    fn eq(&self, other: &Self) -> bool {
        match &self.0 {
            Shred::ShredCode(s1) => match &other.0 {
                Shred::ShredCode(s2) => {
                    let solana_ledger::shred::ShredVariant::MerkleCode {
                        proof_size,
                        chained: _,
                        resigned,
                    } = s1.common_header.shred_variant
                    else {
                        return false;
                    };

                    // see https://github.com/jito-foundation/jito-solana/blob/d6c73374e3b4f863436e4b7d4d1ce5eea01cd262/ledger/src/shred/merkle.rs#L346, and re-add the proof component
                    let comparison_len =
                        <ShredCode as solana_ledger::shred::traits::Shred>::SIZE_OF_PAYLOAD
                            .saturating_sub(
                                usize::from(proof_size)
                                    * solana_ledger::shred::merkle::SIZE_OF_MERKLE_PROOF_ENTRY
                                    + if resigned {
                                        solana_ledger::shred::SIZE_OF_SIGNATURE
                                    } else {
                                        0
                                    },
                            );

                    s1.coding_header == s2.coding_header
                        && s1.common_header == s2.common_header
                        && s1.payload[..comparison_len] == s2.payload[..comparison_len]
                }
                Shred::ShredData(_) => false,
            },
            Shred::ShredData(s1) => match &other.0 {
                Shred::ShredCode(_) => false,
                Shred::ShredData(s2) => {
                    let Ok(s1_data) = solana_ledger::shred::layout::get_data(self.payload()) else {
                        return false;
                    };
                    let Ok(s2_data) = solana_ledger::shred::layout::get_data(other.payload())
                    else {
                        return false;
                    };
                    s1.data_header == s2.data_header
                        && s1.common_header == s2.common_header
                        && s1_data == s2_data
                }
            },
        }
    }
}
