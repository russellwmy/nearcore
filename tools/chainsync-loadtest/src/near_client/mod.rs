pub use near_client_primitives::types::{
    Error, GetBlock, GetBlockProof, GetBlockProofResponse, GetBlockWithMerkleTree, GetChunk,
    GetExecutionOutcome, GetExecutionOutcomeResponse, GetExecutionOutcomesForBlock, GetGasPrice,
    GetNetworkInfo, GetNextLightClientBlock, GetProtocolConfig, GetReceipt, GetStateChanges,
    GetStateChangesInBlock, GetStateChangesWithCauseInBlock, GetValidatorInfo, GetValidatorOrdered,
    Query, QueryError, Status, StatusResponse, SyncStatus, TxStatus, TxStatusError,
};

pub use crate::near_client::client::Client;
pub use crate::near_client::client_actor::{ClientActor,Network};

mod client;
mod client_actor;
pub mod sync;
