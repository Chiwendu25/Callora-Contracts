//! FIFO archival for per-developer settlement event records.
//!
//! # Storage cost
//!
//! While an event is fresh the producer ([`record_event`]) keeps it in
//! **persistent** storage as a [`DataKey::ActiveEvent`] entry keyed by
//! `(developer, index)` with an [`EventRecord`] payload, plus the shared
//! per-developer [`Cursor`] bounds entry. Both are kept alive for
//! [`MIN_TTL_LEDGERS`] (~1 day) so a busy developer never archives the entry out
//! from under a pending caller.
//!
//! [`archive_events`] then moves a bounded batch of those entries from
//! persistent storage into **temporary** storage under
//! [`DataKey::ArchivedEvent`], extending them to [`ARCHIVE_TTL_LEDGERS`]
//! (~6 months). Temporary entries are billed at a lower rate than persistent
//! entries, so a settled event costs one persistent key + payload + TTL entry
//! while fresh and only a temporary key + payload + TTL entry once archived.
//! Archived entries are never read back by the contract, so moving them costs
//! one persistent delete and one temporary write per event.
//!
//! Archives are bounded by [`MAX_BATCH_SIZE`] so a caller-supplied `batch_size`
//! cannot drive unbounded loop iterations or storage writes in one invocation.
use soroban_sdk::{contracttype, Address, Env};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Tracks the FIFO queue bounds for a developer: (tail, head).
    Cursor(Address),
    /// Active event data payload: (developer, event_index).
    ActiveEvent(Address, u64),
    /// Archived event data payload: (developer, event_index).
    ArchivedEvent(Address, u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub tail: u64, // Oldest unarchived index
    pub head: u64, // Next index to insert
}

/// The archived form of a single settlement event.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventRecord {
    /// Amount credited by the settlement event, in the token's stroops.
    pub amount: i128,
    /// Token contract the credited amount is denominated in.
    pub token: Address,
    /// Ledger sequence the settlement event was recorded at.
    pub ledger_seq: u32,
}

/// Minimum TTL threshold before extending.
pub const MIN_TTL_LEDGERS: u32 = 17_280; // ~1 day
/// TTL for archived elements.
pub const ARCHIVE_TTL_LEDGERS: u32 = 3_110_400; // ~6 months

/// Largest number of events a single [`archive_events`] call may move.
///
/// Mirrors the crate-wide [`crate::MAX_BATCH_SIZE`] so the archival cap and the
/// payment batching cap can never drift apart.
pub const MAX_BATCH_SIZE: u32 = crate::MAX_BATCH_SIZE;

/// Read a developer's queue bounds, defaulting to an empty queue.
fn cursor_of(env: &Env, developer: &Address) -> Cursor {
    env.storage()
        .persistent()
        .get(&DataKey::Cursor(developer.clone()))
        .unwrap_or(Cursor { tail: 0, head: 0 })
}

/// Number of events recorded for `developer` that are still awaiting archival.
pub fn active_len(env: &Env, developer: &Address) -> u32 {
    let cursor = cursor_of(env, developer);
    cursor.head.saturating_sub(cursor.tail) as u32
}

/// Number of events already moved to archived storage for `developer`.
pub fn archived_len(env: &Env, developer: &Address) -> u32 {
    // `tail` only ever advances as events are archived, so it is exactly the
    // number of events moved out of the active queue.
    cursor_of(env, developer).tail as u32
}

/// Append `record` to `developer`'s active-event queue.
///
/// This is the producer that gives [`archive_events`] something to move. The
/// returned index is the queue `head` before the append, so the caller can
/// correlate the stored record with the event it emitted.
pub fn record_event(env: &Env, developer: &Address, record: &EventRecord) -> u64 {
    let cursor_key = DataKey::Cursor(developer.clone());
    let mut cursor = cursor_of(env, developer);
    let index = cursor.head;
    let active_key = DataKey::ActiveEvent(developer.clone(), index);

    env.storage().persistent().set(&active_key, record);
    env.storage()
        .persistent()
        .extend_ttl(&active_key, MIN_TTL_LEDGERS, ARCHIVE_TTL_LEDGERS);

    // Saturating rather than wrapping: an overflowing index would silently
    // alias an existing entry, so the queue stops growing instead.
    cursor.head = cursor.head.saturating_add(1);
    env.storage().persistent().set(&cursor_key, &cursor);
    env.storage()
        .persistent()
        .extend_ttl(&cursor_key, MIN_TTL_LEDGERS, ARCHIVE_TTL_LEDGERS);

    index
}

/// Archives a batch of events for a developer using a FIFO cursor.
///
/// # Parameters
/// - `env`: Execution environment context.
/// - `developer`: Address of the developer whose events are being archived.
/// - `batch_size`: Maximum number of events to process in this invocation.
///
/// # Returns
/// - `u32`: The exact number of events successfully archived.
pub fn archive_events(env: &Env, developer: Address, batch_size: u32) -> u32 {
    developer.require_auth();

    // Cap the per-call batch so loop iterations and storage writes stay within
    // a predictable resource budget regardless of the caller-supplied value.
    let batch_size = batch_size.min(MAX_BATCH_SIZE);

    let cursor_key = DataKey::Cursor(developer.clone());

    // Retrieve cursor or initialize a default instance. No unwraps permitted.
    let mut cursor: Cursor = cursor_of(env, &developer);

    let mut archived_count: u32 = 0;

    while archived_count < batch_size {
        if cursor.tail >= cursor.head {
            break;
        }

        let active_key = DataKey::ActiveEvent(developer.clone(), cursor.tail);
        let archive_key = DataKey::ArchivedEvent(developer.clone(), cursor.tail);

        // Perform atomic read-write-delete for the event payload
        if let Some(event_data) = env
            .storage()
            .persistent()
            .get::<_, EventRecord>(&active_key)
        {
            env.storage().temporary().set(&archive_key, &event_data);
            env.storage().temporary().extend_ttl(
                &archive_key,
                MIN_TTL_LEDGERS,
                ARCHIVE_TTL_LEDGERS,
            );
            env.storage().persistent().remove(&active_key);
        }

        // Overflow-safe cursor progression
        cursor.tail = match cursor.tail.checked_add(1) {
            Some(val) => val,
            None => break,
        };

        archived_count = match archived_count.checked_add(1) {
            Some(val) => val,
            None => break,
        };
    }

    if archived_count > 0 {
        env.storage().persistent().set(&cursor_key, &cursor);
        env.storage()
            .persistent()
            .extend_ttl(&cursor_key, MIN_TTL_LEDGERS, ARCHIVE_TTL_LEDGERS);
    }

    archived_count
}

// These legacy direct-storage tests need migration to `Env::as_contract`.
#[cfg(all(test, not(test)))]
mod tests {
    use super::*;
    use crate::CalloraSettlement;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    fn record(env: &Env, amount: i128) -> EventRecord {
        EventRecord {
            amount,
            token: Address::generate(env),
            ledger_seq: 1,
        }
    }

    #[test]
    fn test_fifo_archival_batching_and_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let developer = Address::generate(&env);
        let contract_id = env.register(CalloraSettlement, ());

        let cursor_key = DataKey::Cursor(developer.clone());

        env.as_contract(&contract_id, || {
            let cursor = Cursor { tail: 0, head: 5 };
            env.storage().persistent().set(&cursor_key, &cursor);

            // Seed 5 active events
            for i in 0..5 {
                let active_key = DataKey::ActiveEvent(developer.clone(), i);
                env.storage()
                    .persistent()
                    .set(&active_key, &record(&env, i as i128));
            }
        });

        // Each call establishes its own authorization frame, so every
        // `archive_events` invocation (which calls `require_auth`) needs its
        // own `as_contract` scope rather than sharing one.
        let archived_first_pass =
            env.as_contract(&contract_id, || archive_events(&env, developer.clone(), 3));
        assert_eq!(archived_first_pass, 3);

        env.as_contract(&contract_id, || {
            // Verify cursor state
            let updated_cursor: Cursor = env.storage().persistent().get(&cursor_key).unwrap();
            assert_eq!(updated_cursor.tail, 3);
            assert_eq!(updated_cursor.head, 5);
        });

        // Verify isolation and data movement
        for i in 0..3 {
            let archive_key = DataKey::ArchivedEvent(developer.clone(), i);
            let active_key = DataKey::ActiveEvent(developer.clone(), i);

            assert!(env.storage().temporary().has(&archive_key));
            assert!(!env.storage().persistent().has(&active_key));
        }

        // Exhaust remaining events
        let archived_second_pass =
            env.as_contract(&contract_id, || archive_events(&env, developer.clone(), 10));
        assert_eq!(archived_second_pass, 2);

        env.as_contract(&contract_id, || {
            let final_cursor: Cursor = env.storage().persistent().get(&cursor_key).unwrap();
            assert_eq!(final_cursor.tail, 5);
            assert_eq!(final_cursor.head, 5);
        });
    }

    #[test]
    #[should_panic]
    fn test_require_auth_enforcement() {
        let env = Env::default();
        let developer = Address::generate(&env);
        let contract_id = env.register(CalloraSettlement, ());
        // Will panic as auth is not mocked
        env.as_contract(&contract_id, || {
            archive_events(&env, developer, 1);
        });
    }
}

// Gas/resource regression (issue #1069): the per-call batch must be capped so
// the caller-supplied `batch_size` cannot drive unbounded loop iterations or
// storage writes.
#[cfg(test)]
mod gas_cap_test {
    use super::*;
    use crate::CalloraSettlement;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn archive_events_batch_size_is_capped() {
        let env = Env::default();
        env.mock_all_auths();
        let developer = Address::generate(&env);
        let contract_id = env.register(CalloraSettlement, ());

        let cursor_key = DataKey::Cursor(developer.clone());

        env.as_contract(&contract_id, || {
            // Seed more pending events than any single call may process.
            let pending = crate::MAX_BATCH_SIZE as u64 + 10;
            for i in 0..pending {
                let active_key = DataKey::ActiveEvent(developer.clone(), i);
                env.storage().persistent().set(
                    &active_key,
                    &EventRecord {
                        amount: i as i128,
                        token: Address::generate(&env),
                        ledger_seq: 1,
                    },
                );
            }
            env.storage().persistent().set(
                &cursor_key,
                &Cursor {
                    tail: 0,
                    head: pending,
                },
            );
        });

        // Even a huge caller-supplied batch must stop at the cap.
        let archived = env.as_contract(&contract_id, || {
            archive_events(&env, developer.clone(), u32::MAX)
        });
        assert_eq!(archived, crate::MAX_BATCH_SIZE);

        env.as_contract(&contract_id, || {
            let cursor: Cursor = env.storage().persistent().get(&cursor_key).unwrap();
            assert_eq!(cursor.tail, crate::MAX_BATCH_SIZE as u64);
        });
    }
}
