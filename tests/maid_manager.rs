// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

// For explanation of lint checks, run `rustc -W help` or see
// https://github.com/maidsafe/QA/blob/master/Documentation/Rust%20Lint%20Checks.md

use fake_clock::FakeClock;
use rand::Rng;
use routing::{AccountInfo, Action, BootstrapConfig, ClientError, Event, FullId,
              MAX_IMMUTABLE_DATA_SIZE_IN_BYTES, MAX_MUTABLE_DATA_ENTRIES,
              MAX_MUTABLE_DATA_SIZE_IN_BYTES, MessageId, MutableData, PermissionSet, Response,
              TYPE_TAG_SESSION_PACKET, User, Value, XorName};
use routing::mock_crust::Network;
use rust_sodium::crypto::sign;
use safe_vault::{Config, DEFAULT_MAX_OPS_COUNT, TYPE_TAG_INVITE, test_utils};
use safe_vault::mock_crust_detail::{self, Data, poll, test_node};
use safe_vault::mock_crust_detail::test_client::TestClient;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tiny_keccak;

const TEST_NET_SIZE: usize = 20;
const TEST_TAG: u64 = 123456;

#[test]
fn handle_put_without_account() {
    let group_size = 8;
    let network = Network::new(group_size, None);
    let mut rng = network.new_rng();

    let node_count = TEST_NET_SIZE;
    let mut nodes = test_node::create_nodes(&network, node_count, None, true);

    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    client.ensure_connected(&mut nodes);

    let data = test_utils::gen_immutable_data(1024, &mut rng);
    let _ = client.put_idata(data);
    let event_count = poll::nodes_and_client(&mut nodes, &mut client);
    trace!("Processed {} events.", event_count);

    let count = nodes
        .iter()
        .filter(|node| {
            node.get_maid_manager_mutation_count(client.name())
                .is_some()
        })
        .count();
    assert_eq!(
        count,
        0,
        "mutations count {} found with {} nodes",
        count,
        node_count
    );
}

#[test]
fn handle_put_with_account() {
    let group_size = 8;
    let network = Network::new(group_size, None);
    let mut rng = network.new_rng();

    let node_count = TEST_NET_SIZE;
    let mut nodes = test_node::create_nodes(&network, node_count, None, true);
    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));

    client.ensure_connected(&mut nodes);

    let result = client.get_account_info_response(&mut nodes);
    assert_eq!(result, Err(ClientError::NoSuchAccount));

    client.create_account(&mut nodes);
    let mut expected_mutations_done = 1;
    let mut expected_mutations_available = DEFAULT_MAX_OPS_COUNT - expected_mutations_done;
    let account_info = unwrap!(client.get_account_info_response(&mut nodes));
    assert_eq!(account_info.mutations_done, expected_mutations_done);
    assert_eq!(
        account_info.mutations_available,
        expected_mutations_available
    );

    let data = test_utils::gen_immutable_data(1024, &mut rng);
    let _ = client.put_idata(data.clone());
    let event_count = poll::nodes_and_client(&mut nodes, &mut client);
    trace!("Processed {} events.", event_count);

    let count = nodes
        .iter()
        .filter(|node| {
            node.get_maid_manager_mutation_count(client.name())
                .is_some()
        })
        .count();
    assert_eq!(
        count,
        group_size,
        "client account count {} found on {} nodes",
        count,
        node_count
    );

    mock_crust_detail::check_data(vec![Data::Immutable(data)], &nodes, group_size);

    expected_mutations_done += 1;
    expected_mutations_available = DEFAULT_MAX_OPS_COUNT - expected_mutations_done;
    let account_info = unwrap!(client.get_account_info_response(&mut nodes));
    assert_eq!(account_info.mutations_done, expected_mutations_done);
    assert_eq!(
        account_info.mutations_available,
        expected_mutations_available
    );
}

#[test]
fn put_oversized_data() {
    let group_size = 8;
    let network = Network::new(group_size, None);
    let mut rng = network.new_rng();

    let mut nodes = test_node::create_nodes(&network, TEST_NET_SIZE, None, true);
    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));

    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    // Too large immutable data
    let data =
        test_utils::gen_immutable_data(MAX_IMMUTABLE_DATA_SIZE_IN_BYTES as usize + 1, &mut rng);
    match client.put_large_sized_idata(data, &mut nodes) {
        Err(ClientError::DataTooLarge) => (),
        x => panic!("Unexpected response: {:?}", x),
    }

    // Mutable data with too large entries
    let mut data = test_utils::gen_mutable_data(
        TEST_TAG,
        0,
        *client.full_id().public_id().signing_public_key(),
        &mut rng,
    );
    let key0 = b"key0".to_vec();
    let value0 = Value {
        content: test_utils::gen_vec(MAX_MUTABLE_DATA_SIZE_IN_BYTES as usize / 2, &mut rng),
        entry_version: 0,
    };

    let key1 = b"key1".to_vec();
    let value1 = Value {
        content: test_utils::gen_vec(MAX_MUTABLE_DATA_SIZE_IN_BYTES as usize / 2 + 2, &mut rng),
        entry_version: 0,
    };

    assert!(data.mutate_entry_without_validation(key0, value0));
    assert!(data.mutate_entry_without_validation(key1, value1));

    match client.put_mdata_response(data, &mut nodes) {
        Err(ClientError::DataTooLarge) => (),
        x => panic!("Unexpected response: {:?}", x),
    }

    // Mutable data with too many entries
    let mut data = test_utils::gen_mutable_data(
        TEST_TAG,
        0,
        *client.full_id().public_id().signing_public_key(),
        &mut rng,
    );
    for i in 0..MAX_MUTABLE_DATA_ENTRIES + 1 {
        let key = format!("key{}", i).into_bytes();
        let value = Value {
            content: test_utils::gen_vec(10, &mut rng),
            entry_version: 0,
        };

        assert!(data.mutate_entry_without_validation(key, value));
    }

    match client.put_mdata_response(data, &mut nodes) {
        Err(ClientError::TooManyEntries) => (),
        x => panic!("Unexpected response: {:?}", x),
    }

    // Larger than rate limiter per client capacity
    FakeClock::advance_time(2000);
    let data =
        test_utils::gen_immutable_data(MAX_IMMUTABLE_DATA_SIZE_IN_BYTES as usize + 10240, &mut rng);
    match client.put_large_sized_idata(data, &mut nodes) {
        Err(ClientError::InvalidOperation) => (),
        x => panic!("Unexpected response: {:?}", x),
    }
}


#[test]
fn create_account_twice() {
    let group_size = 8;
    let network = Network::new(group_size, None);
    let mut rng = network.new_rng();

    let node_count = TEST_NET_SIZE;
    let mut nodes = test_node::create_nodes(&network, node_count, None, true);
    let config =
        BootstrapConfig::with_contacts(&[unwrap!(rng.choose(&nodes), "no nodes found").endpoint()]);
    let mut client0 = TestClient::new(&network, Some(config.clone()));
    let mut client1 = TestClient::new(&network, Some(config.clone()));

    client0.ensure_connected(&mut nodes);
    client1.ensure_connected(&mut nodes);

    assert_eq!(
        Err(ClientError::NoSuchAccount),
        client0.get_account_info_response(&mut nodes)
    );
    assert_eq!(
        Err(ClientError::NoSuchAccount),
        client1.get_account_info_response(&mut nodes)
    );

    let mut owners = BTreeSet::new();
    let _ = owners.insert(*client0.signing_public_key());

    let account = unwrap!(MutableData::new(
        rng.gen(),
        TYPE_TAG_SESSION_PACKET,
        Default::default(),
        Default::default(),
        owners.clone(),
    ));

    let expected_account_info = AccountInfo {
        mutations_done: 1,
        mutations_available: DEFAULT_MAX_OPS_COUNT - 1,
    };

    // Create the account using `client0`.
    unwrap!(client0.put_mdata_response(account.clone(), &mut nodes));

    assert_eq!(
        unwrap!(client0.get_account_info_response(&mut nodes)),
        expected_account_info
    );
    assert_eq!(
        client1.get_account_info_response(&mut nodes),
        Err(ClientError::NoSuchAccount)
    );

    // Create the account again using `client0`.
    assert_eq!(
        client0.put_mdata_response(account.clone(), &mut nodes),
        Err(ClientError::AccountExists)
    );
    let _ = poll::nodes_and_client(&mut nodes, &mut client0);

    // That should not have changed anything.
    assert_eq!(
        unwrap!(client0.get_account_info_response(&mut nodes)),
        expected_account_info
    );
    assert_eq!(
        client1.get_account_info_response(&mut nodes),
        Err(ClientError::NoSuchAccount)
    );

    // Create the same account using `client1`.
    assert_eq!(
        client1.put_mdata_response(account, &mut nodes),
        Err(ClientError::InvalidOwners)
    );
    let _ = poll::nodes_and_client(&mut nodes, &mut client1);

    // That should not succeed.
    assert_eq!(
        unwrap!(client0.get_account_info_response(&mut nodes)),
        expected_account_info
    );
    assert_eq!(
        client1.get_account_info_response(&mut nodes),
        Err(ClientError::NoSuchAccount)
    );

    // Create the account again, but with different name, using `client0`.
    let account = unwrap!(MutableData::new(
        rng.gen(),
        TYPE_TAG_SESSION_PACKET,
        Default::default(),
        Default::default(),
        owners,
    ));
    assert_eq!(
        client0.put_mdata_response(account, &mut nodes),
        Err(ClientError::AccountExists)
    );
}

// Test the invite workflow:
// 1. Put a new invite on the network by an admin client
// 2. Verify only admin clients can put invites
// 3. Create account using invite code
// 4. Verify invite code can be used only once
#[test]
fn invite() {
    let seed = None;

    let group_size = 8;
    let node_count = group_size;
    let network = Network::new(group_size, seed);
    let admin_id = FullId::new();
    let vault_config = Config {
        invite_key: Some(admin_id.public_id().signing_public_key().0),
        ..Default::default()
    };

    let mut nodes = test_node::create_nodes(&network, node_count, Some(vault_config), false);
    let mut rng = network.new_rng();
    let config =
        BootstrapConfig::with_contacts(&[unwrap!(rng.choose(&nodes), "no nodes found").endpoint()]);

    let mut admin_client = TestClient::with_id(&network, Some(config.clone()), admin_id);
    admin_client.ensure_connected(&mut nodes);
    admin_client.create_account(&mut nodes);

    let invite_code = "invite";

    // Put the invite
    let name = XorName(tiny_keccak::sha3_256(invite_code.as_bytes()));
    let mut owners = BTreeSet::new();
    let _ = owners.insert(*admin_client.signing_public_key());
    let mut permissions = BTreeMap::new();
    let _ = permissions.insert(User::Anyone, PermissionSet::new().allow(Action::Insert));
    let data = unwrap!(MutableData::new(
        name,
        TYPE_TAG_INVITE,
        permissions.clone(),
        Default::default(),
        owners,
    ));
    unwrap!(admin_client.put_mdata_response(data, &mut nodes));

    let mut client1 = TestClient::new(&network, Some(config.clone()));
    client1.ensure_connected(&mut nodes);

    // Attempt to create account using invalid invite code fails.
    assert_eq!(
        Err(ClientError::InvalidInvitation),
        client1.create_account_with_invitation_response("invalid invite", &mut nodes)
    );

    // Create account using valid invite code.
    unwrap!(client1.create_account_with_invitation_response(
        invite_code,
        &mut nodes,
    ));

    // Attempt to put an invite by non-admin client fails.
    let name = XorName(tiny_keccak::sha3_256(b"fake invite"));
    let mut owners = BTreeSet::new();
    let _ = owners.insert(*client1.signing_public_key());
    let data = unwrap!(MutableData::new(
        name,
        TYPE_TAG_INVITE,
        permissions,
        Default::default(),
        owners,
    ));
    assert_eq!(
        Err(ClientError::InvalidOperation),
        client1.put_mdata_response(data, &mut nodes)
    );

    // Attempt to reuse already claimed invite fails.
    let mut client2 = TestClient::new(&network, Some(config));
    client2.ensure_connected(&mut nodes);

    assert_eq!(
        Err(ClientError::InvitationAlreadyClaimed),
        client2.create_account_with_invitation_response(invite_code, &mut nodes)
    );
}

#[test]
fn storing_till_client_account_full() {
    let group_size = 8;
    let network = Network::new(group_size, None);
    let mut rng = network.new_rng();

    let node_count = 15;
    let mut nodes = test_node::create_nodes(&network, node_count, None, true);
    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    let client_key = *client.full_id().public_id().signing_public_key();

    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    for i in 0..DEFAULT_MAX_OPS_COUNT + 5 {
        let result = if i % 2 == 0 {
            let data = test_utils::gen_mutable_data(TEST_TAG, 0, client_key, &mut rng);
            client.put_mdata_response(data, &mut nodes)
        } else {
            let data = test_utils::gen_immutable_data(10, &mut rng);
            client.put_idata_response(data, &mut nodes)
        };

        if i < DEFAULT_MAX_OPS_COUNT - 1 {
            assert_eq!(result, Ok(()));
        } else {
            assert_eq!(result, Err(ClientError::LowBalance));
        }
    }
}

#[test]
fn account_balance_with_successful_mutations_with_churn() {
    let seed = None;
    let iterations = test_utils::iterations();
    let node_count = 15;
    let data_count = 4;

    let group_size = 8;
    let network = Network::new(group_size, seed);
    let mut rng = network.new_rng();

    let mut nodes = test_node::create_nodes(&network, node_count, None, false);
    let config = BootstrapConfig::with_contacts(&[nodes[1].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    let client_key = *client.full_id().public_id().signing_public_key();

    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    let mut mutation_count = 1; // Session packet.
    let mut event_count = 0;

    for i in 0..iterations {
        trace!("Churning on {} nodes, iteration {}", nodes.len(), i);

        trace!("Putting {} chunks", data_count);
        for _ in 0..data_count {
            let data = test_utils::gen_mutable_data(TEST_TAG, 10, client_key, &mut rng);
            let _ = client.put_mdata(data);
            mutation_count += 1;
        }

        if nodes.len() <= group_size + 2 || rng.gen() {
            let index = rng.gen_range(2, nodes.len());
            trace!("Adding node with bootstrap node {}.", index);
            test_node::add_node(&network, &mut nodes, index, false);
        } else {
            let number = rng.gen_range(1, 4);
            trace!("Removing {} node(s).", number);
            for _ in 0..number {
                let node_index = rng.gen_range(2, nodes.len());
                test_node::drop_node(&mut nodes, node_index);
            }
        }

        event_count += poll::nodes_and_client_with_resend(&mut nodes, &mut client);
        trace!("Processed {} events.", event_count);

        let sorted_nodes = test_node::closest_to(&nodes, client.name(), group_size);
        let node_count_stats: Vec<_> = sorted_nodes
            .into_iter()
            .map(|node| {
                (
                    node.name(),
                    unwrap!(node.get_maid_manager_mutation_count(client.name())),
                )
            })
            .collect();

        for &(_, count) in &node_count_stats {
            assert_eq!(
                count,
                mutation_count,
                "Expected {} mutations, got: {:?}",
                mutation_count,
                node_count_stats
            );
        }

        mock_crust_detail::verify_network_invariant_for_all_nodes(&nodes);
    }
}

#[test]
fn account_balance_with_failed_mutations_with_churn() {
    let seed = None;
    let node_count = 15;
    let chunks_per_iter = 4;

    let group_size = 8;
    let network = Network::new(group_size, seed);
    let mut rng = network.new_rng();

    let mut nodes = test_node::create_nodes(&network, node_count, None, false);
    let client_config = BootstrapConfig::with_contacts(&[nodes[1].endpoint()]);
    let mut client = TestClient::new(&network, Some(client_config));
    let client_key = *client.full_id().public_id().signing_public_key();

    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    let mut event_count = 0;
    let mut data_list = Vec::new();

    for i in 0..test_utils::iterations() as u64 {
        trace!("Churning on {} nodes, iteration {}", nodes.len(), i);

        if i % 2 == 0 {
            trace!("Putting {} chunks (expecting success)", chunks_per_iter);
            data_list.clear();
            for _ in 0..chunks_per_iter {
                let data = test_utils::gen_mutable_data(TEST_TAG, 10, client_key, &mut rng);
                let _ = client.put_mdata(data.clone());
                data_list.push(data);
            }
        } else {
            trace!("Putting {} chunks (expecting failure)", data_list.len());
            for data in &data_list {
                // Expect to be failed in DM. Account balance is not increased.
                let _ = client.put_mdata(data.clone());
            }
        }

        if nodes.len() <= group_size + 2 || rng.gen() {
            let index = rng.gen_range(2, nodes.len());
            trace!("Adding node with bootstrap node {}.", index);
            test_node::add_node(&network, &mut nodes, index, false);
        } else {
            let number = rng.gen_range(1, 4);
            trace!("Removing {} node(s).", number);
            for _ in 0..number {
                let node_index = rng.gen_range(2, nodes.len());
                test_node::drop_node(&mut nodes, node_index);
            }
        }

        event_count += poll::nodes_and_client_with_resend(&mut nodes, &mut client);
        trace!("Processed {} events.", event_count);

        let sorted_nodes = test_node::closest_to(&nodes, client.name(), group_size);
        let node_count_stats: Vec<_> = sorted_nodes
            .into_iter()
            .map(|node| {
                (
                    node.name(),
                    unwrap!(node.get_maid_manager_mutation_count(client.name())),
                )
            })
            .collect();

        let expected_mutation_count = chunks_per_iter * (i / 2 + 1) + 1;
        for &(_, count) in &node_count_stats {
            assert_eq!(
                count,
                expected_mutation_count,
                "Unexpected mutation count: {:?}",
                node_count_stats
            );

        }
    }
}

// Multiple clients try to concurrently mutate (insert and delete) the auth keys.
// At most one mutation should succeed. Verify the keys are in consistent state
// by senfing `ListAuthKeysAndVersion` request and asserting the response reflect
// the mutations (if any).
#[test]
fn account_concurrent_keys_mutation() {
    let seed = None;
    let node_count = TEST_NET_SIZE;
    let iterations = test_utils::iterations();
    let max_mutations = 4;

    let group_size = 8;
    let network = Network::new(group_size, seed);
    let mut rng = network.new_rng();

    let mut nodes = test_node::create_nodes(&network, node_count, None, false);

    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    let mut event_count = 0;
    let mut stored_keys = Vec::new();
    let mut inserted_keys = HashMap::new();
    let mut deleted_keys = HashMap::new();
    let mut version = 0;

    for i in 0..iterations {
        trace!("Iteration {}.", i + 1);
        let mutations = rng.gen_range(2, max_mutations);
        for _ in 0..mutations {
            if stored_keys.is_empty() || rng.gen() {
                // Insert key
                let (key, _) = sign::gen_keypair();
                let msg_id = client.ins_auth_key(key, version + 1);
                let _ = inserted_keys.insert(msg_id, key);
            } else {
                // Delete key
                let key = *unwrap!(rng.choose(&stored_keys));
                let msg_id = client.del_auth_key(key, version + 1);
                let _ = deleted_keys.insert(msg_id, key);
            }
        }

        event_count += poll::nodes_and_client(&mut nodes, &mut client);
        trace!("Processed {} events.", event_count);

        let mut ins_successes = Vec::new();
        let mut del_successes = Vec::new();

        while let Ok(event) = client.try_recv() {
            match event {
                Event::Response {
                    response: Response::InsAuthKey { res: Ok(()), msg_id, }, ..
                } => ins_successes.push(msg_id),
                Event::Response {
                    response: Response::DelAuthKey { res: Ok(()), msg_id, }, ..
                } => del_successes.push(msg_id),
                _ => (),
            }
        }

        let successes = ins_successes.len() + del_successes.len();
        assert!(successes <= 1, "At most one mutation may succeed");

        let (new_keys, new_version) =
            unwrap!(client.list_auth_keys_and_version_response(&mut nodes));

        if !ins_successes.is_empty() {
            assert_eq!(new_keys.len(), stored_keys.len() + 1);

            for msg_id in ins_successes {
                let key = unwrap!(inserted_keys.remove(&msg_id));
                assert!(new_keys.contains(&key));
            }
        }

        if !del_successes.is_empty() {
            assert_eq!(new_keys.len(), stored_keys.len() - 1);

            for msg_id in del_successes {
                let key = unwrap!(deleted_keys.remove(&msg_id));
                assert!(!new_keys.contains(&key));
            }
        }

        assert_eq!(new_version, version + successes as u64);

        stored_keys = new_keys.into_iter().collect();
        version = new_version;
    }
}

// Client trying to concurrently insert a key and put a data.
// The result could be:
//  1, both succeed.
#[test]
fn account_concurrent_insert_key_put_data() {
    let seed = None;
    let node_count = TEST_NET_SIZE;
    let iterations = test_utils::iterations();

    let group_size = 8;
    let network = Network::new(group_size, seed);
    let mut rng = network.new_rng();
    let mut event_count = 0;
    let mut nodes = test_node::create_nodes(&network, node_count, None, false);

    let config = BootstrapConfig::with_contacts(&[nodes[0].endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    let mut version = 0;
    let mut auth_keys = BTreeSet::new();
    let mut mutate_count = 1;
    for i in 0..iterations {
        trace!("Iteration {}.", i + 1);
        let data = test_utils::gen_immutable_data(1024, &mut rng);
        let msg_id_d = client.put_idata(data);

        let (app_key, _) = sign::gen_keypair();
        let msg_id_k = client.ins_auth_key(app_key, version + 1);

        event_count += poll::nodes_and_client(&mut nodes, &mut client);
        trace!("Processed {} events.", event_count);

        // TODO: advance clock and create another event to trigger expiration check.
        while let Ok(event) = client.try_recv() {
            if let Event::Response { response: Response::InsAuthKey { res, .. }, .. } =
                event.clone()
            {
                match res {
                    Ok(()) => {
                        version += 1;
                        let _ = auth_keys.insert(app_key);
                    }
                    Err(error) => {
                        trace!(
                            "Received failed response of insertion {:?}. Reason: {:?}",
                            msg_id_k,
                            error
                        );
                    }
                }
            }
            if let Event::Response { response: Response::PutIData { res, .. }, .. } = event {
                match res {
                    Ok(()) => mutate_count += 1,
                    Err(error) => {
                        trace!(
                            "Received failed response of put data {:?}. Reason: {:?}",
                            msg_id_d,
                            error
                        );
                    }
                }
            }
        }

        match client.list_auth_keys_and_version_response(&mut nodes) {
            Ok(result) => assert_eq!(result, (auth_keys.clone(), version)),
            Err(err) => panic!("Unexpected error {:?} when list auth_keys and version", err),
        }
    }

    let sorted_nodes = test_node::closest_to(&nodes, client.name(), group_size);
    let node_count_stats: Vec<_> = sorted_nodes
        .into_iter()
        .map(|node| {
            (
                node.name(),
                node.get_maid_manager_mutation_count(client.name()),
            )
        })
        .collect();

    for &(_, count) in &node_count_stats {
        assert_eq!(count, Some(mutate_count));
    }
}

/// Multiple requests with the same message IDs should not be allowed.
#[test]
fn reusing_msg_ids() {
    let seed = None;
    let node_count = 8;

    let group_size = 8;
    let network = Network::new(group_size, seed);
    let mut rng = network.new_rng();
    let mut nodes = test_node::create_nodes(&network, node_count, None, false);

    let config = BootstrapConfig::with_contacts(&[unwrap!(rng.choose(&nodes)).endpoint()]);
    let mut client = TestClient::new(&network, Some(config));
    client.ensure_connected(&mut nodes);
    client.create_account(&mut nodes);

    let balance0 = unwrap!(client.get_account_info_response(&mut nodes));

    // Sequential requests

    let data0 = test_utils::gen_immutable_data(10, &mut rng);
    let data1 = test_utils::gen_immutable_data(10, &mut rng);
    let msg_id = MessageId::new();

    unwrap!(client.put_idata_response_with_msg_id(
        data0.clone(),
        msg_id,
        &mut nodes,
    ));
    match client.put_idata_response_with_msg_id(data1.clone(), msg_id, &mut nodes) {
        Err(ClientError::InvalidOperation) => (),
        Err(error) => panic!("Unexpected error: {:?}", error),
        Ok(()) => panic!("Unexpected success"),
    }

    // Only one chunk is stored.
    let retrieved_data = unwrap!(client.get_idata_response(*data0.name(), &mut nodes));
    assert_eq!(retrieved_data, data0);
    assert_eq!(
        client.get_idata_response(*data1.name(), &mut nodes),
        Err(ClientError::NoSuchData)
    );

    // Only one request is charged.
    let balance1 = unwrap!(client.get_account_info_response(&mut nodes));
    assert_eq!(balance1.mutations_done, balance0.mutations_done + 1);

    // Concurrent requests

    let data0 = test_utils::gen_immutable_data(10, &mut rng);
    let data1 = test_utils::gen_immutable_data(10, &mut rng);
    let msg_id = MessageId::new();
    client.put_idata_with_msg_id(data0.clone(), msg_id);
    client.put_idata_with_msg_id(data1.clone(), msg_id);

    let _ = poll::nodes_and_client(&mut nodes, &mut client);

    let mut successes = 0;
    while let Ok(event) = client.try_recv() {
        if let Event::Response { response: Response::PutIData { res: Ok(()), .. }, .. } = event {
            successes += 1;
        }
    }

    // At most one request does succeed.
    assert!(successes <= 1);

    // At most one chunk is stored and at most one request is charged.
    let res0 = client.get_idata_response(*data0.name(), &mut nodes);
    let res1 = client.get_idata_response(*data1.name(), &mut nodes);
    let balance2 = unwrap!(client.get_account_info_response(&mut nodes));

    if successes > 0 {
        assert!((res0.is_ok() && res1.is_err()) || (res0.is_err() && res1.is_ok()));
        assert_eq!(balance2.mutations_done, balance1.mutations_done + 1);
    } else {
        assert!(res0.is_err() && res1.is_err());
        assert_eq!(balance2.mutations_done, balance1.mutations_done);
    }
}

// Test the concurrently claiming invitation workflow:
// 1. Put a new invite on the network by an admin client
// 2. Have two clients concurrently claim that same invitation
// Expect at most once succeed and verify the invitation cannot be reused.
#[test]
fn claiming_invitation_concurrently() {
    let seed = None;
    let group_size = 8;
    let node_count = group_size;
    let network = Network::new(group_size, seed);
    let admin_id = FullId::new();
    let vault_config = Config {
        invite_key: Some(admin_id.public_id().signing_public_key().0),
        ..Default::default()
    };

    let mut rng = network.new_rng();
    let mut nodes = test_node::create_nodes(&network, node_count, Some(vault_config), false);
    let config =
        BootstrapConfig::with_contacts(&[unwrap!(rng.choose(&nodes), "no nodes found").endpoint()]);

    let mut admin_client = TestClient::with_id(&network, Some(config.clone()), admin_id);
    admin_client.ensure_connected(&mut nodes);
    admin_client.create_account(&mut nodes);

    let invite_code = "invite";

    // Put the invite
    let name = XorName(tiny_keccak::sha3_256(invite_code.as_bytes()));
    let mut owners = BTreeSet::new();
    let _ = owners.insert(*admin_client.signing_public_key());
    let mut permissions = BTreeMap::new();
    let _ = permissions.insert(User::Anyone, PermissionSet::new().allow(Action::Insert));
    let data = unwrap!(MutableData::new(
        name,
        TYPE_TAG_INVITE,
        permissions.clone(),
        Default::default(),
        owners,
    ));
    unwrap!(admin_client.put_mdata_response(data, &mut nodes));

    let mut clients: Vec<_> = (0..2)
        .map(|_| {
            let endpoint = unwrap!(rng.choose(&nodes), "no nodes found").endpoint();
            let config = BootstrapConfig::with_contacts(&[endpoint]);
            TestClient::new(&network, Some(config.clone()))
        })
        .collect();

    for client in &mut clients {
        client.ensure_connected(&mut nodes);
    }

    for client in &mut clients {
        let _ = client.create_account_with_invitation(invite_code);
    }

    let _ = poll::nodes_and_clients_parallel(&mut nodes, &mut clients);

    let mut succeeded = 0;
    for client in &mut clients {
        while let Ok(event) = client.try_recv() {
            if let Event::Response { response: Response::PutMData { res, .. }, .. } = event {
                match res {
                    Ok(()) => succeeded += 1,
                    Err(error) => {
                        trace!(
                            "Client {:?} received failed response. Reason: {:?}",
                            client.name(),
                            error
                        );
                    }
                }
            }
        }
    }
    assert!(succeeded <= 1);

    // Attempt to reuse already claimed invite fails.
    let mut client3 = TestClient::new(&network, Some(config));
    client3.ensure_connected(&mut nodes);

    match client3.create_account_with_invitation_response(invite_code, &mut nodes) {
        Ok(()) => panic!("re-claiming a used invitation shall not succeed."),
        // `Err(NetworkOther("Error claiming invitation: Conflicting concurrent mutation"))`
        Err(ClientError::NetworkOther(_)) |
        Err(ClientError::InvitationAlreadyClaimed) => {}
        Err(err) => panic!("Received unexpected error: {:?}", err),
    }
}
