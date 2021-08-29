use tracing::Instrument;

use crate::core::RaftCore;
use crate::core::State;
use crate::core::UpdateCurrentLeader;
use crate::error::RaftResult;
use crate::raft::AppendEntriesRequest;
use crate::raft::AppendEntriesResponse;
use crate::raft::ConflictOpt;
use crate::raft::Entry;
use crate::raft::EntryPayload;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::MessageSummary;
use crate::RaftError;
use crate::RaftNetwork;
use crate::RaftStorage;
use crate::Update;

impl<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> RaftCore<D, R, N, S> {
    /// An RPC invoked by the leader to replicate log entries (§5.3); also used as heartbeat (§5.2).
    ///
    /// See `receiver implementation: AppendEntries RPC` in raft-essentials.md in this repo.
    #[tracing::instrument(level="trace", skip(self, msg), fields(msg=%msg.summary()))]
    pub(super) async fn handle_append_entries_request(
        &mut self,
        msg: AppendEntriesRequest<D>,
    ) -> RaftResult<AppendEntriesResponse> {
        // If message's term is less than most recent term, then we do not honor the request.
        if msg.term < self.current_term {
            tracing::debug!({self.current_term, rpc_term=msg.term}, "AppendEntries RPC term is less than current term");
            return Ok(AppendEntriesResponse {
                term: self.current_term,
                success: false,
                conflict_opt: None,
            });
        }

        // Update election timeout.
        // TODO(xp): only update commit_index if the log present. e.g., append entries first, then update commit_index.
        self.update_next_election_timeout(true);
        let mut report_metrics = false;

        // The value for `self.commit_index` is only updated here when not the leader.
        self.commit_index = msg.leader_commit;

        // Update current term if needed.
        if self.current_term != msg.term {
            self.update_current_term(msg.term, None);
            self.save_hard_state().await?;
            report_metrics = true;
        }

        // Update current leader if needed.
        if self.current_leader.as_ref() != Some(&msg.leader_id) {
            self.update_current_leader(UpdateCurrentLeader::OtherNode(msg.leader_id));
            report_metrics = true;
        }

        // Transition to follower state if needed.
        if !self.target_state.is_follower() && !self.target_state.is_non_voter() {
            self.set_target_state(State::Follower);
        }

        // If RPC's `prev_log_index` is 0, or the RPC's previous log info matches the local
        // log info, then replication is g2g.
        let msg_prev_index_is_min = msg.prev_log_id.index == u64::MIN;
        let msg_index_and_term_match = msg.prev_log_id == self.last_log_id;

        if msg_prev_index_is_min || msg_index_and_term_match {
            if !msg.entries.is_empty() {
                self.append_log_entries(&msg.entries).await?;
            }
            self.replicate_to_state_machine_if_needed().await?;

            if report_metrics {
                self.report_metrics(Update::Ignore);
            }

            return Ok(AppendEntriesResponse {
                term: self.current_term,
                success: true,
                conflict_opt: None,
            });
        }

        /////////////////////////////////////
        //// Begin Log Consistency Check ////
        tracing::debug!("begin log consistency check");

        if self.last_log_id.index < msg.prev_log_id.index {
            if report_metrics {
                self.report_metrics(Update::Ignore);
            }

            return Ok(AppendEntriesResponse {
                term: self.current_term,
                success: false,
                conflict_opt: Some(ConflictOpt {
                    log_id: self.last_log_id,
                }),
            });
        }

        // last_log_id.index >= prev_log_id.index

        // Previous log info doesn't immediately line up, so perform log consistency check and proceed based on its
        // result.
        let prev_entry = self
            .storage
            .try_get_log_entry(msg.prev_log_id.index)
            .await
            .map_err(|err| self.map_fatal_storage_error(err))?;

        let target_entry = match prev_entry {
            Some(target_entry) => target_entry,
            None => {
                // This can only happen if the target entry is removed, e.g., when installing snapshot or log
                // compaction.
                // Use the last known index & term as a conflict opt.

                if report_metrics {
                    self.report_metrics(Update::Ignore);
                }

                return Ok(AppendEntriesResponse {
                    term: self.current_term,
                    success: false,
                    conflict_opt: Some(ConflictOpt {
                        log_id: self.last_log_id,
                    }),
                });
            }
        };

        // The target entry was found. Compare its term with target term to ensure everything is consistent.
        if target_entry.log_id.term == msg.prev_log_id.term {
            // We've found a point of agreement with the leader. If we have any logs present
            // with an index greater than this, then we must delete them per §5.3.
            if self.last_log_id.index > target_entry.log_id.index {
                self.storage
                    .delete_logs_from(target_entry.log_id.index + 1..)
                    .await
                    .map_err(|err| self.map_fatal_storage_error(err))?;
                let membership =
                    self.storage.get_membership_config().await.map_err(|err| self.map_fatal_storage_error(err))?;
                self.update_membership(membership)?;
            }
        }
        // The target entry does not have the same term. Fetch the last 50 logs, and use the last
        // entry of that payload which is still in the target term for conflict optimization.
        else {
            let start = if msg.prev_log_id.index >= 50 {
                msg.prev_log_id.index - 50
            } else {
                0
            };
            let old_entries = self
                .storage
                .get_log_entries(start..msg.prev_log_id.index)
                .await
                .map_err(|err| self.map_fatal_storage_error(err))?;
            let opt = match old_entries.iter().find(|entry| entry.log_id.term == msg.prev_log_id.term) {
                Some(entry) => Some(ConflictOpt { log_id: entry.log_id }),
                None => Some(ConflictOpt {
                    log_id: self.last_log_id,
                }),
            };
            if report_metrics {
                self.report_metrics(Update::Ignore);
            }
            return Ok(AppendEntriesResponse {
                term: self.current_term,
                success: false,
                conflict_opt: opt,
            });
        }

        ///////////////////////////////////
        //// End Log Consistency Check ////
        tracing::debug!("end log consistency check");

        self.append_log_entries(&msg.entries).await?;
        self.replicate_to_state_machine_if_needed().await?;
        if report_metrics {
            self.report_metrics(Update::Ignore);
        }
        Ok(AppendEntriesResponse {
            term: self.current_term,
            success: true,
            conflict_opt: None,
        })
    }

    /// Append the given entries to the log.
    ///
    /// Configuration changes are also detected and applied here. See `configuration changes`
    /// in the raft-essentials.md in this repo.
    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_log_entries(&mut self, entries: &[Entry<D>]) -> RaftResult<()> {
        // Check the given entries for any config changes and take the most recent.
        let last_conf_change = entries
            .iter()
            .filter_map(|ent| match &ent.payload {
                EntryPayload::ConfigChange(conf) => Some(conf),
                _ => None,
            })
            .last();
        if let Some(conf) = last_conf_change {
            tracing::debug!({membership=?conf}, "applying new membership config received from leader");
            self.update_membership(conf.membership.clone())?;
        };

        // Replicate entries to log (same as append, but in follower mode).
        let entry_refs = entries.iter().collect::<Vec<_>>();
        self.storage.append_to_log(&entry_refs).await.map_err(|err| self.map_fatal_storage_error(err))?;
        if let Some(entry) = entries.last() {
            self.last_log_id = entry.log_id;
        }
        Ok(())
    }

    /// Replicate any outstanding entries to the state machine for which it is safe to do so.
    ///
    /// Very importantly, this routine must not block the main control loop main task, else it
    /// may cause the Raft leader to timeout the requests to this node.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn replicate_to_state_machine_if_needed(&mut self) -> Result<(), RaftError> {
        tracing::debug!("replicate_to_sm_if_needed: last_applied: {}", self.last_applied,);

        // Perform initial replication to state machine if needed.
        if !self.has_completed_initial_replication_to_sm {
            // Optimistic update, as failures will cause shutdown.
            self.has_completed_initial_replication_to_sm = true;
            self.initial_replicate_to_state_machine().await;
            return Ok(());
        }

        // If we already have an active replication task, then do nothing.
        if !self.replicate_to_sm_handle.is_empty() {
            tracing::debug!("replicate_to_sm_handle is not empty, return");
            return Ok(());
        }

        // If we don't have any new entries to replicate, then do nothing.
        if self.commit_index <= self.last_applied.index {
            tracing::debug!(
                "commit_index({}) <= last_applied({}), return",
                self.commit_index,
                self.last_applied
            );
            return Ok(());
        }

        // Drain entries from the beginning of the cache up to commit index.

        // TODO(xp): logs in storage must be consecutive.
        let entries = self
            .storage
            .get_log_entries(self.last_applied.index + 1..=self.commit_index)
            .await
            .map_err(|e| self.map_fatal_storage_error(e))?;

        let last_log_id = entries.last().map(|x| x.log_id);

        tracing::debug!("entries: {:?}", entries.iter().map(|x| x.log_id).collect::<Vec<_>>());
        tracing::debug!(?last_log_id);

        // If we have no data entries to apply, then do nothing.
        if entries.is_empty() {
            if let Some(log_id) = last_log_id {
                self.last_applied = log_id;
                self.report_metrics(Update::Ignore);
            }
            tracing::debug!("entries is empty, return");
            return Ok(());
        }

        // Spawn task to replicate these entries to the state machine.
        // Linearizability is guaranteed by `replicate_to_sm_handle`, which is the mechanism used
        // to ensure that only a single task can replicate data to the state machine, and that is
        // owned by a single task, not shared between multiple threads/tasks.
        let storage = self.storage.clone();
        let handle = tokio::spawn(
            async move {
                // Create a new vector of references to the entries data ... might have to change this
                // interface a bit before 1.0.
                let entries_refs: Vec<_> = entries.iter().collect();
                storage.apply_to_state_machine(&entries_refs).await?;
                Ok(last_log_id)
            }
            .instrument(tracing::debug_span!("spawn")),
        );
        self.replicate_to_sm_handle.push(handle);

        Ok(())
    }

    /// Perform an initial replication of outstanding entries to the state machine.
    ///
    /// This will only be executed once, and only in response to its first payload of entries
    /// from the AppendEntries RPC handler.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn initial_replicate_to_state_machine(&mut self) {
        let stop = std::cmp::min(self.commit_index, self.last_log_id.index) + 1;
        let start = self.last_applied.index + 1;
        let storage = self.storage.clone();

        // If we already have an active replication task, then do nothing.
        if !self.replicate_to_sm_handle.is_empty() {
            return;
        }

        assert!(start <= stop);
        if start == stop {
            return;
        }

        // Fetch the series of entries which must be applied to the state machine, then apply them.
        let handle = tokio::spawn(
            async move {
                let mut new_last_applied: Option<LogId> = None;
                let entries = storage.get_log_entries(start..stop).await?;
                if let Some(entry) = entries.last() {
                    new_last_applied = Some(entry.log_id);
                }
                let data_entries: Vec<_> = entries.iter().collect();
                if data_entries.is_empty() {
                    return Ok(new_last_applied);
                }
                storage.apply_to_state_machine(&data_entries).await?;
                Ok(new_last_applied)
            }
            .instrument(tracing::debug_span!("spawn-init-replicate-to-sm")),
        );
        self.replicate_to_sm_handle.push(handle);
    }
}
