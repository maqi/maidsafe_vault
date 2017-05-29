// Copyright 2017 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

mod account;
#[cfg(all(test, feature = "use-mock-routing"))]
mod tests;

use self::account::Account;
pub use self::account::DEFAULT_ACCOUNT_SIZE;
use GROUP_SIZE;
use error::InternalError;
use itertools::Itertools;
use lru_time_cache::LruCache;
use maidsafe_utilities::serialisation;
use routing::{Authority, ClientError, EntryAction, ImmutableData, MessageId, MutableData,
              PermissionSet, RoutingTable, TYPE_TAG_SESSION_PACKET, User, XorName};
use rust_sodium::crypto::sign;
use std::cmp::max;
use std::collections::{BTreeMap, BTreeSet};
use std::collections::hash_map::Entry;
use std::time::Duration;
use utils::{self, HashMap, Instant};
use vault::RoutingNode;

/// The timeout for cached request from client; if no consensus is reached, the request is dropped.
const PENDING_REQUEST_TIMEOUT_SECS: u64 = 60;

pub struct MaidManager {
    accounts: HashMap<XorName, Account>,
    request_cache: HashMap<MessageId, CachedRequest>,
    data_ops_count_cache: LruCache<XorName, u64>,
}

impl MaidManager {
    pub fn new() -> MaidManager {
        MaidManager {
            accounts: HashMap::default(),
            request_cache: HashMap::default(),
            data_ops_count_cache:
                LruCache::with_expiry_duration(Duration::from_secs(PENDING_REQUEST_TIMEOUT_SECS)),
        }
    }

    pub fn handle_tick(&mut self, routing_node: &mut RoutingNode) -> Result<(), InternalError> {
        Ok(self.remove_expired_requests(routing_node))
    }

    pub fn handle_refresh(&mut self,
                          routing_node: &mut RoutingNode,
                          serialised_msg: &[u8],
                          msg_id: MessageId)
                          -> Result<(), InternalError> {
        match serialisation::deserialise::<Refresh>(serialised_msg)? {
            Refresh::Update(maid_name, account) => {
                trace!("MM {:?} received Refresh::Update for account {:?} - {:?}",
                       routing_node.name(),
                       maid_name,
                       account);
                if routing_node.close_group(maid_name, GROUP_SIZE).is_none() {
                    return Ok(());
                }
                let account_count = self.accounts.len();
                match self.accounts.entry(maid_name) {
                    Entry::Vacant(entry) => {
                        let _ = entry.insert(account);
                        info!("Managing {} client accounts.", account_count + 1);
                    }
                    Entry::Occupied(mut entry) => {
                        if entry.get().version < account.version {
                            entry.get_mut().set_keys_info(&account);
                        }
                        if entry.get().info.mutations_done < account.info.mutations_done {
                            entry.get_mut().set_data_info(&account);
                        }
                        trace!("Client account {:?}: {:?}", maid_name, entry.get());
                    }
                }
                let CachedRequest { req_type, src, dst, .. } = self.remove_cached_request(msg_id)?;
                self.send_response_on_req_type(routing_node, req_type, src, dst, Ok(()), msg_id);
            }
            Refresh::Delete(maid_name) => {
                let _ = self.accounts.remove(&maid_name);
                info!("Managing {} client accounts.", self.accounts.len());
            }
        }
        Ok(())
    }

    pub fn handle_get_account_info(&mut self,
                                   routing_node: &mut RoutingNode,
                                   src: Authority<XorName>,
                                   dst: Authority<XorName>,
                                   msg_id: MessageId)
                                   -> Result<(), InternalError> {
        let res = self.get_account(&src, &dst).map(|account| account.info);
        routing_node
            .send_get_account_info_response(dst, src, res, msg_id)?;
        Ok(())
    }

    pub fn handle_put_idata(&mut self,
                            routing_node: &mut RoutingNode,
                            src: Authority<XorName>,
                            dst: Authority<XorName>,
                            data: ImmutableData,
                            msg_id: MessageId)
                            -> Result<(), InternalError> {
        if !data.validate_size() {
            routing_node
                .send_put_idata_response(dst, src, Err(ClientError::DataTooLarge), msg_id)?;
            return Ok(());
        }

        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Key, None) {
            routing_node
                .send_put_idata_response(dst, src, Err(err), msg_id)?;
            return Ok(());
        }

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(*data.name());
        trace!("MM forwarding PutIData request to {:?}", fwd_dst);
        routing_node
            .send_put_idata_request(fwd_src, fwd_dst, data, msg_id)?;

        self.insert_cached_request(RequestType::PutIData, msg_id, src, dst, None);

        Ok(())
    }

    pub fn handle_put_idata_response(&mut self,
                                     routing_node: &mut RoutingNode,
                                     res: Result<(), ClientError>,
                                     msg_id: MessageId)
                                     -> Result<(), InternalError> {
        let CachedRequest { src, dst, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;
        // Send the response back to client
        routing_node
            .send_put_idata_response(dst, src, res, msg_id)?;
        Ok(())
    }

    pub fn handle_put_mdata(&mut self,
                            routing_node: &mut RoutingNode,
                            src: Authority<XorName>,
                            dst: Authority<XorName>,
                            data: MutableData,
                            msg_id: MessageId,
                            requester: sign::PublicKey)
                            -> Result<(), InternalError> {
        if let Err(err) = data.validate() {
            routing_node
                .send_put_mdata_response(dst, src, Err(err), msg_id)?;
            return Ok(());
        }

        let src_name = utils::client_name(&src);
        let dst_name = utils::client_name(&dst);

        if !utils::verify_mdata_owner(&data, &dst_name) {
            routing_node
                .send_put_mdata_response(dst, src, Err(ClientError::InvalidOwners), msg_id)?;
            return Ok(());
        }

        // If the type_tag is `TYPE_TAG_SESSION_PACKET`, the account must not exist, else it must
        // exist.
        if data.tag() == TYPE_TAG_SESSION_PACKET {
            if dst_name != src_name {
                trace!("MM Cannot create account for {:?} as {:?}.", src, dst);
                let err = ClientError::InvalidOperation;
                routing_node
                    .send_put_mdata_response(dst, src, Err(err.clone()), msg_id)?;
                return Ok(());
            }

            match self.accounts.entry(src_name) {
                Entry::Vacant(entry) => {
                    let _ = entry.insert(Account::default());
                }
                Entry::Occupied(_) => {
                    let err = ClientError::AccountExists;
                    trace!("MM Cannot create account for {:?} - it already exists", src);
                    routing_node
                        .send_put_mdata_response(dst, src, Err(err.clone()), msg_id)?;
                    return Ok(());
                }
            }
            info!("Managing {} client accounts.", self.accounts.len());
        }

        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Key, Some(requester)) {
            trace!("MM PutMData request failed: {:?}", err);
            // Undo the account creation
            if data.tag() == TYPE_TAG_SESSION_PACKET {
                let _ = self.accounts.remove(&src_name);
            }

            routing_node
                .send_put_mdata_response(dst, src, Err(err.clone()), msg_id)?;
            return Ok(());
        }

        let tag = data.tag();

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(*data.name());
        trace!("MM forwarding PutMData request to {:?}", fwd_dst);
        routing_node
            .send_put_mdata_request(fwd_src, fwd_dst, data, msg_id, requester)?;

        self.insert_cached_request(RequestType::PutMData, msg_id, src, dst, Some(tag));

        Ok(())
    }

    pub fn handle_put_mdata_response(&mut self,
                                     routing_node: &mut RoutingNode,
                                     res: Result<(), ClientError>,
                                     msg_id: MessageId)
                                     -> Result<(), InternalError> {
        let CachedRequest { src, dst, tag, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;

        let res = match (tag, res) {
            (_, Ok(())) => Ok(()),
            (Some(TYPE_TAG_SESSION_PACKET), Err(ClientError::DataExists)) => {
                // We wouldn't have forwarded two `Put` requests for the same account, so
                // it must have been created via another client manager.
                let client_name = utils::client_name(&src);
                let _ = self.accounts.remove(&client_name);
                let refresh = Refresh::Delete(client_name);
                if let Ok(serialised_refresh) = serialisation::serialise(&refresh) {
                    trace!("MM sending delete refresh for account {}", src.name());
                    let _ = routing_node.send_refresh_request(dst, dst, serialised_refresh, msg_id);
                }

                Err(ClientError::AccountExists)
            }
            (_, Err(err)) => Err(err),
        };

        // Send response back to client
        routing_node
            .send_put_mdata_response(dst, src, res, msg_id)?;
        Ok(())
    }

    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn handle_mutate_mdata_entries(&mut self,
                                       routing_node: &mut RoutingNode,
                                       src: Authority<XorName>,
                                       dst: Authority<XorName>,
                                       name: XorName,
                                       tag: u64,
                                       actions: BTreeMap<Vec<u8>, EntryAction>,
                                       msg_id: MessageId,
                                       requester: sign::PublicKey)
                                       -> Result<(), InternalError> {
        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Key, Some(requester)) {
            routing_node
                .send_mutate_mdata_entries_response(dst, src, Err(err), msg_id)?;
            return Ok(());
        }

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(name);
        trace!("MM forwarding MutateMDataEntries request to {:?}", fwd_dst);
        routing_node
            .send_mutate_mdata_entries_request(fwd_src,
                                               fwd_dst,
                                               name,
                                               tag,
                                               actions,
                                               msg_id,
                                               requester)?;

        self.insert_cached_request(RequestType::MutateMDataEntries, msg_id, src, dst, Some(tag));

        Ok(())
    }

    pub fn handle_mutate_mdata_entries_response(&mut self,
                                                routing_node: &mut RoutingNode,
                                                res: Result<(), ClientError>,
                                                msg_id: MessageId)
                                                -> Result<(), InternalError> {
        let CachedRequest { src, dst, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;
        routing_node
            .send_mutate_mdata_entries_response(dst, src, res, msg_id)?;
        Ok(())
    }

    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn handle_set_mdata_user_permissions(&mut self,
                                             routing_node: &mut RoutingNode,
                                             src: Authority<XorName>,
                                             dst: Authority<XorName>,
                                             name: XorName,
                                             tag: u64,
                                             user: User,
                                             permissions: PermissionSet,
                                             version: u64,
                                             msg_id: MessageId,
                                             requester: sign::PublicKey)
                                             -> Result<(), InternalError> {
        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Key, Some(requester)) {
            routing_node
                .send_set_mdata_user_permissions_response(dst, src, Err(err.clone()), msg_id)?;
            return Ok(());
        }

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(name);
        trace!("MM forwarding SetMDataUserPermissions request to {:?}",
               fwd_dst);
        routing_node
            .send_set_mdata_user_permissions_request(fwd_src,
                                                     fwd_dst,
                                                     name,
                                                     tag,
                                                     user,
                                                     permissions,
                                                     version,
                                                     msg_id,
                                                     requester)?;

        self.insert_cached_request(RequestType::SetMDataUserPermissions,
                                   msg_id,
                                   src,
                                   dst,
                                   Some(tag));
        Ok(())
    }

    pub fn handle_set_mdata_user_permissions_response(&mut self,
                                                      routing_node: &mut RoutingNode,
                                                      res: Result<(), ClientError>,
                                                      msg_id: MessageId)
                                                      -> Result<(), InternalError> {
        let CachedRequest { src, dst, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;
        routing_node
            .send_set_mdata_user_permissions_response(dst, src, res, msg_id)?;
        Ok(())
    }

    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn handle_del_mdata_user_permissions(&mut self,
                                             routing_node: &mut RoutingNode,
                                             src: Authority<XorName>,
                                             dst: Authority<XorName>,
                                             name: XorName,
                                             tag: u64,
                                             user: User,
                                             version: u64,
                                             msg_id: MessageId,
                                             requester: sign::PublicKey)
                                             -> Result<(), InternalError> {
        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Key, Some(requester)) {
            routing_node
                .send_del_mdata_user_permissions_response(dst, src, Err(err.clone()), msg_id)?;
            return Ok(());
        }

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(name);
        trace!("MM forwarding DelMDataUserPermissions request to {:?}",
               fwd_dst);
        routing_node
            .send_del_mdata_user_permissions_request(fwd_src,
                                                     fwd_dst,
                                                     name,
                                                     tag,
                                                     user,
                                                     version,
                                                     msg_id,
                                                     requester)?;

        self.insert_cached_request(RequestType::DelMDataUserPermissions,
                                   msg_id,
                                   src,
                                   dst,
                                   Some(tag));
        Ok(())
    }

    pub fn handle_del_mdata_user_permissions_response(&mut self,
                                                      routing_node: &mut RoutingNode,
                                                      res: Result<(), ClientError>,
                                                      msg_id: MessageId)
                                                      -> Result<(), InternalError> {
        let CachedRequest { src, dst, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;
        routing_node
            .send_del_mdata_user_permissions_response(dst, src, res, msg_id)?;
        Ok(())
    }

    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn handle_change_mdata_owner(&mut self,
                                     routing_node: &mut RoutingNode,
                                     src: Authority<XorName>,
                                     dst: Authority<XorName>,
                                     name: XorName,
                                     tag: u64,
                                     new_owners: BTreeSet<sign::PublicKey>,
                                     version: u64,
                                     msg_id: MessageId)
                                     -> Result<(), InternalError> {
        if let Err(err) = self.prepare_mutation(&src, &dst, AuthPolicy::Owner, None) {
            routing_node
                .send_change_mdata_owner_response(dst, src, Err(err.clone()), msg_id)?;
            return Ok(());
        }

        // Forwarding the request to NAE Manager.
        let fwd_src = dst;
        let fwd_dst = Authority::NaeManager(name);
        trace!("MM forwarding ChangeMDataOwner request to {:?}", fwd_dst);
        routing_node
            .send_change_mdata_owner_request(fwd_src,
                                             fwd_dst,
                                             name,
                                             tag,
                                             new_owners,
                                             version,
                                             msg_id)?;

        self.insert_cached_request(RequestType::ChangeMDataOwner, msg_id, src, dst, Some(tag));
        Ok(())
    }

    pub fn handle_change_mdata_owner_response(&mut self,
                                              routing_node: &mut RoutingNode,
                                              res: Result<(), ClientError>,
                                              msg_id: MessageId)
                                              -> Result<(), InternalError> {
        let CachedRequest { src, dst, .. } =
            self.handle_mutation_response(routing_node, msg_id, res.is_ok())?;
        routing_node
            .send_change_mdata_owner_response(dst, src, res, msg_id)?;
        Ok(())
    }

    pub fn handle_list_auth_keys_and_version(&mut self,
                                             routing_node: &mut RoutingNode,
                                             src: Authority<XorName>,
                                             dst: Authority<XorName>,
                                             msg_id: MessageId)
                                             -> Result<(), InternalError> {
        let res = self.get_account(&src, &dst)
            .map(|account| (account.auth_keys.clone(), account.version));
        routing_node
            .send_list_auth_keys_and_version_response(dst, src, res, msg_id)?;
        Ok(())
    }

    pub fn handle_ins_auth_key(&mut self,
                               routing_node: &mut RoutingNode,
                               src: Authority<XorName>,
                               dst: Authority<XorName>,
                               key: sign::PublicKey,
                               version: u64,
                               msg_id: MessageId)
                               -> Result<(), InternalError> {
        let res = self.mutate_account(&src, &dst, version, |account| {
            let _ = account.auth_keys.insert(key);
            Ok(())
        });
        match res {
            Ok(account) => {
                let _ = self.insert_cached_request(RequestType::InsAuthKey, msg_id, src, dst, None);
                let client_manager_name = utils::client_name(&dst);
                let _ = self.send_refresh(routing_node, &client_manager_name, account, msg_id);
            }
            Err(err) => {
                routing_node
                    .send_ins_auth_key_response(dst, src, Err(err), msg_id)?
            }
        }
        Ok(())
    }

    pub fn handle_del_auth_key(&mut self,
                               routing_node: &mut RoutingNode,
                               src: Authority<XorName>,
                               dst: Authority<XorName>,
                               key: sign::PublicKey,
                               version: u64,
                               msg_id: MessageId)
                               -> Result<(), InternalError> {
        let res = self.mutate_account(&src,
                                      &dst,
                                      version,
                                      |account| if account.auth_keys.remove(&key) {
                                          Ok(())
                                      } else {
                                          Err(ClientError::NoSuchKey)
                                      });
        match res {
            Ok(account) => {
                let _ = self.insert_cached_request(RequestType::DelAuthKey, msg_id, src, dst, None);
                let client_manager_name = utils::client_name(&dst);
                let _ = self.send_refresh(routing_node, &client_manager_name, account, msg_id);
            }
            Err(err) => {
                routing_node
                    .send_del_auth_key_response(dst, src, Err(err), msg_id)?
            }
        }
        Ok(())
    }

    pub fn handle_node_added(&mut self,
                             routing_node: &mut RoutingNode,
                             node_name: &XorName,
                             routing_table: &RoutingTable<XorName>)
                             -> Result<(), InternalError> {
        // Remove all accounts which we are no longer responsible for.
        let accounts_to_delete = self.accounts
            .keys()
            .filter(|name| !routing_table.is_closest(*name, GROUP_SIZE))
            .cloned()
            .collect_vec();

        // Remove all requests from the cache that we are no longer responsible for.
        let msg_ids_to_delete = self.request_cache
            .iter()
            .filter(|&(_, entry)| accounts_to_delete.contains(&entry.src.name()))
            .map(|(msg_id, _)| *msg_id)
            .collect_vec();
        for msg_id in msg_ids_to_delete {
            let _ = self.request_cache.remove(&msg_id);
        }

        for maid_name in &accounts_to_delete {
            trace!("No longer a MM for {}", maid_name);
            let _ = self.accounts.remove(maid_name);
        }

        if !accounts_to_delete.is_empty() {
            info!("Managing {} client accounts.",
                  self.accounts.len() - accounts_to_delete.len());
        }

        // Send refresh messages for the remaining accounts.
        for (maid_name, account) in &self.accounts {
            self.send_refresh(routing_node,
                              maid_name,
                              account.clone(),
                              MessageId::from_added_node(*node_name));
        }

        Ok(())
    }

    pub fn handle_node_lost(&mut self,
                            routing_node: &mut RoutingNode,
                            node_name: &XorName)
                            -> Result<(), InternalError> {
        for (maid_name, account) in &self.accounts {
            self.send_refresh(routing_node,
                              maid_name,
                              account.clone(),
                              MessageId::from_lost_node(*node_name));
        }

        Ok(())
    }

    fn get_account(&self,
                   src: &Authority<XorName>,
                   dst: &Authority<XorName>)
                   -> Result<&Account, ClientError> {
        let requestor_name = utils::client_name(src);
        let client_name = utils::client_name(dst);
        if requestor_name != client_name {
            trace!("MM Cannot allow requestor {:?} accessing account {:?}.",
                   src,
                   dst);
            return Err(ClientError::AccessDenied);
        }
        if let Some(account) = self.accounts.get(&client_name) {
            Ok(account)
        } else {
            Err(ClientError::NoSuchAccount)
        }
    }

    fn mutate_account<F>(&mut self,
                         src: &Authority<XorName>,
                         dst: &Authority<XorName>,
                         version: u64,
                         f: F)
                         -> Result<Account, ClientError>
        where F: FnOnce(&mut Account) -> Result<(), ClientError>
    {
        let client_name = utils::client_name(src);
        let client_manager_name = utils::client_name(dst);

        if client_name != client_manager_name {
            return Err(ClientError::AccessDenied);
        }

        let res = if let Some(account) = self.accounts.get(&client_manager_name) {
            let mut account_copy = account.clone();
            if version == account_copy.version + 1 {
                f(&mut account_copy)?;
                account_copy.version = version;
                Ok(account_copy)
            } else {
                Err(ClientError::InvalidSuccessor)
            }
        } else {
            Err(ClientError::NoSuchAccount)
        };

        res
    }

    fn prepare_mutation(&mut self,
                        src: &Authority<XorName>,
                        dst: &Authority<XorName>,
                        policy: AuthPolicy,
                        requester: Option<sign::PublicKey>)
                        -> Result<(), ClientError> {
        let client_manager_name = utils::client_name(dst);

        let account = if let Some(account) = self.accounts.get(&client_manager_name) {
            account
        } else {
            return Err(ClientError::NoSuchAccount);
        };

        let client_key = utils::client_key(src);
        let client_name = utils::client_name_from_key(client_key);

        let allowed = client_name == client_manager_name ||
                      if AuthPolicy::Key == policy {
                          account.auth_keys.contains(client_key)
                      } else {
                          false
                      };

        if !allowed {
            return Err(ClientError::AccessDenied);
        }

        if let Some(requester) = requester {
            if requester != *client_key {
                return Err(ClientError::AccessDenied);
            }
        }
        Ok(())
    }

    fn handle_mutation_response(&mut self,
                                routing_node: &mut RoutingNode,
                                msg_id: MessageId,
                                success: bool)
                                -> Result<CachedRequest, InternalError> {
        let cached_req = self.remove_cached_request(msg_id)?;

        let client_name = utils::client_name(&cached_req.dst);
        if let Some(account) = self.accounts.get(&client_name) {
            let mut account_copy = account.clone();
            if success {
                let mutations_done = account.info.mutations_done;
                let count_vote = max(mutations_done,
                                     self.data_ops_count_cache
                                         .get(&client_name)
                                         .map_or(0, |record| *record)) +
                                 1;
                account_copy.set_mutation_counter(count_vote - mutations_done);
                self.send_refresh(routing_node, &client_name, account_copy, MessageId::zero());
                let _ = self.data_ops_count_cache.insert(client_name, count_vote);
            }
        } else {
            error!("Account for {:?} not found.", client_name);
            return Err(InternalError::NoSuchAccount);
        };

        Ok(cached_req)
    }

    fn send_refresh(&self,
                    routing_node: &mut RoutingNode,
                    maid_name: &XorName,
                    account: Account,
                    msg_id: MessageId) {
        let src = Authority::ClientManager(*maid_name);
        let refresh = Refresh::Update(*maid_name, account.clone());
        if let Ok(serialised_refresh) = serialisation::serialise(&refresh) {
            trace!("MM {:?} sending refresh for account {} - {:?} with msg_id {:?}",
                   routing_node.name(),
                   src.name(),
                   account,
                   msg_id);
            let _ = routing_node.send_refresh_request(src, src, serialised_refresh, msg_id);
        }
    }

    fn insert_cached_request(&mut self,
                             req_type: RequestType,
                             msg_id: MessageId,
                             src: Authority<XorName>,
                             dst: Authority<XorName>,
                             tag: Option<u64>) {
        if let Some(prior) = self.request_cache
               .insert(msg_id,
                       CachedRequest {
                           req_type: req_type,
                           src: src,
                           dst: dst,
                           tag: tag,
                           time_stamp: Instant::now(),
                       }) {
            error!("Overwrote existing cached request with {:?} from {:?} to {:?}",
                   msg_id,
                   prior.src,
                   prior.dst);
        }
    }

    fn remove_cached_request(&mut self, msg_id: MessageId) -> Result<CachedRequest, InternalError> {
        self.request_cache
            .remove(&msg_id)
            .ok_or_else(move || InternalError::FailedToFindCachedRequest(msg_id))
    }

    fn remove_expired_requests(&mut self, routing_node: &mut RoutingNode) {
        let timeout = Duration::from_secs(PENDING_REQUEST_TIMEOUT_SECS);
        let mut expired_requests = Vec::new();

        let expired_msg_ids: Vec<_> = self.request_cache
            .iter()
            .filter_map(|entry| if entry.1.time_stamp.elapsed() > timeout {
                            Some(entry.0.clone())
                        } else {
                            None
                        })
            .collect();

        for msg_id in expired_msg_ids {
            let _ = self.request_cache
                .remove(&msg_id)
                .map(|request| expired_requests.push((msg_id, request)));
        }

        for (msg_id, CachedRequest { req_type, src, dst, .. }) in expired_requests {
            let error = Err(ClientError::from("Request expired."));
            trace!("request {:?} did not accumulate. Sending failure", msg_id);
            self.send_response_on_req_type(routing_node, req_type, src, dst, error, msg_id);
        }
    }

    fn send_response_on_req_type(&mut self,
                                 routing_node: &mut RoutingNode,
                                 req_type: RequestType,
                                 src: Authority<XorName>,
                                 dst: Authority<XorName>,
                                 res: Result<(), ClientError>,
                                 msg_id: MessageId) {
        use self::RequestType::*;
        match req_type {
            PutIData => unwrap!(routing_node.send_put_idata_response(dst, src, res, msg_id)),
            PutMData => unwrap!(routing_node.send_put_mdata_response(dst, src, res, msg_id)),
            InsAuthKey => unwrap!(routing_node.send_ins_auth_key_response(dst, src, res, msg_id)),
            DelAuthKey => unwrap!(routing_node.send_del_auth_key_response(dst, src, res, msg_id)),
            MutateMDataEntries => {
                unwrap!(routing_node.send_mutate_mdata_entries_response(dst, src, res, msg_id))
            }
            SetMDataUserPermissions => {
                unwrap!(routing_node.send_set_mdata_user_permissions_response(dst,
                                                                              src,
                                                                              res,
                                                                              msg_id))
            }
            DelMDataUserPermissions => {
                unwrap!(routing_node.send_del_mdata_user_permissions_response(dst,
                                                                              src,
                                                                              res,
                                                                              msg_id))
            }
            ChangeMDataOwner => {
                unwrap!(routing_node.send_change_mdata_owner_response(dst, src, res, msg_id))
            }
        }
    }
}

#[cfg(feature = "use-mock-crust")]
impl MaidManager {
    pub fn get_mutation_count(&self, client_name: &XorName) -> Option<u64> {
        self.accounts
            .get(client_name)
            .map(|account| account.info.mutations_done)
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
enum Refresh {
    Update(XorName, Account),
    Delete(XorName),
}

// Entry in the request cache.
struct CachedRequest {
    req_type: RequestType,
    src: Authority<XorName>,
    dst: Authority<XorName>,
    // Some(type_tag) if the request is for mutable data. None otherwise.
    tag: Option<u64>,
    time_stamp: Instant,
}

#[derive(PartialEq)]
enum AuthPolicy {
    // Operation allowed only for the account owner.
    Owner,
    // Operation allowed for any authorised client.
    Key,
}

enum RequestType {
    PutIData,
    PutMData,
    InsAuthKey,
    DelAuthKey,
    MutateMDataEntries,
    SetMDataUserPermissions,
    DelMDataUserPermissions,
    ChangeMDataOwner,
}
