mod signer;
pub use signer::*;

mod nonce;
pub use nonce::*;

mod abci;
pub use abci::*;

mod payable;
pub use payable::*;

mod fee;
pub use fee::*;

pub mod chain_commitment;
pub use chain_commitment::ChainCommitmentPlugin;

pub mod sdk_compat;
pub use sdk_compat::SdkCompatPlugin;

pub type DefaultPlugins<S, T, const ID: &'static str> = SdkCompatPlugin<
    S,
    SignerPlugin<NoncePlugin<ChainCommitmentPlugin<PayablePlugin<FeePlugin<S, T>>, ID>>>,
    ID,
>;
