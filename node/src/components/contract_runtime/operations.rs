use std::{collections::VecDeque, sync::Arc, time::Instant};

use itertools::Itertools;
use tokio::task;
use tracing::{debug, error, trace};

use casper_execution_engine::{
    core::engine_state::{
        self, EngineState, ExecuteRequest, ExecutionResult as EngineExecutionResult,
        ExecutionResults,
    },
    shared::{additive_map::AdditiveMap, newtypes::CorrelationId, transform::Transform},
    storage::global_state::{lmdb::LmdbGlobalState, CommitResult},
};
use casper_types::{ExecutionResult, Key};

use super::ContractRuntimeMetrics;
use crate::{crypto::hash::Digest, types::DeployHash};

/// Commits the execution effects.
pub(super) async fn commit_execution_effects(
    engine_state: Arc<EngineState<LmdbGlobalState>>,
    metrics: Arc<ContractRuntimeMetrics>,
    state_root_hash: Digest,
    deploy_hash: DeployHash,
    execution_results: ExecutionResults,
) -> Result<(Digest, ExecutionResult), ()> {
    let ee_execution_result = execution_results
        .into_iter()
        .exactly_one()
        .expect("should only be one exec result");
    let execution_result = ExecutionResult::from(&ee_execution_result);

    let execution_effect = match ee_execution_result {
        EngineExecutionResult::Success { effect, cost, .. } => {
            // We do want to see the deploy hash and cost in the logs.
            // We don't need to see the effects in the logs.
            debug!(?deploy_hash, %cost, "execution succeeded");
            effect
        }
        EngineExecutionResult::Failure {
            error,
            effect,
            cost,
            ..
        } => {
            // Failure to execute a contract is a user error, not a system error.
            // We do want to see the deploy hash, error, and cost in the logs.
            // We don't need to see the effects in the logs.
            debug!(?deploy_hash, ?error, %cost, "execution failure");
            effect
        }
    };
    let commit_result = commit(
        engine_state,
        metrics,
        state_root_hash,
        execution_effect.transforms,
    )
    .await;
    trace!(?commit_result, "commit result");
    match commit_result {
        Ok(CommitResult::Success { state_root }) => {
            debug!(?state_root, "commit succeeded");
            Ok((state_root.into(), execution_result))
        }
        _ => {
            error!(
                ?commit_result,
                "commit failed - internal contract runtime error"
            );
            Err(())
        }
    }
}

async fn commit(
    engine_state: Arc<EngineState<LmdbGlobalState>>,
    metrics: Arc<ContractRuntimeMetrics>,
    state_root_hash: Digest,
    effects: AdditiveMap<Key, Transform>,
) -> Result<CommitResult, engine_state::Error> {
    trace!(?state_root_hash, ?effects, "commit");
    task::spawn_blocking(move || {
        let correlation_id = CorrelationId::new();
        let start = Instant::now();
        let result = engine_state.apply_effect(correlation_id, state_root_hash.into(), effects);
        metrics.apply_effect.observe(start.elapsed().as_secs_f64());
        trace!(?result, "commit result");
        result
    })
    .await
    .expect("should run commit task")
}

pub(super) async fn execute(
    engine_state: Arc<EngineState<LmdbGlobalState>>,
    metrics: Arc<ContractRuntimeMetrics>,
    execute_request: ExecuteRequest,
) -> Result<VecDeque<EngineExecutionResult>, engine_state::Error> {
    trace!(?execute_request, "execute");
    task::spawn_blocking(move || {
        let correlation_id = CorrelationId::new();
        let start = Instant::now();
        let result = engine_state.run_execute(correlation_id, execute_request);
        metrics.run_execute.observe(start.elapsed().as_secs_f64());
        trace!(?result, "execute result");
        result
    })
    .await
    .expect("should run execute task")
}
