use actix::prelude::*;

use crate::{
    AppError,
    admin::{ProposeConfigChange, ProposeConfigChangeError},
    messages::{ClientPayload, ClientPayloadResponse, MembershipConfig},
    network::RaftNetwork,
    raft::{ConsensusState, RaftState, Raft, ReplicationState},
    replication::{ReplicationStream},
    storage::RaftStorage,
};

impl<E: AppError, N: RaftNetwork<E>, S: RaftStorage<E>> Handler<ProposeConfigChange<E>> for Raft<E, N, S> {
    type Result = ResponseActFuture<Self, (), ProposeConfigChangeError<E>>;

    /// An admin message handler invoked to trigger dynamic cluster configuration changes. See §6.
    fn handle(&mut self, msg: ProposeConfigChange<E>, ctx: &mut Self::Context) -> Self::Result {
        // Ensure the node is currently the cluster leader.
        let leader_state = match &mut self.state {
            RaftState::Leader(state) => state,
            _ => return Box::new(fut::err(ProposeConfigChangeError::NodeNotLeader(self.current_leader.clone()))),
        };

        // Normalize the proposed config to ensure everything is valid.
        let msg = match normalize_proposed_config(msg, &self.membership) {
            Ok(msg) => msg,
            Err(err) => return Box::new(fut::err(err)),
        };

        // Update consensus state, for use in finalizing joint consensus.
        match &mut leader_state.consensus_state {
            // Merge with any current consensus state.
            ConsensusState::Joint{new_nodes, is_committed} => {
                new_nodes.extend_from_slice(msg.add_members.as_slice());
                *is_committed = false;
            }
            _ => {
                leader_state.consensus_state = ConsensusState::Joint{new_nodes: msg.add_members.clone(), is_committed: false};
            }
        }

        // Add new nodes to current connfig.
        self.membership.non_voters.extend_from_slice(msg.add_members.as_slice());

        // Register nodes being removed.
        self.membership.removing.extend_from_slice(msg.remove_members.as_slice());

        // Spawn new replication streams for new members. Track state as non voters so that they
        // can be updated to be normal members once all non-voters have been brought up-to-date.
        for target in msg.add_members {
            // Build the replication stream for the target member.
            let rs = ReplicationStream::new(
                self.id, target, self.current_term, self.config.clone(),
                self.last_log_index, self.last_log_term, self.commit_index,
                ctx.address(), self.network.clone(), self.storage.clone().recipient(),
            );
            let addr = rs.start(); // Start the actor on the same thread.

            // Retain the addr of the replication stream.
            let state = ReplicationState{
                addr, match_index: self.last_log_index, remove_after_commit: None,
                is_at_line_rate: true, // Line rate is always initialize to true.
            };
            leader_state.nodes.insert(target, state);
        }

        // For any nodes being removed which are currently non-voters, immediately remove them.
        for node in msg.remove_members {
            if let Some((idx, _)) = self.membership.non_voters.iter().enumerate().find(|(_, e)| *e == &node) {
                leader_state.nodes.remove(&node); // Dropping the replication stream's addr will kill it.
                self.membership.non_voters.remove(idx);
            }
        }

        // Report metrics.
        self.report_metrics(ctx);

        // Propose the config change to cluster.
        Box::new(fut::wrap_future(ctx.address().send(ClientPayload::new_config(self.membership.clone())))
            .map_err(|_, _: &mut Self, _| ProposeConfigChangeError::Internal)
            .and_then(|res, _, _| fut::result(res.map_err(|err| ProposeConfigChangeError::ClientError(err))))
            .and_then(|res, act, ctx| act.handle_newly_committed_cluster_config(ctx, res))
        )
    }
}

impl<E: AppError, N: RaftNetwork<E>, S: RaftStorage<E>> Raft<E, N, S> {
    /// Handle response from a newly committed cluster config.
    pub(super) fn handle_newly_committed_cluster_config(&mut self, ctx: &mut Context<Self>, _: ClientPayloadResponse) -> impl ActorFuture<Actor=Self, Item=(), Error=ProposeConfigChangeError<E>> {
        let leader_state = match &mut self.state {
            RaftState::Leader(state) => state,
            _ => return fut::ok(()),
        };

        match &mut leader_state.consensus_state {
            ConsensusState::Joint{is_committed, new_nodes} => {
                *is_committed = true;
                if new_nodes.len() == 0 {
                    self.finalize_joint_consensus(ctx);
                }
            }
            _ => (),
        }

        fut::ok(())
    }

    /// Transition the cluster out of a joint consensus state.
    ///
    /// NOTE: this routine will only behave as intended when in leader state & the current
    /// membership config is in a joint consensus state.
    pub(super) fn finalize_joint_consensus(&mut self, ctx: &mut Context<Self>) {
        // It is only safe to call this routine as leader & when in a joint consensus state.
        match &mut self.state {
            RaftState::Leader(state) => match &state.consensus_state {
                ConsensusState::Joint{..} => (),
                _ => return,
            }
            _ => return,
        }

        // Update current config to prepare for exiting joint consensus.
        for node in self.membership.non_voters.drain(..) {
            self.membership.members.push(node);
        }
        for node in self.membership.removing.drain(..) {
            if let Some((idx, _)) = self.membership.members.iter().enumerate().find(|(_, e)| *e == &node) {
                self.membership.members.remove(idx);
            }
        }
        self.membership.is_in_joint_consensus = false;

        // Committ new config to cluster.
        //
        // We monitor for a response here, as we need to check if the leader node which committed
        // the subject config is no longer present in the config after it has been committed. In
        // such a case, the node will revert to NonVoter state, and wait for the parent
        // application to shutdown. Errors will only take place if the storage engine returns an
        // error, in which case the node will terminate, or if the node has transitioned out of
        // leadership state, in which case, another node will pick up the responsibility of
        // committing the updated config.
        ctx.spawn(fut::wrap_future(ctx.address().send(ClientPayload::new_config(self.membership.clone())))
            .map_err(|_, _, _| ())
            .and_then(|res, _, _| fut::result(res.map_err(|_| ())))
            .and_then(|res, act: &mut Self, ctx| act.handle_joint_consensus_finalization(ctx, res))
        );
    }

    pub(super) fn handle_joint_consensus_finalization(&mut self, ctx: &mut Context<Self>, res: ClientPayloadResponse) -> impl ActorFuture<Actor=Self, Item=(), Error=()> {
        // It is only safe to call this routine as leader & when in a uniform consensus state.
        let leader_state = match &mut self.state {
            RaftState::Leader(state) => match &state.consensus_state {
                ConsensusState::Uniform => state,
                _ => return fut::ok(()),
            }
            _ => return fut::ok(()),
        };

        // Step down if needed.
        if !self.membership.contains(&self.id) {
            self.become_non_voter(ctx);
            return fut::ok(());
        }

        // Remove any replication streams which have replicated this config & which are no longer
        // cluster members. All other replication streams which are no longer cluster members, but
        // which have not yet replicated this config will be marked for removal.
        let membership = &self.membership;
        let nodes_to_remove: Vec<_> = leader_state.nodes.iter_mut()
            .filter(|(id, _)| !membership.contains(id))
            .filter_map(|(idx, replstate)| {
                if replstate.match_index >= res.index {
                    Some(idx.clone())
                } else {
                    replstate.remove_after_commit = Some(res.index);
                    None
                }
            }).collect();
        for node in nodes_to_remove {
            leader_state.nodes.remove(&node);
        }

        fut::ok(())
    }
}

/// Check the proposed config changes with the current config to ensure changes are valid.
///
/// See the documentation on on `ProposeConfigChangeError` for the conditions which will cause
/// errors to be returned.
fn normalize_proposed_config<E: AppError>(mut msg: ProposeConfigChange<E>, current: &MembershipConfig) -> Result<ProposeConfigChange<E>, ProposeConfigChangeError<E>> {
    // Ensure no duplicates in adding new nodes & ensure the new
    // node is not also be requested for removal.
    let mut new_nodes = vec![];
    for node in msg.add_members {
        if !current.contains(&node) && !msg.remove_members.contains(&node) {
            new_nodes.push(node);
        }
    }

    // Ensure targets to remove exist in current config.
    let mut remove_nodes = vec![];
    for node in msg.remove_members {
        if current.contains(&node) && !current.removing.contains(&node) {
            remove_nodes.push(node);
        }
    }

    // Account for noop.
    if (new_nodes.len() == 0) && (remove_nodes.len() == 0) {
        return Err(ProposeConfigChangeError::Noop);
    }

    // Ensure cluster will have at least two nodes.
    let total_removing = current.removing.len() + remove_nodes.len();
    let count = current.members.len() + current.non_voters.len() + new_nodes.len();
    if total_removing >= count {
        return Err(ProposeConfigChangeError::InoperableConfig);
    } else if (count - total_removing) < 2 {
        return Err(ProposeConfigChangeError::InoperableConfig);
    }

    msg.add_members = new_nodes;
    msg.remove_members = remove_nodes;
    Ok(msg)
}
