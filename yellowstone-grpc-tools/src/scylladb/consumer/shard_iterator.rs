use core::fmt;
use std::{collections::VecDeque, sync::Arc, time::Duration};

use scylla::{prepared_statement::PreparedStatement, routing::Shard, Session};
use tokio::{sync::oneshot::{self, error::TryRecvError}, time::Instant};
use tracing::{debug, info};

use crate::scylladb::types::{BlockchainEvent, BlockchainEventType, ProducerId, ShardId, ShardOffset, ShardPeriod, SHARD_OFFSET_MODULO};

const MICRO_BATCH_SIZE: usize = 40;

pub const GET_NEW_TRANSACTION_EVENT: &str = r###"
    SELECT
        shard_id,
        period,
        producer_id,
        offset,
        slot,
        event_type,

        pubkey,
        lamports,
        owner,
        executable,
        rent_epoch,
        write_version,
        data,
        txn_signature,

        signature,
        signatures,
        num_required_signatures,
        num_readonly_signed_accounts,
        num_readonly_unsigned_accounts,
        account_keys,
        recent_blockhash,
        instructions,
        versioned,
        address_table_lookups,
        meta,
        is_vote,
        tx_index
    FROM log
    WHERE producer_id = ? and shard_id = ? and offset > ? and period = ?
    and event_type = 1
    ORDER BY offset ASC
    ALLOW FILTERING
"###;

const PRODUCER_SHARD_PERIOD_COMMIT_EXISTS: &str = r###"
    SELECT
        producer_id
    FROM producer_period_commit_log
    WHERE 
        producer_id = ?
        AND shard_id = ?
        AND period = ?
"###;


/// Empty : the shard iterator is either brand new or no more row are available in its inner row stream.
/// Loading : We asked for a row iterator that may take some time to resolve but we don't want to block a consumer.
/// Available: the inner row stream is available to stream event.
/// EndOfPeriod : No more data for the current "period", we need to go back to the end Empty tate.
enum ShardIteratorState {
    Empty(ShardOffset),
    Loading(ShardOffset, oneshot::Receiver<VecDeque<BlockchainEvent>>),
    Loaded(ShardOffset, VecDeque<BlockchainEvent>),
    ConfirmingPeriod(ShardOffset, oneshot::Receiver<bool>),
    Streaming(ShardOffset, VecDeque<BlockchainEvent>),
    WaitingEndOfPeriod(ShardOffset, oneshot::Receiver<bool>),
}

impl fmt::Debug for ShardIteratorState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(arg0) => f.debug_tuple("Empty").field(arg0).finish(),
            Self::Loading(arg0, _) => f.debug_tuple("Loading").field(arg0).finish(),
            Self::Loaded(arg0, _) => f.debug_tuple("Loading").field(arg0).finish(),
            Self::ConfirmingPeriod(arg0, _) => f.debug_tuple("Loading").field(arg0).finish(),
            Self::Streaming(arg0, _) => f.debug_tuple("Available").field(arg0).finish(),
            Self::WaitingEndOfPeriod(arg0, _) => f.debug_tuple("EndOfPeriod").field(arg0).finish(),
        }
    }
}

impl ShardIteratorState {
    fn last_offset(&self) -> ShardOffset {
        match self {
            Self::Empty(offset) => *offset,
            Self::Loading(offset, _) => *offset,
            Self::Loaded(offset, _) => *offset,
            Self::ConfirmingPeriod(offset, _) => *offset,
            Self::Streaming(offset, _) => *offset,
            Self::WaitingEndOfPeriod(offset, _) => *offset,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            ShardIteratorState::Empty(_) => true,
            _ => false
        }
    }
}



#[derive(Clone, Default)]
pub(crate) struct ShardFilter {
    pub(crate) tx_account_keys: Vec<Vec<u8>>,
    pub(crate) account_owners: Vec<Vec<u8>>,
    pub(crate) account_pubkyes: Vec<Vec<u8>>,
}


pub(crate) struct ShardIterator {
    session: Arc<Session>,
    pub(crate) producer_id: ProducerId,
    pub(crate) shard_id: ShardId,
    inner: ShardIteratorState,
    pub(crate) event_type: BlockchainEventType,
    get_events_prepared_stmt: PreparedStatement,
    period_commit_exists_prepared_stmt: PreparedStatement,
    last_period_confirmed: ShardPeriod,
    filter: ShardFilter,
}



impl ShardIterator {
    pub(crate) async fn new(
        session: Arc<Session>,
        producer_id: ProducerId,
        shard_id: ShardId,
        offset: ShardOffset,
        event_type: BlockchainEventType,
        filter: Option<ShardFilter>,
    ) -> anyhow::Result<Self> {
        let mut get_events_ps = if event_type == BlockchainEventType::AccountUpdate {
            let query_str = forge_account_upadate_event_query(filter.clone().unwrap_or_default());
            session.prepare(query_str).await?
        } else {
            session.prepare(GET_NEW_TRANSACTION_EVENT).await?
        };

        let period_commit_exists_ps = session.prepare(PRODUCER_SHARD_PERIOD_COMMIT_EXISTS).await?;

        Ok(ShardIterator {
            session,
            producer_id,
            shard_id,
            inner: ShardIteratorState::Empty(offset),
            event_type,
            get_events_prepared_stmt: get_events_ps,
            period_commit_exists_prepared_stmt: period_commit_exists_ps,
            last_period_confirmed: (offset / SHARD_OFFSET_MODULO) - 1,
            filter: filter.unwrap_or_default(),
        })
    }

    pub(crate) fn last_offset(&self) -> ShardOffset {
        self.inner.last_offset()
    }

    ///
    /// If the state of the shard iterator is [[`ShardIteratorState::Empty`]] it loads the scylladb row iterator, otherwise nothing.
    pub(crate) async fn warm(&mut self) -> anyhow::Result<()> {
        if !self.inner.is_empty() {
            return Ok(())
        }
        let last_offset = self.inner.last_offset();

        let micro_batch = self.fetch_micro_batch(last_offset).await?;
        let new_state = ShardIteratorState::Streaming(last_offset, micro_batch);
        self.inner = new_state;
        Ok(())
    }

    fn is_period_committed(&self, last_offset: ShardOffset) -> oneshot::Receiver<bool> {
        let session = Arc::clone(&self.session);
        let producer_id = self.producer_id;
        let ps = self.period_commit_exists_prepared_stmt.clone();
        let shard_id = self.shard_id;
        let period = last_offset / SHARD_OFFSET_MODULO;
        let (sender, receiver) = oneshot::channel();
        let _handle: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let result = session
                .execute(&ps, (producer_id, shard_id, period))
                .await?
                .maybe_first_row()?
                .map(|_row| true)
                .unwrap_or(false);
            sender.send(result).map_err(|_| anyhow::anyhow!("failed to send back period commit status to shard iterator {}", shard_id))?;
            Ok(())
        });
        receiver
    }

    fn fetch_micro_batch(&self, last_offset: ShardOffset) -> oneshot::Receiver<VecDeque<BlockchainEvent>> {
        let period = (last_offset + 1) / SHARD_OFFSET_MODULO;
        let producer_id = self.producer_id;
        let ps = self.get_events_prepared_stmt.clone();
        let shard_id = self.shard_id;
        let session = Arc::clone(&self.session);
        let (sender, receiver) = oneshot::channel();
        let _: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let micro_batch = session
                .execute(&ps, (producer_id, shard_id, last_offset, period))
                .await?
                .rows_typed_or_empty::<BlockchainEvent>().collect::<Result<VecDeque<_>, _>>()?;
            sender.send(micro_batch).map_err(|_| anyhow::anyhow!("Failed to send micro batch to shard iterator {}", shard_id))?;
            Ok(())
        });
        receiver
    }

    ///
    /// Apply any filter that can not be pushdown to scylladb
    /// 
    fn filter_row(&self, row: BlockchainEvent) -> Option<BlockchainEvent> {
        if row.event_type == BlockchainEventType::NewTransaction {
            // Apply transaction filter here
            let elligible_acc_keys = &self.filter.tx_account_keys;
            if !elligible_acc_keys.is_empty() {
                let is_row_elligible = row.account_keys
                    .as_ref()
                    .filter(|actual_keys| 
                        actual_keys
                            .iter()
                            .find(|account_key| elligible_acc_keys.contains(account_key))
                            .is_some()
                    )
                    .map(|_| true)
                    .unwrap_or(false);
                if !is_row_elligible {
                    return None;
                }
            }
        }

        Some(row)
    }

    pub(crate) async fn try_next(&mut self) -> anyhow::Result<Option<BlockchainEvent>> {
        let last_offset = self.inner.last_offset();
        let current_state =
            std::mem::replace(&mut self.inner, ShardIteratorState::Empty(last_offset));

        let (next_state, maybe_to_return) = match current_state {
            ShardIteratorState::Empty(last_offset) => {
                let receiver = self.fetch_micro_batch(last_offset);
                (ShardIteratorState::Loading(last_offset, receiver), None)
            },
            ShardIteratorState::Loading(last_offset, mut receiver) => {
                let result = receiver.try_recv();
                match result {
                    Err(TryRecvError::Empty) => (ShardIteratorState::Loading(last_offset, receiver), None),
                    Err(TryRecvError::Closed) => anyhow::bail!("failed to receive micro batch"),
                    Ok(micro_batch) => {
                        (ShardIteratorState::Loaded(last_offset, micro_batch), None)
                    } 
                }
            },
            ShardIteratorState::Loaded(last_offset, mut micro_batch) => {
                let maybe_row = micro_batch.pop_front();
                if let Some(row) = maybe_row  {
                    (ShardIteratorState::Streaming(row.offset, micro_batch), Some(row))
                } else {
                    let curr_period = last_offset / SHARD_OFFSET_MODULO;
                    if curr_period <= self.last_period_confirmed {
                        let last_period_offset = ((curr_period + 1) * SHARD_OFFSET_MODULO) - 1;
                        (ShardIteratorState::Empty(last_period_offset), None)
                    } else {
                        // If a newly loaded row stream is already empty, we must figure out if
                        // its because there no more data in the period or is it because we consume too fast and we should try again later.
                        let receiver = self.is_period_committed(last_offset);
                        (ShardIteratorState::ConfirmingPeriod(last_offset, receiver), None)
                    }
                } 
            }
            ShardIteratorState::ConfirmingPeriod(last_offset, mut rx) => {
                match rx.try_recv() {
                    Err(TryRecvError::Empty) => (ShardIteratorState::ConfirmingPeriod(last_offset, rx), None),
                    Err(TryRecvError::Closed) => anyhow::bail!("fail"),
                    Ok(period_committed) => {
                        if period_committed {
                            self.last_period_confirmed = last_offset / SHARD_OFFSET_MODULO;
                        }
                        (ShardIteratorState::Empty(last_offset), None)
                    } 
                }
            }
            ShardIteratorState::Streaming(last_offset, mut micro_batch) => {
                let maybe_row = micro_batch.pop_front();
                if let Some(row) = maybe_row {
                    (ShardIteratorState::Streaming(row.offset, micro_batch), Some(row))
                } else {
                    if (last_offset + 1) % SHARD_OFFSET_MODULO == 0 {
                        let receiver = self.is_period_committed(last_offset);
                        (ShardIteratorState::WaitingEndOfPeriod(last_offset, receiver), None)
                    } else {
                        (ShardIteratorState::Empty(last_offset), None)
                    }
                }
            },
            ShardIteratorState::WaitingEndOfPeriod(last_offset, mut rx) => {
                match rx.try_recv() {
                    Err(TryRecvError::Empty) => (ShardIteratorState::WaitingEndOfPeriod(last_offset, rx), None),
                    Err(TryRecvError::Closed) => anyhow::bail!("fail"),
                    Ok(period_committed) => {
                        if period_committed {
                            self.last_period_confirmed = last_offset / SHARD_OFFSET_MODULO;
                            (ShardIteratorState::Empty(last_offset), None)
                        } else {
                            // Renew the background task
                            let rx2 = self.is_period_committed(last_offset);
                            (ShardIteratorState::WaitingEndOfPeriod(last_offset, rx2), None)
                        }
                    } 
                }
            }
        };
        let _ = std::mem::replace(&mut self.inner, next_state);
        Ok(maybe_to_return.and_then(|row| self.filter_row(row)))
    }
}


const LOG_PRIMARY_KEY_CONDITION: &str = r###"
    producer_id = ? and shard_id = ? and offset > ? and period = ?
"###;

const LOG_PROJECTION: &str = r###"
    shard_id,
    period,
    producer_id,
    offset,
    slot,
    event_type,
    pubkey,
    lamports,
    owner,
    executable,
    rent_epoch,
    write_version,
    data,
    txn_signature,
    signature,
    signatures,
    num_required_signatures,
    num_readonly_signed_accounts,
    num_readonly_unsigned_accounts,
    account_keys,
    recent_blockhash,
    instructions,
    versioned,
    address_table_lookups,
    meta,
    is_vote,
    tx_index
"###;


fn format_as_scylla_hexstring(bytes: &[u8]) -> String{
    if bytes.len() == 0 {
        panic!("byte slice is empty")
    }
    let hex = bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>().join("");
    format!("0x{}", hex)
}

fn forge_account_upadate_event_query(filter: ShardFilter) -> String {
    let mut conds = vec![];

    let pubkeys = filter.account_pubkyes
        .iter()
        .map(|pubkey| format_as_scylla_hexstring(pubkey.as_slice()))
        .collect::<Vec<_>>();

    let owners = filter.account_owners
        .iter()
        .map(|owner| format_as_scylla_hexstring(owner.as_slice()))
        .collect::<Vec<_>>();


    if !pubkeys.is_empty() {
        let cond = format!("AND pubkey IN ({})", pubkeys.join(", "));
        conds.push(cond);
    }
    if !owners.is_empty() {
        let cond = format!("AND owner IN ({})", owners.join(", "));
        conds.push(cond)
    }
    let conds_string = conds.join(" ");

    format!(
        r###"
        SELECT
        {projection}
        FROM log
        WHERE {primary_key_cond}
        AND event_type = 0
        {other_conds}
        ORDER BY offset ASC
        LIMIT {batch_size}
        ALLOW FILTERING
        "###,
        projection = LOG_PROJECTION,
        primary_key_cond = LOG_PRIMARY_KEY_CONDITION,
        other_conds = conds_string,
        batch_size = MICRO_BATCH_SIZE,
    )
}

