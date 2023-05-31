use crate::call::Call as CallTrait;
use crate::coins::{Address, Symbol};
use crate::describe::Describe;
use crate::encoding::{Decode, Encode};
use crate::migrate::{MigrateFrom, MigrateInto};
use crate::query::{FieldQuery, Query};
use crate::state::State;
use crate::{Error, Result};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

pub const MAX_CALL_SIZE: usize = 65_535;
pub const NATIVE_CALL_FLAG: u8 = 0xff;

#[derive(State, FieldQuery, Default, Clone, Describe)]
pub struct SdkCompatPlugin<S, T> {
    pub(crate) symbol: PhantomData<S>,
    pub inner: T,
}

impl<S1, S2, T1: State, T2: State> MigrateFrom<SdkCompatPlugin<S1, T1>> for SdkCompatPlugin<S2, T2>
where
    T1: MigrateInto<T2>,
{
    fn migrate_from(other: SdkCompatPlugin<S1, T1>) -> Result<Self> {
        Ok(Self {
            symbol: other.symbol.migrate_into()?,
            inner: other.inner.migrate_into()?,
        })
    }
}

#[derive(Debug)]
pub enum Call<T> {
    Native(T),
    Sdk(sdk::Tx),
}

unsafe impl<T> Send for Call<T> {}

impl<T: Encode> Encode for Call<T> {
    fn encoding_length(&self) -> ed::Result<usize> {
        match self {
            Call::Native(native) => Ok(native.encoding_length()? + 1),
            Call::Sdk(tx) => tx.encoding_length(),
        }
    }

    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        match self {
            Call::Native(native) => {
                NATIVE_CALL_FLAG.encode_into(dest)?;
                native.encode_into(dest)
            }
            Call::Sdk(tx) => tx.encode_into(dest),
        }
    }
}

impl<T: Decode> Decode for Call<T> {
    fn decode<R: std::io::Read>(mut reader: R) -> ed::Result<Self> {
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes)?;

        if bytes.len() > MAX_CALL_SIZE {
            return Err(ed::Error::UnexpectedByte(0));
        }

        match bytes.first() {
            Some(&NATIVE_CALL_FLAG) => {
                let native = T::decode(&bytes.as_slice()[1..])?;
                Ok(Call::Native(native))
            }
            Some(_) => {
                let tx = sdk::Tx::decode(bytes.as_slice())?;
                Ok(Call::Sdk(tx))
            }
            None => {
                let io_err = std::io::ErrorKind::UnexpectedEof.into();
                Err(ed::Error::IOError(io_err))
            }
        }
    }
}

pub mod sdk {
    use super::{Address, Decode, Encode, Error, Result, MAX_CALL_SIZE};
    use cosmrs::proto::cosmos::tx::v1beta1::Tx as ProtoTx;
    use prost::Message;
    use serde::{Deserialize, Serialize};
    use std::io::{Error as IoError, ErrorKind};

    #[derive(Debug, Clone)]
    pub enum Tx {
        Amino(AminoTx),
        Protobuf(cosmrs::Tx),
    }

    impl Encode for Tx {
        fn encoding_length(&self) -> ed::Result<usize> {
            match self {
                Tx::Amino(tx) => {
                    let bytes = serde_json::to_vec(tx).map_err(|_| ed::Error::UnexpectedByte(0))?;
                    Ok(bytes.len())
                }
                Tx::Protobuf(tx) => {
                    let tx: ProtoTx = tx.clone().into();
                    Ok(tx.encoded_len())
                }
            }
        }

        fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
            let bytes = match self {
                Tx::Amino(tx) => {
                    serde_json::to_vec(tx).map_err(|_| ed::Error::UnexpectedByte(0))?
                }
                Tx::Protobuf(tx) => {
                    let tx: ProtoTx = tx.clone().into();
                    tx.encode_to_vec()
                }
            };

            if bytes.len() > MAX_CALL_SIZE {
                return Err(ed::Error::UnexpectedByte(0));
            }

            dest.write_all(&bytes)?;
            Ok(())
        }
    }

    impl Decode for Tx {
        fn decode<R: std::io::Read>(mut reader: R) -> ed::Result<Self> {
            let mut bytes = Vec::with_capacity(MAX_CALL_SIZE);
            reader.read_to_end(&mut bytes)?;

            if bytes.len() > MAX_CALL_SIZE || bytes.is_empty() {
                return Err(ed::Error::UnexpectedByte(0));
            }

            if b'{' == bytes[0] {
                let tx = serde_json::from_slice(bytes.as_slice())
                    .map_err(|_| ed::Error::UnexpectedByte(123))?;
                return Ok(Tx::Amino(tx));
            }

            let tx = cosmrs::Tx::from_bytes(bytes.as_slice())
                .map_err(|e| IoError::new(ErrorKind::InvalidData, e))?;
            Ok(Tx::Protobuf(tx))
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct AminoTx {
        pub msg: Vec<Msg>,
        pub fee: Fee,
        pub memo: String,
        pub signatures: Vec<Signature>,
    }

    impl Tx {
        pub fn sign_bytes(&self, chain_id: String, nonce: u64) -> Result<Vec<u8>> {
            match self {
                Tx::Amino(tx) => {
                    let sign_tx = SignDoc {
                        account_number: "0".to_string(),
                        chain_id,
                        fee: tx.fee.clone(),
                        memo: tx.memo.clone(),
                        msgs: tx.msg.clone(),
                        sequence: nonce.to_string(),
                    };

                    serde_json::to_vec(&sign_tx).map_err(|e| Error::App(e.to_string()))
                }
                Tx::Protobuf(tx) => {
                    let tx = tx.clone();
                    let signdoc = cosmrs::tx::SignDoc {
                        body_bytes: tx
                            .body
                            .into_bytes()
                            .map_err(|e| Error::App(e.to_string()))?,
                        auth_info_bytes: tx
                            .auth_info
                            .into_bytes()
                            .map_err(|e| Error::App(e.to_string()))?,
                        chain_id,
                        account_number: 0,
                    };
                    signdoc.into_bytes().map_err(|e| Error::App(e.to_string()))
                }
            }
        }

        pub fn sender_pubkey(&self) -> Result<[u8; 33]> {
            let pubkey_vec = match self {
                Tx::Amino(tx) => {
                    let pubkey_b64 = &tx
                        .signatures
                        .first()
                        .ok_or_else(|| Error::App("No signatures provided".to_string()))?
                        .pub_key
                        .value;

                    base64::decode(pubkey_b64).map_err(|e| Error::App(e.to_string()))?
                }
                Tx::Protobuf(tx) => tx
                    .auth_info
                    .signer_infos
                    .first()
                    .ok_or_else(|| Error::App("No auth info provided".to_string()))?
                    .public_key
                    .as_ref()
                    .ok_or_else(|| Error::App("No public key provided".to_string()))?
                    .single()
                    .ok_or_else(|| Error::App("Invalid public key".to_string()))?
                    .to_bytes(),
            };

            let mut pubkey_arr = [0; 33];
            pubkey_arr.copy_from_slice(&pubkey_vec);

            Ok(pubkey_arr)
        }

        pub fn sender_address(&self) -> Result<Address> {
            let signer_call = super::super::signer::sdk_to_signercall(self)?;
            signer_call.address()
        }

        pub fn signature(&self) -> Result<[u8; 64]> {
            let sig_vec = match self {
                Tx::Amino(tx) => {
                    let sig_b64 = &tx
                        .signatures
                        .first()
                        .ok_or_else(|| Error::App("No signatures provided".to_string()))?
                        .signature;

                    base64::decode(sig_b64).map_err(|e| Error::App(e.to_string()))?
                }
                Tx::Protobuf(tx) => tx
                    .signatures
                    .first()
                    .ok_or_else(|| Error::App("No signatures provided".to_string()))?
                    .clone(),
            };

            let mut sig_arr = [0; 64];
            sig_arr.copy_from_slice(&sig_vec);

            Ok(sig_arr)
        }

        pub fn sig_type(&self) -> Result<Option<&str>> {
            Ok(match self {
                Tx::Amino(tx) => tx
                    .signatures
                    .first()
                    .ok_or_else(|| Error::App("No signatures provided".to_string()))?
                    .r#type
                    .as_deref(),

                Tx::Protobuf(_) => None,
            })
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct SignDoc {
        pub account_number: String,
        pub chain_id: String,
        pub fee: Fee,
        pub memo: String,
        pub msgs: Vec<Msg>,
        pub sequence: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Msg {
        #[serde(rename = "type")]
        pub type_: String,
        pub value: serde_json::Value,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Fee {
        pub amount: Vec<Coin>,
        pub gas: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Coin {
        pub amount: String,
        pub denom: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Signature {
        pub pub_key: PubKey,
        pub signature: String,
        pub r#type: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct PubKey {
        #[serde(rename = "type")]
        pub type_: String,
        pub value: String,
    }

    #[derive(Deserialize, Debug, Clone)]
    pub struct MsgSend {
        pub from_address: String,
        pub to_address: String,
        pub amount: Vec<Coin>,
    }

    #[derive(Deserialize, Debug, Clone)]
    pub struct MsgDelegate {
        pub delegator_address: String,
        pub validator_address: String,
        pub amount: Option<Coin>,
    }

    #[derive(Deserialize, Debug, Clone)]
    pub struct MsgBeginRedelegate {
        pub delegator_address: String,
        pub validator_src_address: String,
        pub validator_dst_address: String,
        pub amount: Option<Coin>,
    }

    #[derive(Deserialize, Debug, Clone)]
    pub struct MsgUndelegate {
        pub delegator_address: String,
        pub validator_address: String,
        pub amount: Option<Coin>,
    }
}

pub trait ConvertSdkTx {
    type Output;

    fn convert(&self, msg: &sdk::Tx) -> Result<Self::Output>;
}

impl<S: Symbol, T> CallTrait for SdkCompatPlugin<S, T>
where
    T: CallTrait + State + ConvertSdkTx<Output = T::Call>,
{
    type Call = Call<T::Call>;

    fn call(&mut self, call: Self::Call) -> Result<()> {
        let call = match call {
            Call::Native(call) => call,
            Call::Sdk(tx) => self.inner.convert(&tx)?,
        };

        self.inner.call(call)
    }
}

#[cfg(feature = "abci")]
mod abci {
    use super::super::*;
    use super::*;
    use crate::abci::{BeginBlock, EndBlock, InitChain};

    impl<S, T: State> BeginBlock for SdkCompatPlugin<S, T>
    where
        T: BeginBlock + State,
    {
        fn begin_block(&mut self, ctx: &BeginBlockCtx) -> Result<()> {
            self.inner.begin_block(ctx)
        }
    }

    impl<S, T: State> EndBlock for SdkCompatPlugin<S, T>
    where
        T: EndBlock + State,
    {
        fn end_block(&mut self, ctx: &EndBlockCtx) -> Result<()> {
            self.inner.end_block(ctx)
        }
    }

    impl<S, T: State> InitChain for SdkCompatPlugin<S, T>
    where
        T: InitChain + State,
    {
        fn init_chain(&mut self, ctx: &InitChainCtx) -> Result<()> {
            self.inner.init_chain(ctx)
        }
    }

    impl<S, T> crate::abci::AbciQuery for SdkCompatPlugin<S, T>
    where
        T: crate::abci::AbciQuery + State + CallTrait,
    {
        fn abci_query(
            &self,
            request: &tendermint_proto::abci::RequestQuery,
        ) -> Result<tendermint_proto::abci::ResponseQuery> {
            self.inner.abci_query(request)
        }
    }
}
