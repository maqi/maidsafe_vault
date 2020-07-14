// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{
    chunk_store::{error::Error as ChunkStoreError, MutableChunkStore},
    cmd::OutboundMsg,
    node::msg_decisions::ElderMsgDecisions,
    node::Init,
    Config, Result,
};
use safe_nd::{
    CmdError, Error as NdError, MData, MDataAction, MDataAddress, MDataEntryActions,
    MDataPermissionSet, MDataValue, MapRead, MapWrite, Message, MessageId, MsgEnvelope, MsgSender,
    PublicKey, QueryResponse, Result as NdResult,
};
use std::{
    cell::Cell,
    fmt::{self, Display, Formatter},
    rc::Rc,
};

pub(super) struct MapStorage {
    chunks: MutableChunkStore,
    decisions: ElderMsgDecisions,
}

impl MapStorage {
    pub(super) fn new(
        config: &Config,
        total_used_space: &Rc<Cell<u64>>,
        init_mode: Init,
        decisions: ElderMsgDecisions,
    ) -> Result<Self> {
        let root_dir = config.root_dir()?;
        let max_capacity = config.max_capacity();
        let chunks = MutableChunkStore::new(
            &root_dir,
            max_capacity,
            Rc::clone(total_used_space),
            init_mode,
        )?;
        Ok(Self { chunks, decisions })
    }

    pub(super) fn read(&self, read: &MapRead, msg: &MsgEnvelope) -> Option<OutboundMsg> {
        use MapRead::*;
        match read {
            Get(address) => self.get(*address, msg.id(), &msg.origin),
            GetValue { address, ref key } => self.get_value(*address, key, msg.id(), &msg.origin),
            GetShell(address) => self.get_shell(*address, msg.id(), &msg.origin),
            GetVersion(address) => self.get_version(*address, msg.id(), &msg.origin),
            ListEntries(address) => self.list_entries(*address, msg.id(), &msg.origin),
            ListKeys(address) => self.list_keys(*address, msg.id(), &msg.origin),
            ListValues(address) => self.list_values(*address, msg.id(), &msg.origin),
            ListPermissions(address) => self.list_permissions(*address, msg.id(), &msg.origin),
            ListUserPermissions { address, user } => {
                self.list_user_permissions(*address, *user, msg.id(), &msg.origin)
            }
        }
    }

    pub(super) fn write(
        &mut self,
        write: MapWrite,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        use MapWrite::*;
        match write {
            New(data) => self.create(&data, msg_id, origin),
            Delete(address) => self.delete(address, msg_id, origin),
            SetUserPermissions {
                address,
                user,
                ref permissions,
                version,
            } => self.set_user_permissions(address, user, permissions, version, msg_id, origin),
            DelUserPermissions {
                address,
                user,
                version,
            } => self.delete_user_permissions(address, user, version, msg_id, origin),
            Edit { address, changes } => self.edit_entries(address, changes, msg_id, origin),
        }
    }

    /// Get `MData` from the chunk store and check permissions.
    /// Returns `Some(Result<..>)` if the flow should be continued, returns
    /// `None` if there was a logic error encountered and the flow should be
    /// terminated.
    fn get_chunk(
        &self,
        address: &MDataAddress,
        origin: &MsgSender,
        action: MDataAction,
    ) -> Option<NdResult<MData>> {
        Some(
            self.chunks
                .get(&address)
                .map_err(|e| match e {
                    ChunkStoreError::NoSuchChunk => NdError::NoSuchData,
                    error => error.to_string().into(),
                })
                .and_then(move |mdata| {
                    mdata
                        .check_permissions(action, *origin.id())
                        .map(move |_| mdata)
                }),
        )
    }

    /// Get MData from the chunk store, update it, and overwrite the stored chunk.
    fn edit_chunk<F>(
        &mut self,
        address: &MDataAddress,
        origin: &MsgSender,
        msg_id: MessageId,
        mutation_fn: F,
    ) -> Option<OutboundMsg>
    where
        F: FnOnce(MData) -> NdResult<MData>,
    {
        let result = self
            .chunks
            .get(address)
            .map_err(|e| match e {
                ChunkStoreError::NoSuchChunk => NdError::NoSuchData,
                error => error.to_string().into(),
            })
            .and_then(mutation_fn)
            .and_then(|mdata| {
                self.chunks
                    .put(&mdata)
                    .map_err(|error| error.to_string().into())
            });
        self.ok_or_error(result, msg_id, &origin)
    }

    /// Put MData.
    fn create(
        &mut self,
        data: &MData,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = if self.chunks.has(data.address()) {
            Err(NdError::DataExists)
        } else {
            self.chunks
                .put(&data)
                .map_err(|error| error.to_string().into())
        };
        self.ok_or_error(result, msg_id, origin)
    }

    fn delete(
        &mut self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .chunks
            .get(&address)
            .map_err(|e| match e {
                ChunkStoreError::NoSuchChunk => NdError::NoSuchData,
                error => error.to_string().into(),
            })
            .and_then(|mdata| {
                mdata.check_is_owner(*origin.id())?;
                self.chunks
                    .delete(&address)
                    .map_err(|error| error.to_string().into())
            });

        self.ok_or_error(result, msg_id, origin)
    }

    /// Set MData user permissions.
    fn set_user_permissions(
        &mut self,
        address: MDataAddress,
        user: PublicKey,
        permissions: &MDataPermissionSet,
        version: u64,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        self.edit_chunk(&address, origin, msg_id, move |mut data| {
            data.check_permissions(MDataAction::ManagePermissions, *origin.id())?;
            data.set_user_permissions(user, permissions.clone(), version)?;
            Ok(data)
        })
    }

    /// Delete MData user permissions.
    fn delete_user_permissions(
        &mut self,
        address: MDataAddress,
        user: PublicKey,
        version: u64,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        self.edit_chunk(&address, origin, msg_id, move |mut data| {
            data.check_permissions(MDataAction::ManagePermissions, *origin.id())?;
            data.del_user_permissions(user, version)?;
            Ok(data)
        })
    }

    /// Edit MData.
    fn edit_entries(
        &mut self,
        address: MDataAddress,
        actions: MDataEntryActions,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        self.edit_chunk(&address, origin, msg_id, move |mut data| {
            data.mutate_entries(actions, *origin.id())?;
            Ok(data)
        })
    }

    /// Get entire MData.
    fn get(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self.get_chunk(&address, origin, MDataAction::Read)?;
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::GetMap(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData shell.
    fn get_shell(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .get_chunk(&address, origin, MDataAction::Read)?
            .map(|data| data.shell());
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::GetMapShell(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData version.
    fn get_version(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .get_chunk(&address, origin, MDataAction::Read)?
            .map(|data| data.version());
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::GetMapVersion(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData value.
    fn get_value(
        &self,
        address: MDataAddress,
        key: &[u8],
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let res = self.get_chunk(&address, origin, MDataAction::Read)?;
        let result = res.and_then(|data| match data {
            MData::Seq(md) => md
                .get(key)
                .cloned()
                .map(MDataValue::from)
                .ok_or_else(|| NdError::NoSuchEntry),
            MData::Unseq(md) => md
                .get(key)
                .cloned()
                .map(MDataValue::from)
                .ok_or_else(|| NdError::NoSuchEntry),
        });
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::GetMapValue(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData keys.
    fn list_keys(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .get_chunk(&address, origin, MDataAction::Read)?
            .map(|data| data.keys());
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::ListMapKeys(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData values.
    fn list_values(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let res = self.get_chunk(&address, origin, MDataAction::Read)?;
        let result = res.and_then(|data| match data {
            MData::Seq(md) => Ok(md.values().into()),
            MData::Unseq(md) => Ok(md.values().into()),
        });
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::ListMapValues(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData entries.
    fn list_entries(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let res = self.get_chunk(&address, origin, MDataAction::Read)?;
        let result = res.and_then(|data| match data {
            MData::Seq(md) => Ok(md.entries().clone().into()),
            MData::Unseq(md) => Ok(md.entries().clone().into()),
        });
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::ListMapEntries(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData permissions.
    fn list_permissions(
        &self,
        address: MDataAddress,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .get_chunk(&address, origin, MDataAction::Read)?
            .map(|data| data.permissions());
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::ListMapPermissions(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    /// Get MData user permissions.
    fn list_user_permissions(
        &self,
        address: MDataAddress,
        user: PublicKey,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        let result = self
            .get_chunk(&address, origin, MDataAction::Read)?
            .and_then(|data| data.user_permissions(user).map(MDataPermissionSet::clone));
        self.decisions.send(Message::QueryResponse {
            response: QueryResponse::ListMapUserPermissions(result),
            id: MessageId::new(),
            correlation_id: msg_id,
            query_origin: origin.address(),
        })
    }

    fn ok_or_error(
        &self,
        result: NdResult<()>,
        msg_id: MessageId,
        origin: &MsgSender,
    ) -> Option<OutboundMsg> {
        if let Err(error) = result {
            self.decisions.error(CmdError::Data(error), msg_id, origin)
        } else {
            None
        }
    }
}

impl Display for MapStorage {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "{}", "MapStorage")
    }
}