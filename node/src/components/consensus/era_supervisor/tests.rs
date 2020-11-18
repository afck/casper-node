use anyhow::Error;
use casper_execution_engine::shared::motes::Motes;
use derive_more::From;
use prometheus::Registry;

use super::*;
use crate::{
    components::{
        consensus::{
            consensus_protocol::EraEnd,
            highway_core::highway_testing::{
                new_test_chainspec, ALICE_PUBLIC_KEY, ALICE_SECRET_KEY, BOB_PUBLIC_KEY,
            },
            tests::mock_proto::{self, MockProto, NodeId},
            Config,
        },
        Component,
    },
    crypto::asymmetric_key::PublicKey,
    effect::{
        announcements::ConsensusAnnouncement,
        requests::{
            BlockExecutorRequest, BlockProposerRequest, BlockValidationRequest,
            ContractRuntimeRequest, NetworkRequest, StorageRequest,
        },
        EffectBuilder,
    },
    protocol,
    reactor::{EventQueueHandle, QueueKind, Scheduler},
    testing::TestRng,
    utils::{self, External},
    NodeRng,
};

type ClMessage = mock_proto::Message<ClContext>;

#[derive(Debug, From)]
enum Event {
    #[from]
    Consensus(super::Event<NodeId>),
    #[from]
    Network(NetworkRequest<NodeId, protocol::Message>),
    #[from]
    BlockProposer(BlockProposerRequest),
    #[from]
    ConsensusAnnouncement(ConsensusAnnouncement),
    #[from]
    BlockExecutor(BlockExecutorRequest),
    #[from]
    BlockValidator(BlockValidationRequest<ProtoBlock, NodeId>),
    #[from]
    Storage(StorageRequest),
    #[from]
    ContractRuntime(ContractRuntimeRequest),
}

struct MockReactor {
    scheduler: &'static Scheduler<Event>,
}

impl MockReactor {
    fn new() -> Self {
        MockReactor {
            scheduler: utils::leak(Scheduler::<Event>::new(QueueKind::weights())),
        }
    }

    /// Checks and responds to a block validation request.
    async fn expect_block_validation(
        &self,
        expected_block: &ProtoBlock,
        expected_sender: NodeId,
        expected_timestamp: Timestamp,
        valid: bool,
    ) {
        let (event, _) = self.scheduler.pop().await;
        if let Event::BlockValidator(BlockValidationRequest {
            block,
            sender,
            responder,
            block_timestamp,
        }) = event
        {
            assert_eq!(expected_block, &block);
            assert_eq!(expected_sender, sender);
            assert_eq!(expected_timestamp, block_timestamp);
            responder.respond((valid, block)).await;
        } else {
            panic!(
                "unexpected event: {:?}, expected block validation request",
                event
            );
        }
    }

    async fn expect_send_message(&self, peer: NodeId) {
        let (event, _) = self.scheduler.pop().await;
        if let Event::Network(NetworkRequest::SendMessage {
            dest, responder, ..
        }) = event
        {
            assert_eq!(peer, dest);
            responder.respond(()).await;
        } else {
            panic!(
                "unexpected event: {:?}, expected send message request",
                event
            );
        }
    }

    async fn expect_broadcast(&self) {
        let (event, _) = self.scheduler.pop().await;
        if let Event::Network(NetworkRequest::Broadcast { responder, .. }) = event {
            responder.respond(()).await;
        } else {
            panic!("unexpected event: {:?}, expected broadcast request", event);
        }
    }

    async fn expect_proposed(&self) -> ProtoBlock {
        let (event, _) = self.scheduler.pop().await;
        if let Event::ConsensusAnnouncement(ConsensusAnnouncement::Proposed(proto_block)) = event {
            proto_block
        } else {
            panic!(
                "unexpected event: {:?}, expected announcement of proposed proto block",
                event
            );
        }
    }

    async fn expect_finalized(&self) -> FinalizedBlock {
        let (event, _) = self.scheduler.pop().await;
        if let Event::ConsensusAnnouncement(ConsensusAnnouncement::Finalized(fb)) = event {
            *fb
        } else {
            panic!(
                "unexpected event: {:?}, expected announcement of finalized block",
                event
            );
        }
    }

    async fn expect_execute(&self) -> FinalizedBlock {
        let (event, _) = self.scheduler.pop().await;
        if let Event::BlockExecutor(BlockExecutorRequest::ExecuteBlock(fb)) = event {
            fb
        } else {
            panic!(
                "unexpected event: {:?}, expected block execution request",
                event
            );
        }
    }
}

async fn propose_and_finalize(
    es: &mut EraSupervisor<NodeId>,
    proposer: PublicKey,
    accusations: Vec<PublicKey>,
    rng: &mut NodeRng,
) -> FinalizedBlock {
    let reactor = MockReactor::new();
    let effect_builder = EffectBuilder::new(EventQueueHandle::new(reactor.scheduler));
    let mut handle = |es: &mut EraSupervisor<NodeId>, event: super::Event<NodeId>| {
        es.handle_event(effect_builder, rng, event)
    };

    // Propose a new block. We receive that proposal via node 1.
    let proto_block = ProtoBlock::new(vec![], true);
    let candidate_block = CandidateBlock::new(proto_block.clone(), accusations.clone());
    let timestamp = Timestamp::now();
    let event = ClMessage::BlockByOtherValidator {
        value: candidate_block.clone(),
        timestamp,
        proposer,
    }
    .received(NodeId(1), EraId(0)); // TODO: Compute era ID.
    let mut effects = handle(es, event);
    let validate = tokio::spawn(effects.pop().unwrap());
    reactor
        .expect_block_validation(&proto_block, NodeId(1), timestamp, true)
        .await;
    let rv_event = validate.await.unwrap().pop().unwrap();

    // As a result, the era supervisor should request validation of the proto block and evidence
    // against Alice.
    for _ in &accusations {
        let request_evidence = tokio::spawn(effects.pop().unwrap());
        reactor.expect_send_message(NodeId(1)).await; // Sending request for evidence.
        request_evidence.await.unwrap();
    }
    assert!(effects.is_empty());

    // Node 1 replies with requested evidence.
    for pk in &accusations {
        let event = ClMessage::Evidence(pk.clone()).received(NodeId(1), EraId(0));
        let mut effects = handle(es, event);
        let broadcast_evidence = tokio::spawn(effects.pop().unwrap());
        assert!(effects.is_empty());
        reactor.expect_broadcast().await; //Gossip evidence to other nodes.
        broadcast_evidence.await.unwrap();
    }

    // The block validator returns true: The deploys in the block are valid. That completes our
    // requirements for the proposed block. The era supervisor announces that it has passed the
    // proposed block to the consensus protocol.
    let mut effects = handle(es, rv_event);
    let announce_proposed = tokio::spawn(effects.pop().unwrap());
    assert!(effects.is_empty());
    assert_eq!(proto_block, reactor.expect_proposed().await);
    announce_proposed.await.unwrap();

    // Node 1 now sends us another message that is sufficient for the protocol to finalize the
    // block. The era supervisor is expected to announce finalization, and to request execution.
    let event = ClMessage::FinalizeBlock.received(NodeId(1), EraId(0));
    let mut effects = handle(es, event);
    tokio::spawn(effects.pop().unwrap()).await.unwrap();
    tokio::spawn(effects.pop().unwrap()).await.unwrap();
    assert!(effects.is_empty());

    let fb = reactor.expect_execute().await;
    assert_eq!(fb, reactor.expect_finalized().await);
    fb
}

#[tokio::test]
async fn cross_era_slashing() -> Result<(), Error> {
    let mut rng = TestRng::new();

    let (mut es, effects) = {
        let chainspec = new_test_chainspec(vec![(*ALICE_PUBLIC_KEY, 10), (*BOB_PUBLIC_KEY, 100)]);
        let config = Config {
            secret_key_path: External::Loaded(ALICE_SECRET_KEY.clone()),
        };

        let registry = Registry::new();
        let reactor = MockReactor::new();
        let effect_builder = EffectBuilder::new(EventQueueHandle::new(reactor.scheduler));

        // Initialize the era supervisor. There are two validators, Alice and Bob. This instance,
        // however, is only a passive observer.
        EraSupervisor::new(
            Timestamp::now(),
            WithDir::new("tmp", config),
            effect_builder,
            vec![
                (*ALICE_PUBLIC_KEY, Motes::new(10.into())),
                (*BOB_PUBLIC_KEY, Motes::new(100.into())),
            ],
            &chainspec,
            Default::default(), // genesis state root hash
            &registry,
            Box::new(MockProto::new_boxed),
            &mut rng,
        )?
    };
    assert!(effects.is_empty());

    let fb =
        propose_and_finalize(&mut es, *BOB_PUBLIC_KEY, vec![*ALICE_PUBLIC_KEY], &mut rng).await;
    let expected_fb = FinalizedBlock::new(
        fb.proto_block().clone(),
        fb.timestamp(),
        None, // not the era's last block
        EraId(0),
        0, // height
        *BOB_PUBLIC_KEY,
    );
    assert_eq!(expected_fb, fb);

    let fb = propose_and_finalize(&mut es, *BOB_PUBLIC_KEY, vec![], &mut rng).await;
    let expected_fb = FinalizedBlock::new(
        fb.proto_block().clone(),
        fb.timestamp(),
        Some(EraEnd {
            equivocators: vec![*ALICE_PUBLIC_KEY],
            rewards: Default::default(),
        }), // the era's last block
        EraId(0),
        1, // height
        *BOB_PUBLIC_KEY,
    );
    assert_eq!(expected_fb, fb);

    Ok(())
}
