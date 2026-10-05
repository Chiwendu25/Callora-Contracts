extern crate std;

use crate::archive::{self, Cursor, DataKey, EventRecord, MAX_BATCH_SIZE};
use crate::{CalloraSettlement, CalloraSettlementClient};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};

/// Initialise a settlement contract and register `token` so `receive_payment`
/// accepts it. Returns `(vault, token, contract address, client)`.
fn setup(env: &Env) -> (Address, Address, Address, CalloraSettlementClient<'_>) {
    let admin = Address::generate(env);
    let vault = Address::generate(env);
    let token = Address::generate(env);
    let addr = env.register(CalloraSettlement, ());
    let client = CalloraSettlementClient::new(env, &addr);
    client.init(&admin, &vault);
    client.add_supported_token(&admin, &token);
    (vault, token, addr, client)
}

/// Credit `amount` to `developer` at ledger sequence `seq`.
fn credit(
    client: &CalloraSettlementClient<'_>,
    vault: &Address,
    token: &Address,
    developer: &Address,
    amount: i128,
    seq: u32,
) {
    client.receive_payment(
        vault,
        &amount,
        &false,
        &Some(developer.clone()),
        token,
        &seq,
    );
}

#[test]
fn receive_payment_records_one_active_event_per_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, addr, client) = setup(&env);
    let developer = Address::generate(&env);

    // Ledger sequences must strictly increase: sequence 0 is treated as a
    // replay of the guard's default, so start at 1.
    for seq in 1..4u32 {
        credit(&client, &vault, &token, &developer, 100, seq);
    }

    env.as_contract(&addr, || {
        assert_eq!(archive::active_len(&env, &developer), 3);
        assert_eq!(archive::archived_len(&env, &developer), 0);
    });
}

#[test]
fn archive_events_moves_active_to_archived() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, addr, client) = setup(&env);
    let developer = Address::generate(&env);

    credit(&client, &vault, &token, &developer, 100, 1);
    credit(&client, &vault, &token, &developer, 200, 2);

    env.as_contract(&addr, || {
        assert_eq!(archive::active_len(&env, &developer), 2);
        assert_eq!(archive::archived_len(&env, &developer), 0);
    });

    let archived = client.archive_events(&developer, &MAX_BATCH_SIZE);
    assert_eq!(archived, 2);

    env.as_contract(&addr, || {
        assert_eq!(archive::active_len(&env, &developer), 0);
        assert_eq!(archive::archived_len(&env, &developer), 2);
    });
}

#[test]
fn archive_events_moves_the_record_payload_unchanged() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, addr, client) = setup(&env);
    let developer = Address::generate(&env);

    credit(&client, &vault, &token, &developer, 250, 7);
    client.archive_events(&developer, &1u32);

    env.as_contract(&addr, || {
        let archived: EventRecord = env
            .storage()
            .temporary()
            .get(&DataKey::ArchivedEvent(developer.clone(), 0))
            .expect("archived event record");
        assert_eq!(
            archived,
            EventRecord {
                amount: 250,
                token: token.clone(),
                ledger_seq: 7,
            }
        );
        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::ActiveEvent(developer.clone(), 0)),
            "the active entry must be deleted once it is archived"
        );
    });
}

#[test]
fn archive_events_respects_batch_size() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, addr, client) = setup(&env);
    let developer = Address::generate(&env);

    for seq in 1..6u32 {
        credit(&client, &vault, &token, &developer, 100, seq);
    }

    let archived = client.archive_events(&developer, &2u32);
    assert_eq!(archived, 2);

    env.as_contract(&addr, || {
        assert_eq!(archive::active_len(&env, &developer), 3);
        assert_eq!(archive::archived_len(&env, &developer), 2);
        // FIFO: the two oldest entries moved, the newest three stay active.
        let cursor: Cursor = env
            .storage()
            .persistent()
            .get(&DataKey::Cursor(developer.clone()))
            .expect("cursor");
        assert_eq!(cursor.tail, 2);
        assert_eq!(cursor.head, 5);
    });
}

#[test]
fn archive_events_batch_size_capped() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, _addr, client) = setup(&env);
    let developer = Address::generate(&env);

    credit(&client, &vault, &token, &developer, 100, 1);

    // A caller-supplied batch above the cap must still stop at the cap.
    let archived = client.archive_events(&developer, &(MAX_BATCH_SIZE + 1));
    assert_eq!(archived, 1);
}

#[test]
fn archive_events_empty_returns_zero() {
    let env = Env::default();
    env.mock_all_auths();
    let (_vault, _token, _addr, client) = setup(&env);
    let developer = Address::generate(&env);

    assert_eq!(client.archive_events(&developer, &MAX_BATCH_SIZE), 0);
}

#[test]
fn archive_events_second_call_continues_the_cursor() {
    let env = Env::default();
    env.mock_all_auths();
    let (vault, token, addr, client) = setup(&env);
    let developer = Address::generate(&env);

    for seq in 1..4u32 {
        credit(&client, &vault, &token, &developer, 100, seq);
    }

    assert_eq!(client.archive_events(&developer, &2u32), 2);
    assert_eq!(client.archive_events(&developer, &2u32), 1);
    assert_eq!(client.archive_events(&developer, &2u32), 0);

    env.as_contract(&addr, || {
        assert_eq!(archive::active_len(&env, &developer), 0);
        assert_eq!(archive::archived_len(&env, &developer), 3);
    });
}

#[test]
fn archive_events_requires_developer_auth() {
    let env = Env::default();
    env.mock_all_auths();
    let (_vault, _token, _addr, client) = setup(&env);
    let developer = Address::generate(&env);

    env.set_auths(&[]);
    assert!(
        client.try_archive_events(&developer, &MAX_BATCH_SIZE).is_err(),
        "archive_events must require the developer's authorization"
    );
}
