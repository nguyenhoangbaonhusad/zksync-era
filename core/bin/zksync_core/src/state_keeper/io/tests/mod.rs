use futures::FutureExt;

use std::time::Duration;

use db_test_macro::db_test;
use vm::vm_with_bootloader::{derive_base_fee_and_gas_per_pubdata, BlockContextMode};
use zksync_contracts::BaseSystemContractsHashes;
use zksync_dal::ConnectionPool;
use zksync_mempool::L2TxFilter;
use zksync_types::{
    block::BlockGasCount, tx::ExecutionMetrics, AccountTreeId, Address, L1BatchNumber,
    MiniblockNumber, StorageKey, VmEvent, H256, U256,
};
use zksync_utils::time::millis_since_epoch;

use crate::state_keeper::{
    io::{MiniblockSealer, StateKeeperIO},
    mempool_actor::l2_tx_filter,
    tests::{
        create_block_metadata, create_execution_result, create_transaction, create_updates_manager,
        default_block_context, default_vm_block_result, Query,
    },
    updates::{MiniblockSealCommand, MiniblockUpdates, UpdatesManager},
};

mod tester;

use self::tester::Tester;

/// Ensure that MempoolIO.filter is correctly initialized right after mempool initialization.
#[db_test]
async fn test_filter_initialization(connection_pool: ConnectionPool) {
    let tester = Tester::new();

    // Genesis is needed for proper mempool initialization.
    tester.genesis(&connection_pool).await;
    let (mempool, _) = tester.create_test_mempool_io(connection_pool, 1).await;

    // Upon initialization, the filter should be set to the default values.
    assert_eq!(mempool.filter(), &L2TxFilter::default());
}

/// Ensure that MempoolIO.filter is modified correctly if there is a pending batch upon mempool initialization.
#[db_test]
async fn test_filter_with_pending_batch(connection_pool: ConnectionPool) {
    let tester = Tester::new();

    tester.genesis(&connection_pool).await;

    // Insert a sealed batch so there will be a prev_l1_batch_state_root.
    // These gas values are random and don't matter for filter calculation as there will be a
    // pending batch the filter will be based off of.
    tester
        .insert_miniblock(&connection_pool, 1, 5, 55, 555)
        .await;
    tester.insert_sealed_batch(&connection_pool, 1).await;

    // Inserting a pending miniblock that isn't included in a sealed batch means there is a pending batch.
    // The gas values are randomly chosen but so affect filter values calculation.
    let (give_l1_gas_price, give_fair_l2_gas_price) = (100, 1000);
    tester
        .insert_miniblock(
            &connection_pool,
            2,
            10,
            give_l1_gas_price,
            give_fair_l2_gas_price,
        )
        .await;

    let (mut mempool, _) = tester.create_test_mempool_io(connection_pool, 1).await;
    // Before the mempool knows there is a pending batch, the filter is still set to the default values.
    assert_eq!(mempool.filter(), &L2TxFilter::default());

    mempool.load_pending_batch().await;
    let (want_base_fee, want_gas_per_pubdata) =
        derive_base_fee_and_gas_per_pubdata(give_l1_gas_price, give_fair_l2_gas_price);
    let want_filter = L2TxFilter {
        l1_gas_price: give_l1_gas_price,
        fee_per_gas: want_base_fee,
        gas_per_pubdata: want_gas_per_pubdata as u32,
    };
    assert_eq!(mempool.filter(), &want_filter);
}

/// Ensure that MempoolIO.filter is modified correctly if there is no pending batch.
#[db_test]
async fn test_filter_with_no_pending_batch(connection_pool: ConnectionPool) {
    let tester = Tester::new();
    tester.genesis(&connection_pool).await;

    // Insert a sealed batch so there will be a prev_l1_batch_state_root.
    // These gas values are random and don't matter for filter calculation.
    tester
        .insert_miniblock(&connection_pool, 1, 5, 55, 555)
        .await;
    tester.insert_sealed_batch(&connection_pool, 1).await;

    // Create a copy of the tx filter that the mempool will use.
    let want_filter = l2_tx_filter(
        &tester.create_gas_adjuster().await,
        tester.fair_l2_gas_price(),
    );

    // Create a mempool without pending batch and ensure that filter is not initialized just yet.
    let (mut mempool, mut guard) = tester.create_test_mempool_io(connection_pool, 1).await;
    assert_eq!(mempool.filter(), &L2TxFilter::default());

    // Insert a transaction that matches the expected filter.
    tester.insert_tx(
        &mut guard,
        want_filter.fee_per_gas,
        want_filter.gas_per_pubdata,
    );

    // Now, given that there is a transaction matching the expected filter, waiting for the new batch params
    // should succeed and initialize the filter.
    mempool
        .wait_for_new_batch_params(Duration::from_secs(10))
        .await
        .expect("No batch params in the test mempool");
    assert_eq!(mempool.filter(), &want_filter);
}

async fn test_l1_batch_timestamps_are_distinct(
    connection_pool: ConnectionPool,
    prev_l1_batch_timestamp: u64,
) {
    let mut tester = Tester::new();
    tester.genesis(&connection_pool).await;

    tester.set_timestamp(prev_l1_batch_timestamp);
    tester
        .insert_miniblock(&connection_pool, 1, 5, 55, 555)
        .await;
    tester.insert_sealed_batch(&connection_pool, 1).await;

    let (mut mempool, mut guard) = tester.create_test_mempool_io(connection_pool, 1).await;
    // Insert a transaction to trigger L1 batch creation.
    let tx_filter = l2_tx_filter(
        &tester.create_gas_adjuster().await,
        tester.fair_l2_gas_price(),
    );
    tester.insert_tx(&mut guard, tx_filter.fee_per_gas, tx_filter.gas_per_pubdata);

    let batch_params = mempool
        .wait_for_new_batch_params(Duration::from_secs(10))
        .await
        .expect("No batch params in the test mempool");
    assert!(batch_params.context_mode.timestamp() > prev_l1_batch_timestamp);
}

#[db_test]
async fn l1_batch_timestamp_basics(connection_pool: ConnectionPool) {
    let current_timestamp = (millis_since_epoch() / 1_000) as u64;
    test_l1_batch_timestamps_are_distinct(connection_pool, current_timestamp).await;
}

#[db_test]
async fn l1_batch_timestamp_with_clock_skew(connection_pool: ConnectionPool) {
    let current_timestamp = (millis_since_epoch() / 1_000) as u64;
    test_l1_batch_timestamps_are_distinct(connection_pool, current_timestamp + 2).await;
}

#[db_test]
async fn processing_storage_logs_when_sealing_miniblock(connection_pool: ConnectionPool) {
    let mut miniblock = MiniblockUpdates::new(0);

    let tx = create_transaction(10, 100);
    let storage_logs = [
        (U256::from(1), Query::Read(U256::from(0))),
        (U256::from(2), Query::InitialWrite(U256::from(1))),
        (
            U256::from(3),
            Query::RepeatedWrite(U256::from(2), U256::from(3)),
        ),
        (
            U256::from(2),
            Query::RepeatedWrite(U256::from(1), U256::from(4)),
        ),
    ];
    let execution_result = create_execution_result(0, storage_logs);
    miniblock.extend_from_executed_transaction(
        tx,
        execution_result,
        BlockGasCount::default(),
        ExecutionMetrics::default(),
        vec![],
    );

    let tx = create_transaction(10, 100);
    let storage_logs = [
        (U256::from(4), Query::InitialWrite(U256::from(5))),
        (
            U256::from(3),
            Query::RepeatedWrite(U256::from(3), U256::from(6)),
        ),
    ];
    let execution_result = create_execution_result(1, storage_logs);
    miniblock.extend_from_executed_transaction(
        tx,
        execution_result,
        BlockGasCount::default(),
        ExecutionMetrics::default(),
        vec![],
    );

    let l1_batch_number = L1BatchNumber(2);
    let seal_command = MiniblockSealCommand {
        l1_batch_number,
        miniblock_number: MiniblockNumber(3),
        miniblock,
        first_tx_index: 0,
        l1_gas_price: 100,
        fair_l2_gas_price: 100,
        base_fee_per_gas: 10,
        base_system_contracts_hashes: BaseSystemContractsHashes::default(),
        l2_erc20_bridge_addr: Address::default(),
    };
    let mut conn = connection_pool.access_storage_tagged("state_keeper").await;
    seal_command.seal(&mut conn).await;

    // Manually mark the miniblock as executed so that getting touched slots from it works
    conn.blocks_dal()
        .mark_miniblocks_as_executed_in_l1_batch(l1_batch_number)
        .await;
    let touched_slots = conn
        .storage_logs_dal()
        .get_touched_slots_for_l1_batch(l1_batch_number)
        .await;

    // Keys that are only read must not be written to `storage_logs`.
    let account = AccountTreeId::default();
    let read_key = StorageKey::new(account, H256::from_low_u64_be(1));
    assert!(!touched_slots.contains_key(&read_key));

    // The storage logs must be inserted and read in the correct order, so that
    // `touched_slots` contain the most recent values in the L1 batch.
    assert_eq!(touched_slots.len(), 3);
    let written_kvs = [(2, 4), (3, 6), (4, 5)];
    for (key, value) in written_kvs {
        let key = StorageKey::new(account, H256::from_low_u64_be(key));
        let expected_value = H256::from_low_u64_be(value);
        assert_eq!(touched_slots[&key], expected_value);
    }
}

#[db_test]
async fn processing_events_when_sealing_miniblock(pool: ConnectionPool) {
    let l1_batch_number = L1BatchNumber(2);
    let mut miniblock = MiniblockUpdates::new(0);

    let events = (0_u8..10).map(|i| VmEvent {
        location: (l1_batch_number, u32::from(i / 4)),
        value: vec![i],
        ..VmEvent::default()
    });
    let events: Vec<_> = events.collect();

    for (i, events_chunk) in events.chunks(4).enumerate() {
        let tx = create_transaction(10, 100);
        let mut execution_result = create_execution_result(i as u16, []);
        execution_result.result.logs.events = events_chunk.to_vec();
        miniblock.extend_from_executed_transaction(
            tx,
            execution_result,
            BlockGasCount::default(),
            ExecutionMetrics::default(),
            vec![],
        );
    }

    let miniblock_number = MiniblockNumber(3);
    let seal_command = MiniblockSealCommand {
        l1_batch_number,
        miniblock_number,
        miniblock,
        first_tx_index: 0,
        l1_gas_price: 100,
        fair_l2_gas_price: 100,
        base_fee_per_gas: 10,
        base_system_contracts_hashes: BaseSystemContractsHashes::default(),
        l2_erc20_bridge_addr: Address::default(),
    };
    let mut conn = pool.access_storage_tagged("state_keeper").await;
    seal_command.seal(&mut conn).await;

    let logs = conn
        .events_web3_dal()
        .get_all_logs(miniblock_number - 1)
        .await
        .unwrap();

    assert_eq!(logs.len(), 10);
    // The event logs should be inserted in the correct order.
    for (i, log) in logs.iter().enumerate() {
        assert_eq!(log.data.0, [i as u8]);
    }
}

async fn test_miniblock_and_l1_batch_processing(
    pool: ConnectionPool,
    miniblock_sealer_capacity: usize,
) {
    let tester = Tester::new();

    // Genesis is needed for proper mempool initialization.
    tester.genesis(&pool).await;
    let mut conn = pool.access_storage_tagged("state_keeper").await;
    // Save metadata for the genesis L1 batch so that we don't hang in `seal_l1_batch`.
    let block_metadata = create_block_metadata(0);
    conn.blocks_dal()
        .save_blocks_metadata(L1BatchNumber(0), &block_metadata, H256::zero())
        .await;
    drop(conn);

    let (mut mempool, _) = tester
        .create_test_mempool_io(pool.clone(), miniblock_sealer_capacity)
        .await;

    let mut block_context = default_block_context();
    block_context.context.block_timestamp = 100; // change timestamp to pass monotonicity check
    let block_context_mode = BlockContextMode::NewBlock(block_context, 0.into());
    let mut updates =
        UpdatesManager::new(&block_context_mode, BaseSystemContractsHashes::default());

    let tx = create_transaction(10, 100);
    updates.extend_from_executed_transaction(
        tx,
        create_execution_result(0, []),
        vec![],
        BlockGasCount::default(),
        ExecutionMetrics::default(),
    );
    mempool.seal_miniblock(&updates).await;
    updates.push_miniblock(1);

    let block_result = default_vm_block_result();
    mempool
        .seal_l1_batch(block_result, updates, block_context)
        .await;

    // Check that miniblock #1 and L1 batch #1 are persisted.
    let mut conn = pool.access_storage_tagged("state_keeper").await;
    assert_eq!(
        conn.blocks_dal().get_sealed_miniblock_number().await,
        MiniblockNumber(2) // + fictive miniblock
    );
    let l1_batch_header = conn
        .blocks_dal()
        .get_block_header(L1BatchNumber(1))
        .await
        .unwrap();
    assert_eq!(l1_batch_header.l2_tx_count, 1);
    assert!(l1_batch_header.is_finished);
}

#[db_test]
async fn miniblock_and_l1_batch_processing(pool: ConnectionPool) {
    test_miniblock_and_l1_batch_processing(pool, 1).await;
}

#[db_test]
async fn miniblock_and_l1_batch_processing_with_sync_sealer(pool: ConnectionPool) {
    test_miniblock_and_l1_batch_processing(pool, 0).await;
}

#[db_test]
async fn miniblock_sealer_handle_blocking(pool: ConnectionPool) {
    let (mut sealer, mut sealer_handle) = MiniblockSealer::new(pool, 1);

    // The first command should be successfully submitted immediately.
    let updates_manager = create_updates_manager();
    let seal_command = updates_manager.seal_miniblock_command(
        L1BatchNumber(1),
        MiniblockNumber(1),
        Address::default(),
    );
    sealer_handle.submit(seal_command).await;

    // The second command should lead to blocking
    let seal_command = updates_manager.seal_miniblock_command(
        L1BatchNumber(1),
        MiniblockNumber(2),
        Address::default(),
    );
    {
        let submit_future = sealer_handle.submit(seal_command);
        futures::pin_mut!(submit_future);

        assert!((&mut submit_future).now_or_never().is_none());
        // ...until miniblock #1 is processed
        let command = sealer.commands_receiver.recv().await.unwrap();
        command.completion_sender.send(()).unwrap_err(); // completion receiver should be dropped
        submit_future.await;
    }

    {
        let wait_future = sealer_handle.wait_for_all_commands();
        futures::pin_mut!(wait_future);
        assert!((&mut wait_future).now_or_never().is_none());
        let command = sealer.commands_receiver.recv().await.unwrap();
        command.completion_sender.send(()).unwrap();
        wait_future.await;
    }

    // Check that `wait_for_all_commands()` state is reset after use.
    sealer_handle.wait_for_all_commands().await;

    let seal_command = updates_manager.seal_miniblock_command(
        L1BatchNumber(2),
        MiniblockNumber(3),
        Address::default(),
    );
    sealer_handle.submit(seal_command).await;
    let command = sealer.commands_receiver.recv().await.unwrap();
    command.completion_sender.send(()).unwrap();
    sealer_handle.wait_for_all_commands().await;
}

#[db_test]
async fn miniblock_sealer_handle_parallel_processing(pool: ConnectionPool) {
    let (mut sealer, mut sealer_handle) = MiniblockSealer::new(pool, 5);

    // 5 miniblock sealing commands can be submitted without blocking.
    for i in 1..=5 {
        let updates_manager = create_updates_manager();
        let seal_command = updates_manager.seal_miniblock_command(
            L1BatchNumber(1),
            MiniblockNumber(i),
            Address::default(),
        );
        sealer_handle.submit(seal_command).await;
    }

    for i in 1..=5 {
        let command = sealer.commands_receiver.recv().await.unwrap();
        assert_eq!(command.command.miniblock_number, MiniblockNumber(i));
        command.completion_sender.send(()).ok();
    }

    sealer_handle.wait_for_all_commands().await;
}
