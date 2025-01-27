// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::iter::once;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use mrecordlog::error::{CreateQueueError, DeleteQueueError, TruncateError};
use mrecordlog::MultiRecordLog;
use quickwit_common::tower::Pool;
use quickwit_common::ServiceStream;
use quickwit_proto::ingest::ingester::{
    AckReplicationMessage, FetchResponseV2, IngesterService, IngesterServiceClient,
    IngesterServiceStream, OpenFetchStreamRequest, OpenReplicationStreamRequest,
    OpenReplicationStreamResponse, PersistFailure, PersistFailureReason, PersistRequest,
    PersistResponse, PersistSuccess, PingRequest, PingResponse, ReplicateRequest,
    ReplicateSubrequest, SynReplicationMessage, TruncateRequest, TruncateResponse,
};
use quickwit_proto::ingest::{CommitTypeV2, IngestV2Error, IngestV2Result, ShardState};
use quickwit_proto::types::{NodeId, Position, QueueId};
use tokio::sync::RwLock;
use tracing::{error, info};

use super::fetch::FetchTask;
use super::models::{IngesterShard, PrimaryShard};
use super::mrecord::{is_eof_mrecord, MRecord};
use super::replication::{
    ReplicationStreamTask, ReplicationStreamTaskHandle, ReplicationTask, ReplicationTaskHandle,
    SYN_REPLICATION_STREAM_CAPACITY,
};
use super::IngesterPool;
use crate::ingest_v2::models::SoloShard;
use crate::metrics::INGEST_METRICS;
use crate::{FollowerId, LeaderId};

/// Duration after which persist requests time out with
/// [`quickwit_proto::ingest::IngestV2Error::Timeout`].
pub(super) const PERSIST_REQUEST_TIMEOUT: Duration = if cfg!(any(test, feature = "testsuite")) {
    Duration::from_millis(10)
} else {
    Duration::from_secs(6)
};

#[derive(Clone)]
pub struct Ingester {
    self_node_id: NodeId,
    ingester_pool: IngesterPool,
    state: Arc<RwLock<IngesterState>>,
    replication_factor: usize,
}

impl fmt::Debug for Ingester {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ingester")
            .field("replication_factor", &self.replication_factor)
            .finish()
    }
}

pub(super) struct IngesterState {
    pub mrecordlog: MultiRecordLog,
    pub shards: HashMap<QueueId, IngesterShard>,
    // Replication stream opened with followers.
    pub replication_streams: HashMap<FollowerId, ReplicationStreamTaskHandle>,
    // Replication tasks running for each replication stream opened with leaders.
    pub replication_tasks: HashMap<LeaderId, ReplicationTaskHandle>,
}

impl Ingester {
    pub async fn try_new(
        self_node_id: NodeId,
        ingester_pool: Pool<NodeId, IngesterServiceClient>,
        wal_dir_path: &Path,
        replication_factor: usize,
    ) -> IngestV2Result<Self> {
        let mrecordlog = MultiRecordLog::open_with_prefs(
            wal_dir_path,
            mrecordlog::SyncPolicy::OnDelay(Duration::from_secs(5)),
        )
        .await
        .map_err(|error| IngestV2Error::Internal(error.to_string()))?;

        let inner = IngesterState {
            mrecordlog,
            shards: HashMap::new(),
            replication_streams: HashMap::new(),
            replication_tasks: HashMap::new(),
        };
        let mut ingester = Self {
            self_node_id,
            ingester_pool,
            state: Arc::new(RwLock::new(inner)),
            replication_factor,
        };
        info!(
            replication_factor=%replication_factor,
            wal_dir=%wal_dir_path.display(),
            "spawning ingester"
        );
        ingester.init().await?;

        Ok(ingester)
    }

    async fn init(&mut self) -> IngestV2Result<()> {
        let mut state_guard = self.state.write().await;

        let queue_ids: Vec<QueueId> = state_guard
            .mrecordlog
            .list_queues()
            .map(|queue_id| queue_id.to_string())
            .collect();

        for queue_id in queue_ids {
            append_eof_record_if_necessary(&mut state_guard.mrecordlog, &queue_id).await;

            let solo_shard = SoloShard::new(ShardState::Closed, Position::Eof);
            let shard = IngesterShard::Solo(solo_shard);
            state_guard.shards.insert(queue_id, shard);
        }
        Ok(())
    }

    async fn create_shard<'a>(
        &self,
        state: &'a mut IngesterState,
        queue_id: &QueueId,
        leader_id: &NodeId,
        follower_id_opt: Option<&NodeId>,
    ) -> IngestV2Result<&'a IngesterShard> {
        match state.mrecordlog.create_queue(queue_id).await {
            Ok(_) => {}
            Err(CreateQueueError::AlreadyExists) => panic!("queue should not exist"),
            Err(CreateQueueError::IoError(io_error)) => {
                // TODO: Close all shards and set readiness to false.
                error!(
                    "failed to create mrecordlog queue `{}`: {}",
                    queue_id, io_error
                );
                return Err(IngestV2Error::IngesterUnavailable {
                    ingester_id: leader_id.clone(),
                });
            }
        };
        let shard = if let Some(follower_id) = follower_id_opt {
            self.init_replication_stream(state, leader_id, follower_id)
                .await?;
            let primary_shard = PrimaryShard::new(follower_id.clone());
            IngesterShard::Primary(primary_shard)
        } else {
            IngesterShard::Solo(SoloShard::default())
        };
        let entry = state.shards.entry(queue_id.clone());
        Ok(entry.or_insert(shard))
    }

    async fn init_replication_stream(
        &self,
        state: &mut IngesterState,
        leader_id: &NodeId,
        follower_id: &NodeId,
    ) -> IngestV2Result<()> {
        let Entry::Vacant(entry) = state.replication_streams.entry(follower_id.clone()) else {
            // A replication stream with this follower is already opened.
            return Ok(());
        };
        let open_request = OpenReplicationStreamRequest {
            leader_id: leader_id.clone().into(),
            follower_id: follower_id.clone().into(),
        };
        let open_message = SynReplicationMessage::new_open_request(open_request);
        let (syn_replication_stream_tx, syn_replication_stream) =
            ServiceStream::new_bounded(SYN_REPLICATION_STREAM_CAPACITY);
        syn_replication_stream_tx
            .try_send(open_message)
            .expect("channel should be open and have capacity");

        let mut ingester =
            self.ingester_pool
                .get(follower_id)
                .ok_or(IngestV2Error::IngesterUnavailable {
                    ingester_id: follower_id.clone(),
                })?;
        let mut ack_replication_stream = ingester
            .open_replication_stream(syn_replication_stream)
            .await?;
        ack_replication_stream
            .next()
            .await
            .expect("TODO")
            .expect("TODO")
            .into_open_response()
            .expect("first message should be an open response");

        let replication_stream_task_handle = ReplicationStreamTask::spawn(
            leader_id.clone(),
            follower_id.clone(),
            syn_replication_stream_tx,
            ack_replication_stream,
        );
        entry.insert(replication_stream_task_handle);
        Ok(())
    }
}

#[async_trait]
impl IngesterService for Ingester {
    async fn persist(
        &mut self,
        persist_request: PersistRequest,
    ) -> IngestV2Result<PersistResponse> {
        if persist_request.leader_id != self.self_node_id {
            return Err(IngestV2Error::Internal(format!(
                "routing error: expected leader ID `{}`, got `{}`",
                self.self_node_id, persist_request.leader_id,
            )));
        }
        let mut state_guard = self.state.write().await;

        let mut persist_successes = Vec::with_capacity(persist_request.subrequests.len());
        let mut persist_failures = Vec::new();
        let mut replicate_subrequests: HashMap<NodeId, Vec<ReplicateSubrequest>> = HashMap::new();

        let commit_type = persist_request.commit_type();
        let force_commit = commit_type == CommitTypeV2::Force;
        let leader_id: NodeId = persist_request.leader_id.into();

        for subrequest in persist_request.subrequests {
            let queue_id = subrequest.queue_id();
            let follower_id_opt: Option<NodeId> = subrequest.follower_id.map(Into::into);
            let shard = if let Some(shard) = state_guard.shards.get_mut(&queue_id) {
                shard
            } else {
                self.create_shard(
                    &mut state_guard,
                    &queue_id,
                    &leader_id,
                    follower_id_opt.as_ref(),
                )
                .await
                .expect("TODO")
            };
            let from_position_exclusive = shard.replication_position_inclusive();

            if shard.is_closed() {
                let persist_failure = PersistFailure {
                    subrequest_id: subrequest.subrequest_id,
                    index_uid: subrequest.index_uid,
                    source_id: subrequest.source_id,
                    shard_id: subrequest.shard_id,
                    reason: PersistFailureReason::ShardClosed as i32,
                };
                persist_failures.push(persist_failure);
                continue;
            }
            let doc_batch = subrequest
                .doc_batch
                .expect("router should not send empty persist subrequests");

            let current_position_inclusive: Position = if force_commit {
                let encoded_mrecords = doc_batch
                    .docs()
                    .map(|doc| MRecord::Doc(doc).encode())
                    .chain(once(MRecord::Commit.encode()));
                state_guard
                    .mrecordlog
                    .append_records(&queue_id, None, encoded_mrecords)
                    .await
                    .expect("TODO") // TODO: Io error, close shard?
            } else {
                let encoded_mrecords = doc_batch.docs().map(|doc| MRecord::Doc(doc).encode());
                state_guard
                    .mrecordlog
                    .append_records(&queue_id, None, encoded_mrecords)
                    .await
                    .expect("TODO") // TODO: Io error, close shard?
            }
            .into();
            let batch_num_bytes = doc_batch.num_bytes() as u64;
            let batch_num_docs = doc_batch.num_docs() as u64;

            INGEST_METRICS.ingested_num_bytes.inc_by(batch_num_bytes);
            INGEST_METRICS.ingested_num_docs.inc_by(batch_num_docs);

            state_guard
                .shards
                .get_mut(&queue_id)
                .expect("primary shard should exist")
                .set_replication_position_inclusive(current_position_inclusive.clone());

            if let Some(follower_id) = follower_id_opt {
                let replicate_subrequest = ReplicateSubrequest {
                    subrequest_id: subrequest.subrequest_id,
                    index_uid: subrequest.index_uid,
                    source_id: subrequest.source_id,
                    shard_id: subrequest.shard_id,
                    from_position_exclusive: Some(from_position_exclusive),
                    to_position_inclusive: Some(current_position_inclusive),
                    doc_batch: Some(doc_batch),
                };
                replicate_subrequests
                    .entry(follower_id)
                    .or_default()
                    .push(replicate_subrequest);
            } else {
                let persist_success = PersistSuccess {
                    subrequest_id: subrequest.subrequest_id,
                    index_uid: subrequest.index_uid,
                    source_id: subrequest.source_id,
                    shard_id: subrequest.shard_id,
                    replication_position_inclusive: Some(current_position_inclusive),
                };
                persist_successes.push(persist_success);
            }
        }
        if replicate_subrequests.is_empty() {
            let leader_id = self.self_node_id.to_string();
            let persist_response = PersistResponse {
                leader_id,
                successes: persist_successes,
                failures: persist_failures,
            };
            return Ok(persist_response);
        }
        let mut replicate_futures = FuturesUnordered::new();

        for (follower_id, subrequests) in replicate_subrequests {
            let replication_stream = state_guard
                .replication_streams
                .get(&follower_id)
                .expect("replication stream should be initialized");
            let replication_seqno = replication_stream.next_replication_seqno();
            let replicate_request = ReplicateRequest {
                leader_id: self.self_node_id.clone().into(),
                follower_id: follower_id.clone().into(),
                subrequests,
                commit_type: persist_request.commit_type,
                replication_seqno,
            };
            replicate_futures.push(replication_stream.replicate(replicate_request));
        }
        // Drop the write lock AFTER pushing the replicate request into the replication client
        // channel to ensure that sequential writes in mrecordlog turn into sequential replicate
        // requests in the same order.
        drop(state_guard);

        while let Some(replication_result) = replicate_futures.next().await {
            let replicate_response = match replication_result {
                Ok(replicate_response) => replicate_response,
                Err(_) => {
                    // TODO: Handle replication error:
                    // 1. Close and evict all the shards hosted by the follower.
                    // 2. Close and evict the replication client.
                    // 3. Return `PersistFailureReason::ShardClosed` to router.
                    continue;
                }
            };
            for replicate_success in replicate_response.successes {
                let persist_success = PersistSuccess {
                    subrequest_id: replicate_success.subrequest_id,
                    index_uid: replicate_success.index_uid,
                    source_id: replicate_success.source_id,
                    shard_id: replicate_success.shard_id,
                    replication_position_inclusive: replicate_success
                        .replication_position_inclusive,
                };
                persist_successes.push(persist_success);
            }
        }
        let _state_guard = self.state.write().await;

        for persist_success in &persist_successes {
            let _queue_id = persist_success.queue_id();
        }
        let leader_id = self.self_node_id.to_string();
        let persist_response = PersistResponse {
            leader_id,
            successes: persist_successes,
            failures: Vec::new(), // TODO
        };
        Ok(persist_response)
    }

    /// Opens a replication stream, which is a bi-directional gRPC stream. The client-side stream
    async fn open_replication_stream(
        &mut self,
        mut syn_replication_stream: quickwit_common::ServiceStream<SynReplicationMessage>,
    ) -> IngestV2Result<IngesterServiceStream<AckReplicationMessage>> {
        let open_replication_stream_request = syn_replication_stream
            .next()
            .await
            .ok_or_else(|| IngestV2Error::Internal("syn replication stream aborted".to_string()))?
            .into_open_request()
            .expect("first message should be an open replication stream request");

        if open_replication_stream_request.follower_id != self.self_node_id {
            return Err(IngestV2Error::Internal("routing error".to_string()));
        }
        let leader_id: NodeId = open_replication_stream_request.leader_id.into();
        let follower_id: NodeId = open_replication_stream_request.follower_id.into();

        let mut state_guard = self.state.write().await;

        let Entry::Vacant(entry) = state_guard.replication_tasks.entry(leader_id.clone()) else {
            return Err(IngestV2Error::Internal(format!(
                "a replication stream betwen {leader_id} and {follower_id} is already opened"
            )));
        };
        // Channel capacity: there is no need to bound the capacity of the channel here because it
        // is already virtually bounded by the capacity of the SYN replication stream.
        let (ack_replication_stream_tx, ack_replication_stream) = ServiceStream::new_unbounded();
        let open_response = OpenReplicationStreamResponse {};
        let ack_replication_message = AckReplicationMessage::new_open_response(open_response);
        ack_replication_stream_tx
            .send(Ok(ack_replication_message))
            .expect("channel should be open");

        let replication_task_handle = ReplicationTask::spawn(
            leader_id,
            follower_id,
            self.state.clone(),
            syn_replication_stream,
            ack_replication_stream_tx,
        );
        entry.insert(replication_task_handle);
        Ok(ack_replication_stream)
    }

    async fn open_fetch_stream(
        &mut self,
        open_fetch_stream_request: OpenFetchStreamRequest,
    ) -> IngestV2Result<ServiceStream<IngestV2Result<FetchResponseV2>>> {
        let queue_id = open_fetch_stream_request.queue_id();
        let new_records_rx = self
            .state
            .read()
            .await
            .shards
            .get(&queue_id)
            .ok_or_else(|| IngestV2Error::Internal("shard not found".to_string()))?
            .new_records_rx();
        let (service_stream, _fetch_task_handle) = FetchTask::spawn(
            open_fetch_stream_request,
            self.state.clone(),
            new_records_rx,
            FetchTask::DEFAULT_BATCH_NUM_BYTES,
        );
        Ok(service_stream)
    }

    async fn ping(&mut self, ping_request: PingRequest) -> IngestV2Result<PingResponse> {
        if ping_request.leader_id != self.self_node_id {
            let ping_response = PingResponse {};
            return Ok(ping_response);
        };
        let Some(follower_id) = &ping_request.follower_id else {
            let ping_response = PingResponse {};
            return Ok(ping_response);
        };
        let follower_id: NodeId = follower_id.clone().into();
        let mut ingester = self.ingester_pool.get(&follower_id).ok_or({
            IngestV2Error::IngesterUnavailable {
                ingester_id: follower_id,
            }
        })?;
        ingester.ping(ping_request).await?;
        let ping_response = PingResponse {};
        Ok(ping_response)
    }

    async fn truncate(
        &mut self,
        truncate_request: TruncateRequest,
    ) -> IngestV2Result<TruncateResponse> {
        if truncate_request.ingester_id != self.self_node_id {
            return Err(IngestV2Error::Internal(format!(
                "routing error: expected ingester `{}`, got `{}`",
                self.self_node_id, truncate_request.ingester_id,
            )));
        }
        let mut state_guard = self.state.write().await;

        for subrequest in truncate_request.subrequests {
            let queue_id = subrequest.queue_id();
            let to_position_inclusive = subrequest.to_position_inclusive();

            if let Some(to_offset_inclusive) = to_position_inclusive.as_u64() {
                match state_guard
                    .mrecordlog
                    .truncate(&queue_id, to_offset_inclusive)
                    .await
                {
                    Ok(_) | Err(TruncateError::MissingQueue(_)) => {}
                    Err(error) => {
                        error!("failed to truncate queue `{}`: {}", queue_id, error);
                    }
                }
            } else if to_position_inclusive == Position::Eof {
                match state_guard.mrecordlog.delete_queue(&queue_id).await {
                    Ok(_) | Err(DeleteQueueError::MissingQueue(_)) => {}
                    Err(error) => {
                        error!("failed to delete queue `{}`: {}", queue_id, error);
                    }
                }
                state_guard.shards.remove(&queue_id);
            };
        }
        let truncate_response = TruncateResponse {};
        Ok(truncate_response)
    }
}

/// Appends an EOF record to the queue if the it is empty or the last record is not an EOF
/// record.
///
/// # Panics
///
/// Panics if the queue does not exist.
async fn append_eof_record_if_necessary(mrecordlog: &mut MultiRecordLog, queue_id: &QueueId) {
    let mut should_append_eof_record = true;

    if let Some(current_position) = mrecordlog.current_position(queue_id).expect("TODO") {
        let mrecords = mrecordlog
            .range(queue_id, current_position..)
            .expect("TODO");

        if let Some((_, last_mecord)) = mrecords.last() {
            should_append_eof_record = !is_eof_mrecord(&last_mecord);
        }
    }
    if should_append_eof_record {
        mrecordlog
            .append_record(queue_id, None, MRecord::Eof.encode())
            .await
            .expect("TODO");
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use bytes::Bytes;
    use quickwit_proto::ingest::ingester::{
        IngesterServiceGrpcServer, IngesterServiceGrpcServerAdapter, PersistSubrequest,
        TruncateSubrequest,
    };
    use quickwit_proto::ingest::DocBatchV2;
    use quickwit_proto::types::queue_id;
    use tonic::transport::{Endpoint, Server};

    use super::*;
    use crate::ingest_v2::test_utils::{IngesterShardTestExt, MultiRecordLogTestExt};

    #[tokio::test]
    async fn test_ingester_init() {
        let tempdir = tempfile::tempdir().unwrap();
        let self_node_id: NodeId = "test-ingester-0".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 2;
        let mut ingester = Ingester::try_new(
            self_node_id.clone(),
            ingester_pool,
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let mut state_guard = ingester.state.write().await;

        let queue_id_01 = queue_id("test-index:0", "test-source", 1);
        state_guard
            .mrecordlog
            .create_queue(&queue_id_01)
            .await
            .unwrap();

        let records = [MRecord::new_doc("test-doc-foo").encode()].into_iter();

        state_guard
            .mrecordlog
            .append_records(&queue_id_01, None, records)
            .await
            .unwrap();

        state_guard
            .mrecordlog
            .truncate(&queue_id_01, 0)
            .await
            .unwrap();

        let queue_id_02 = queue_id("test-index:0", "test-source", 2);
        state_guard
            .mrecordlog
            .create_queue(&queue_id_02)
            .await
            .unwrap();

        let records = [MRecord::new_doc("test-doc-foo").encode()].into_iter();

        state_guard
            .mrecordlog
            .append_records(&queue_id_02, None, records)
            .await
            .unwrap();

        let queue_id_03 = queue_id("test-index:0", "test-source", 3);
        state_guard
            .mrecordlog
            .create_queue(&queue_id_03)
            .await
            .unwrap();

        drop(state_guard);

        ingester.init().await.unwrap();

        // It should only append EOF records if necessary.
        ingester.init().await.unwrap();

        let state_guard = ingester.state.read().await;
        assert_eq!(state_guard.shards.len(), 3);

        let solo_shard_01 = state_guard.shards.get(&queue_id_01).unwrap();
        solo_shard_01.assert_is_solo();
        solo_shard_01.assert_is_closed();
        solo_shard_01.assert_replication_position(Position::Eof);

        state_guard
            .mrecordlog
            .assert_records_eq(&queue_id_01, .., &[(1, "\0\x02")]);

        let solo_shard_02 = state_guard.shards.get(&queue_id_02).unwrap();
        solo_shard_02.assert_is_solo();
        solo_shard_02.assert_is_closed();
        solo_shard_02.assert_replication_position(Position::Eof);

        state_guard.mrecordlog.assert_records_eq(
            &queue_id_02,
            ..,
            &[(0, "\0\0test-doc-foo"), (1, "\0\x02")],
        );

        let solo_shard_03 = state_guard.shards.get(&queue_id_03).unwrap();
        solo_shard_03.assert_is_solo();
        solo_shard_03.assert_is_closed();
        solo_shard_03.assert_replication_position(Position::Eof);

        state_guard
            .mrecordlog
            .assert_records_eq(&queue_id_03, .., &[(0, "\0\x02")]);
    }

    #[tokio::test]
    async fn test_ingester_persist() {
        let tempdir = tempfile::tempdir().unwrap();
        let self_node_id: NodeId = "test-ingester-0".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 1;
        let mut ingester = Ingester::try_new(
            self_node_id.clone(),
            ingester_pool,
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let persist_request = PersistRequest {
            leader_id: self_node_id.to_string(),
            commit_type: CommitTypeV2::Force as i32,
            subrequests: vec![
                PersistSubrequest {
                    subrequest_id: 0,
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: None,
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-010"])),
                },
                PersistSubrequest {
                    subrequest_id: 1,
                    index_uid: "test-index:1".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: None,
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-110", "test-doc-111"])),
                },
            ],
        };
        ingester.persist(persist_request).await.unwrap();

        let state_guard = ingester.state.read().await;
        assert_eq!(state_guard.shards.len(), 2);

        let queue_id_01 = queue_id("test-index:0", "test-source", 1);
        let solo_shard_01 = state_guard.shards.get(&queue_id_01).unwrap();
        solo_shard_01.assert_is_solo();
        solo_shard_01.assert_is_open();
        solo_shard_01.assert_replication_position(1u64);

        state_guard.mrecordlog.assert_records_eq(
            &queue_id_01,
            ..,
            &[(0, "\0\0test-doc-010"), (1, "\0\x01")],
        );

        let queue_id_11 = queue_id("test-index:1", "test-source", 1);
        let solo_shard_11 = state_guard.shards.get(&queue_id_11).unwrap();
        solo_shard_11.assert_is_solo();
        solo_shard_11.assert_is_open();
        solo_shard_11.assert_replication_position(2u64);

        state_guard.mrecordlog.assert_records_eq(
            &queue_id_11,
            ..,
            &[
                (0, "\0\0test-doc-110"),
                (1, "\0\0test-doc-111"),
                (2, "\0\x01"),
            ],
        );
    }

    #[tokio::test]
    async fn test_ingester_open_replication_stream() {
        let tempdir = tempfile::tempdir().unwrap();
        let self_node_id: NodeId = "test-follower".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 1;
        let mut ingester = Ingester::try_new(
            self_node_id.clone(),
            ingester_pool,
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();
        let (syn_replication_stream_tx, syn_replication_stream) = ServiceStream::new_bounded(5);
        let open_stream_request = OpenReplicationStreamRequest {
            leader_id: "test-leader".to_string(),
            follower_id: "test-follower".to_string(),
        };
        let syn_replication_message = SynReplicationMessage::new_open_request(open_stream_request);
        syn_replication_stream_tx
            .send(syn_replication_message)
            .await
            .unwrap();
        let mut ack_replication_stream = ingester
            .open_replication_stream(syn_replication_stream)
            .await
            .unwrap();
        ack_replication_stream
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_open_response()
            .unwrap();

        let state_guard = ingester.state.read().await;
        assert!(state_guard.replication_tasks.contains_key("test-leader"));
    }

    #[tokio::test]
    async fn test_ingester_persist_replicate() {
        let tempdir = tempfile::tempdir().unwrap();
        let leader_id: NodeId = "test-leader".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 2;
        let mut leader = Ingester::try_new(
            leader_id.clone(),
            ingester_pool.clone(),
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let tempdir = tempfile::tempdir().unwrap();
        let follower_id: NodeId = "test-follower".into();
        let wal_dir_path = tempdir.path();
        let replication_factor = 2;
        let follower = Ingester::try_new(
            follower_id.clone(),
            ingester_pool.clone(),
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        ingester_pool.insert(
            follower_id.clone(),
            IngesterServiceClient::new(follower.clone()),
        );

        let persist_request = PersistRequest {
            leader_id: "test-leader".to_string(),
            commit_type: CommitTypeV2::Force as i32,
            subrequests: vec![
                PersistSubrequest {
                    subrequest_id: 0,
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: Some(follower_id.to_string()),
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-010"])),
                },
                PersistSubrequest {
                    subrequest_id: 1,
                    index_uid: "test-index:1".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: Some(follower_id.to_string()),
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-110", "test-doc-111"])),
                },
            ],
        };
        let persist_response = leader.persist(persist_request).await.unwrap();
        assert_eq!(persist_response.leader_id, "test-leader");
        assert_eq!(persist_response.successes.len(), 2);
        assert_eq!(persist_response.failures.len(), 0);

        let leader_state_guard = leader.state.read().await;
        assert_eq!(leader_state_guard.shards.len(), 2);

        let queue_id_01 = queue_id("test-index:0", "test-source", 1);
        let primary_shard_01 = leader_state_guard.shards.get(&queue_id_01).unwrap();
        primary_shard_01.assert_is_primary();
        primary_shard_01.assert_is_open();
        primary_shard_01.assert_replication_position(1u64);

        leader_state_guard.mrecordlog.assert_records_eq(
            &queue_id_01,
            ..,
            &[(0, "\0\0test-doc-010"), (1, "\0\x01")],
        );

        let queue_id_11 = queue_id("test-index:1", "test-source", 1);
        let primary_shard_11 = leader_state_guard.shards.get(&queue_id_11).unwrap();
        primary_shard_11.assert_is_primary();
        primary_shard_11.assert_is_open();
        primary_shard_11.assert_replication_position(2u64);

        leader_state_guard.mrecordlog.assert_records_eq(
            &queue_id_11,
            ..,
            &[
                (0, "\0\0test-doc-110"),
                (1, "\0\0test-doc-111"),
                (2, "\0\x01"),
            ],
        );

        let follower_state_guard = follower.state.read().await;
        assert_eq!(follower_state_guard.shards.len(), 2);

        let replica_shard_01 = follower_state_guard.shards.get(&queue_id_01).unwrap();
        replica_shard_01.assert_is_replica();
        replica_shard_01.assert_is_open();
        replica_shard_01.assert_replication_position(1u64);

        follower_state_guard.mrecordlog.assert_records_eq(
            &queue_id_01,
            ..,
            &[(0, "\0\0test-doc-010"), (1, "\0\x01")],
        );

        let replica_shard_11 = follower_state_guard.shards.get(&queue_id_11).unwrap();
        replica_shard_11.assert_is_replica();
        replica_shard_11.assert_is_open();
        replica_shard_11.assert_replication_position(2u64);

        follower_state_guard.mrecordlog.assert_records_eq(
            &queue_id_11,
            ..,
            &[
                (0, "\0\0test-doc-110"),
                (1, "\0\0test-doc-111"),
                (2, "\0\x01"),
            ],
        );
    }

    #[tokio::test]
    async fn test_ingester_persist_replicate_grpc() {
        let tempdir = tempfile::tempdir().unwrap();
        let leader_id: NodeId = "test-leader".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 2;
        let mut leader = Ingester::try_new(
            leader_id.clone(),
            ingester_pool.clone(),
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let leader_grpc_server_adapter = IngesterServiceGrpcServerAdapter::new(leader.clone());
        let leader_grpc_server = IngesterServiceGrpcServer::new(leader_grpc_server_adapter);
        let leader_socket_addr: SocketAddr = "127.0.0.1:6666".parse().unwrap();

        tokio::spawn({
            async move {
                Server::builder()
                    .add_service(leader_grpc_server)
                    .serve(leader_socket_addr)
                    .await
                    .unwrap();
            }
        });

        let tempdir = tempfile::tempdir().unwrap();
        let follower_id: NodeId = "test-follower".into();
        let wal_dir_path = tempdir.path();
        let replication_factor = 2;
        let follower = Ingester::try_new(
            follower_id.clone(),
            ingester_pool.clone(),
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let follower_grpc_server_adapter = IngesterServiceGrpcServerAdapter::new(follower.clone());
        let follower_grpc_server = IngesterServiceGrpcServer::new(follower_grpc_server_adapter);
        let follower_socket_addr: SocketAddr = "127.0.0.1:7777".parse().unwrap();

        tokio::spawn({
            async move {
                Server::builder()
                    .add_service(follower_grpc_server)
                    .serve(follower_socket_addr)
                    .await
                    .unwrap();
            }
        });
        let follower_channel = Endpoint::from_static("http://127.0.0.1:7777").connect_lazy();
        let follower_grpc_client = IngesterServiceClient::from_channel(
            "127.0.0.1:7777".parse().unwrap(),
            follower_channel,
        );

        ingester_pool.insert(follower_id.clone(), follower_grpc_client);

        let persist_request = PersistRequest {
            leader_id: "test-leader".to_string(),
            commit_type: CommitTypeV2::Auto as i32,
            subrequests: vec![
                PersistSubrequest {
                    subrequest_id: 0,
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: Some(follower_id.to_string()),
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-010"])),
                },
                PersistSubrequest {
                    subrequest_id: 1,
                    index_uid: "test-index:1".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: Some(follower_id.to_string()),
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-110", "test-doc-111"])),
                },
            ],
        };
        let persist_response = leader.persist(persist_request).await.unwrap();
        assert_eq!(persist_response.leader_id, "test-leader");
        assert_eq!(persist_response.successes.len(), 2);
        assert_eq!(persist_response.failures.len(), 0);

        let leader_state_guard = leader.state.read().await;
        assert_eq!(leader_state_guard.shards.len(), 2);

        let queue_id_01 = queue_id("test-index:0", "test-source", 1);
        let primary_shard_01 = leader_state_guard.shards.get(&queue_id_01).unwrap();
        primary_shard_01.assert_is_primary();
        primary_shard_01.assert_is_open();
        primary_shard_01.assert_replication_position(0u64);

        leader_state_guard.mrecordlog.assert_records_eq(
            &queue_id_01,
            ..,
            &[(0, "\0\0test-doc-010")],
        );

        let queue_id_11 = queue_id("test-index:1", "test-source", 1);
        let primary_shard_11 = leader_state_guard.shards.get(&queue_id_11).unwrap();
        primary_shard_11.assert_is_primary();
        primary_shard_11.assert_is_open();
        primary_shard_11.assert_replication_position(1u64);

        leader_state_guard.mrecordlog.assert_records_eq(
            &queue_id_11,
            ..,
            &[(0, "\0\0test-doc-110"), (1, "\0\0test-doc-111")],
        );

        let follower_state_guard = follower.state.read().await;
        assert_eq!(follower_state_guard.shards.len(), 2);

        let replica_shard_01 = follower_state_guard.shards.get(&queue_id_01).unwrap();
        replica_shard_01.assert_is_replica();
        replica_shard_01.assert_is_open();
        replica_shard_01.assert_replication_position(0u64);

        follower_state_guard.mrecordlog.assert_records_eq(
            &queue_id_01,
            ..,
            &[(0, "\0\0test-doc-010")],
        );

        let replica_shard_11 = follower_state_guard.shards.get(&queue_id_11).unwrap();
        replica_shard_11.assert_is_replica();
        replica_shard_11.assert_is_open();
        replica_shard_11.assert_replication_position(1u64);

        follower_state_guard.mrecordlog.assert_records_eq(
            &queue_id_11,
            ..,
            &[(0, "\0\0test-doc-110"), (1, "\0\0test-doc-111")],
        );
    }

    #[tokio::test]
    async fn test_ingester_open_fetch_stream() {
        let tempdir = tempfile::tempdir().unwrap();
        let self_node_id: NodeId = "test-ingester-0".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 1;
        let mut ingester = Ingester::try_new(
            self_node_id.clone(),
            ingester_pool,
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let persist_request = PersistRequest {
            leader_id: self_node_id.to_string(),
            commit_type: CommitTypeV2::Auto as i32,
            subrequests: vec![
                PersistSubrequest {
                    subrequest_id: 0,
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    follower_id: None,
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-010"])),
                },
                PersistSubrequest {
                    subrequest_id: 1,
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 2,
                    follower_id: None,
                    doc_batch: Some(DocBatchV2::for_test(["test-doc-020"])),
                },
            ],
        };
        ingester.persist(persist_request).await.unwrap();

        let client_id = "test-client".to_string();

        let open_fetch_stream_request = OpenFetchStreamRequest {
            client_id: client_id.clone(),
            index_uid: "test-index:0".to_string(),
            source_id: "test-source".to_string(),
            shard_id: 1,
            from_position_exclusive: None,
            to_position_inclusive: None,
        };
        let mut fetch_stream = ingester
            .open_fetch_stream(open_fetch_stream_request)
            .await
            .unwrap();

        let fetch_response = fetch_stream.next().await.unwrap().unwrap();
        assert_eq!(
            fetch_response.from_position_exclusive(),
            Position::Beginning
        );
        assert_eq!(fetch_response.to_position_inclusive(), Position::from(0u64));

        let mrecord_batch = fetch_response.mrecord_batch.unwrap();
        assert_eq!(
            mrecord_batch.mrecord_buffer,
            Bytes::from_static(b"\0\0test-doc-010")
        );
        assert_eq!(mrecord_batch.mrecord_lengths, [14]);

        let persist_request = PersistRequest {
            leader_id: self_node_id.to_string(),
            commit_type: CommitTypeV2::Auto as i32,
            subrequests: vec![PersistSubrequest {
                subrequest_id: 0,
                index_uid: "test-index:0".to_string(),
                source_id: "test-source".to_string(),
                shard_id: 1,
                follower_id: None,
                doc_batch: Some(DocBatchV2::for_test(["test-doc-011", "test-doc-012"])),
            }],
        };
        ingester.persist(persist_request).await.unwrap();

        let fetch_response = fetch_stream.next().await.unwrap().unwrap();
        assert_eq!(
            fetch_response.from_position_exclusive(),
            Position::from(0u64)
        );
        assert_eq!(fetch_response.to_position_inclusive(), Position::from(2u64));

        let mrecord_batch = fetch_response.mrecord_batch.unwrap();
        assert_eq!(
            mrecord_batch.mrecord_buffer,
            Bytes::from_static(b"\0\0test-doc-011\0\0test-doc-012")
        );
        assert_eq!(mrecord_batch.mrecord_lengths, [14, 14]);
    }

    #[tokio::test]
    async fn test_ingester_truncate() {
        let tempdir = tempfile::tempdir().unwrap();
        let self_node_id: NodeId = "test-ingester-0".into();
        let ingester_pool = IngesterPool::default();
        let wal_dir_path = tempdir.path();
        let replication_factor = 1;
        let mut ingester = Ingester::try_new(
            self_node_id.clone(),
            ingester_pool,
            wal_dir_path,
            replication_factor,
        )
        .await
        .unwrap();

        let queue_id_01 = queue_id("test-index:0", "test-source", 1);
        let queue_id_02 = queue_id("test-index:0", "test-source", 2);

        let mut state_guard = ingester.state.write().await;
        ingester
            .create_shard(&mut state_guard, &queue_id_01, &self_node_id, None)
            .await
            .unwrap();
        ingester
            .create_shard(&mut state_guard, &queue_id_02, &self_node_id, None)
            .await
            .unwrap();

        let records = [
            MRecord::new_doc("test-doc-010").encode(),
            MRecord::new_doc("test-doc-011").encode(),
        ]
        .into_iter();

        state_guard
            .mrecordlog
            .append_records(&queue_id_01, None, records)
            .await
            .unwrap();

        let records = [MRecord::new_doc("test-doc-020").encode()].into_iter();

        state_guard
            .mrecordlog
            .append_records(&queue_id_02, None, records)
            .await
            .unwrap();

        drop(state_guard);

        let truncate_request = TruncateRequest {
            ingester_id: self_node_id.to_string(),
            subrequests: vec![
                TruncateSubrequest {
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    to_position_inclusive: Some(Position::from(0u64)),
                },
                TruncateSubrequest {
                    index_uid: "test-index:0".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 2,
                    to_position_inclusive: Some(Position::Eof),
                },
                TruncateSubrequest {
                    index_uid: "test-index:1337".to_string(),
                    source_id: "test-source".to_string(),
                    shard_id: 1,
                    to_position_inclusive: Some(Position::from(1337u64)),
                },
            ],
        };
        ingester.truncate(truncate_request).await.unwrap();

        let state_guard = ingester.state.read().await;
        assert_eq!(state_guard.shards.len(), 1);
        assert!(state_guard.shards.contains_key(&queue_id_01));

        state_guard
            .mrecordlog
            .assert_records_eq(&queue_id_01, .., &[(1, "\0\0test-doc-011")]);
        assert!(!state_guard.shards.contains_key(&queue_id_02));
    }
}
