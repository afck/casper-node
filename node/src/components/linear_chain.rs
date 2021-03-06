use super::{storage::Storage, Component};
use crate::{
    components::storage::Value,
    crypto::asymmetric_key::Signature,
    effect::{
        self,
        requests::{LinearChainRequest, StorageRequest},
        EffectExt, Effects,
    },
    protocol::Message,
    types::{Block, BlockHash},
};
use derive_more::From;
use effect::requests::{ConsensusRequest, NetworkRequest};
use futures::FutureExt;
use rand::{CryptoRng, Rng};
use std::fmt::Display;
use tracing::{debug, error, warn};

#[derive(Debug, From)]
pub enum Event<I> {
    /// A linear chain request issued by another node in the network.
    #[from]
    Request(LinearChainRequest<I>),
    /// New linear chain block has been produced.
    LinearChainBlock(Block),
    /// A continuation for `GetBlock` scenario.
    GetBlockResult(BlockHash, Option<Block>, I),
    /// New finality signature.
    NewFinalitySignature(BlockHash, Signature),
    /// The result of putting a block to storage.
    PutBlockResult(Block),
}

impl<I: Display> Display for Event<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Request(req) => write!(f, "linear-chain request: {}", req),
            Event::LinearChainBlock(b) => write!(f, "linear-chain new block: {}", b.hash()),
            Event::GetBlockResult(bh, res, peer) => write!(
                f,
                "linear-chain get-block for {} from {} found: {}",
                bh,
                peer,
                res.is_some()
            ),
            Event::NewFinalitySignature(bh, _) => {
                write!(f, "linear-chain new finality signature for block: {}", bh)
            }
            Event::PutBlockResult(_) => write!(f, "linear-chain put-block result"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LinearChain<I> {
    _marker: std::marker::PhantomData<I>,
    /// The last block this component put to storage which is presumably the last block in the
    /// linear chain.
    last_block: Option<Block>,
}

impl<I> LinearChain<I> {
    pub fn new() -> Self {
        LinearChain {
            _marker: std::marker::PhantomData,
            last_block: None,
        }
    }
}

impl<I, REv, R> Component<REv, R> for LinearChain<I>
where
    REv: From<StorageRequest<Storage>>
        + From<ConsensusRequest>
        + From<NetworkRequest<I, Message>>
        + Send,
    R: Rng + CryptoRng + ?Sized,
    I: Display + Send + 'static,
{
    type Event = Event<I>;

    fn handle_event(
        &mut self,
        effect_builder: crate::effect::EffectBuilder<REv>,
        _rng: &mut R,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::Request(LinearChainRequest::BlockRequest(bh, sender)) => effect_builder
                .get_block_from_storage(bh)
                .event(move |maybe_block| Event::GetBlockResult(bh, maybe_block, sender)),
            Event::Request(LinearChainRequest::LastFinalizedBlock(responder)) => {
                responder.respond(self.last_block.clone()).ignore()
            }
            Event::GetBlockResult(block_hash, maybe_block, sender) => {
                match maybe_block {
                    None => {
                        debug!("failed to get {} for {}", block_hash, sender);
                        Effects::new()
                    },
                    Some(block) => match Message::new_get_response(&block) {
                        Ok(message) => effect_builder.send_message(sender, message).ignore(),
                        Err(error) => {
                            error!("failed to create get-response {}", error);
                            Effects::new()
                        }
                    }
                }
            }
            Event::LinearChainBlock(block) => {
                effect_builder
                .put_block_to_storage(Box::new(block.clone()))
                .event(move |_| Event::PutBlockResult(block))
            },
            Event::PutBlockResult(block) => {
                let block_hash = *block.hash();
                debug!("LinearChainBlock --block_hash: {}", block_hash);
                self.last_block = Some(block.clone());
                effect_builder.handle_linear_chain_block(block.header().clone())
                    .event(move |signature| Event::NewFinalitySignature(block_hash, signature))
            },
            Event::NewFinalitySignature(bh, signature) => {
                effect_builder
                .clone()
                    .get_block_from_storage(bh)
                    .then(move |maybe_block| match maybe_block {
                        Some(mut block) => {
                            block.append_proof(signature);
                            effect_builder.put_block_to_storage(Box::new(block))
                        }
                        None => {
                            warn!("Received a signature for {} but block was not found in the Linear chain storage", bh);
                            panic!("Unhandled")
                        }
                    })
                    .ignore()
            },
        }
    }
}
