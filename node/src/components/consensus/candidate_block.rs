use blake2::{
    digest::{Update, VariableOutput},
    VarBlake2b,
};
use datasize::DataSize;
use derive_more::Display;
use serde::{Deserialize, Serialize};

use casper_types::PublicKey;

use crate::{
    components::consensus::traits::ConsensusValueT,
    crypto::hash::Digest,
    types::{ProtoBlock, Timestamp},
};

#[derive(
    Copy,
    Clone,
    DataSize,
    Debug,
    Display,
    PartialOrd,
    Ord,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
)]
pub(crate) struct CandidateBlockHash(Digest);

/// A proposed block. Once the consensus protocol reaches agreement on it, it will be converted to
/// a `FinalizedBlock`.
#[derive(Clone, DataSize, Debug, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CandidateBlock {
    proto_block: ProtoBlock,
    timestamp: Timestamp,
    accusations: Vec<PublicKey>,
}

impl CandidateBlock {
    /// Creates a new candidate block, wrapping a proto block and accusing the given validators.
    pub(crate) fn new(
        proto_block: ProtoBlock,
        timestamp: Timestamp,
        accusations: Vec<PublicKey>,
    ) -> Self {
        CandidateBlock {
            proto_block,
            timestamp,
            accusations,
        }
    }

    /// Returns the proto block containing the deploys.
    pub(crate) fn proto_block(&self) -> &ProtoBlock {
        &self.proto_block
    }

    /// Returns the candidate block's timestamp, i.e. when the block was proposed.
    ///
    /// This is identical to the timestamp of the Highway unit, and the timestamp of the `Block`,
    /// if it gets finalized.
    pub(crate) fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns the validators accused by this block.
    pub(crate) fn accusations(&self) -> &Vec<PublicKey> {
        &self.accusations
    }

    pub(crate) fn hash(&self) -> CandidateBlockHash {
        let CandidateBlock {
            proto_block,
            timestamp,
            accusations,
        } = self;
        let mut result = [0; Digest::LENGTH];

        let mut hasher = VarBlake2b::new(Digest::LENGTH).expect("should create hasher");
        hasher.update(proto_block.hash().inner());
        let data = (timestamp, accusations);
        hasher.update(bincode::serialize(&data).expect("should serialize candidate block data"));
        hasher.finalize_variable(|slice| {
            result.copy_from_slice(slice);
        });
        CandidateBlockHash(result.into())
    }

    pub(crate) fn needs_validation(&self) -> bool {
        !self.proto_block.wasm_deploys().is_empty() || !self.proto_block.transfers().is_empty()
    }
}

impl From<CandidateBlock> for ProtoBlock {
    fn from(cb: CandidateBlock) -> ProtoBlock {
        cb.proto_block
    }
}

impl ConsensusValueT for CandidateBlock {
    type Hash = CandidateBlockHash;

    fn hash(&self) -> Self::Hash {
        self.hash()
    }

    fn needs_validation(&self) -> bool {
        self.needs_validation()
    }
}
