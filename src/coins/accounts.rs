use cosmrs::crypto::PublicKey;

use crate::coins::{Address, Amount, Coin, Give, Symbol, Take};
use crate::collections::map::Iter as MapIter;
use crate::collections::Map;
use crate::context::GetContext;
use crate::migrate::Migrate;
use crate::orga;
use crate::plugins::Paid;
use crate::plugins::Signer;
use crate::state::State;
use crate::{Error, Result};

#[orga]
pub struct Accounts<S: Symbol> {
    transfers_allowed: bool,
    transfer_exceptions: Map<Address, ()>,
    accounts: Map<Address, Coin<S>>,
    pub_keys: Map<Address, PublicKey>,
}

impl Migrate for PublicKey {}
impl State for PublicKey {
    fn load(_store: orga::store::Store, _bytes: &mut &[u8]) -> orga::Result<Self> {
        unreachable!()
    }

    fn attach(&mut self, _store: orga::store::Store) -> orga::Result<()> {
        unreachable!()
    }

    fn flush<W: std::io::Write>(self, _out: &mut W) -> orga::Result<()> {
        unreachable!()
    }
}

// impl<K, V> State for Map<K, V>
// where
//     K: Encode + Terminated + 'static,
//     V: State,
// {
//     fn attach(&mut self, store: Store) -> Result<()> {
//         for (key, value) in self.children.iter_mut() {
//             value.attach(store.sub(key.inner_bytes.as_slice()))?;
//         }
//         self.store.attach(store)
//     }

//     fn flush<W: std::io::Write>(mut self, _out: &mut W) -> Result<()> {
//         while let Some((key, maybe_value)) = self.children.pop_first() {
//             Self::apply_change(&mut self.store, key.inner.encode()?, maybe_value)?;
//         }

//         Ok(())
//     }

//     fn load(store: Store, _bytes: &mut &[u8]) -> Result<Self> {
//         let mut map = Self::default();
//         map.attach(store)?;

//         Ok(map)
//     }
// }

#[orga]
impl<S: Symbol> Accounts<S> {
    pub fn iter(&self) -> Result<MapIter<Address, Coin<S>>> {
        self.accounts.iter()
    }

    #[call]
    pub fn transfer(&mut self, to: Address, amount: Amount) -> Result<()> {
        let signer = self.signer()?;
        if !self.transfers_allowed && !self.transfer_exceptions.contains_key(signer)? {
            return Err(Error::Coins("Transfers are currently disabled".into()));
        }
        let taken_coins = self.take_own_coins(amount)?;
        let mut receiver = self.accounts.entry(to)?.or_insert_default()?;
        receiver.give(taken_coins)?;

        Ok(())
    }

    // #[call]
    // pub fn store_pubkey(&mut self, addr: Address, pub_key: PublicKey) -> Result<()> {
    //     let mut receiver = self
    //         .pub_keys
    //         .entry(addr)?
    //         .or_create(PublicKey::from_json("").map_err(|err| Error::App(err.to_string()))?)?;

    //     Ok(())
    // }

    #[call]
    pub fn take_as_funding(&mut self, amount: Amount) -> Result<()> {
        let taken_coins = self.take_own_coins(amount)?;

        let paid = self
            .context::<Paid>()
            .ok_or_else(|| Error::Coins("No Paid context found".into()))?;

        paid.give::<S, _>(taken_coins.amount)
    }

    fn take_own_coins(&mut self, amount: Amount) -> Result<Coin<S>> {
        let signer = self.signer()?;

        let taken_coins = self
            .accounts
            .get_mut(signer)?
            .ok_or_else(|| Error::Coins("Insufficient funds".into()))?
            .take(amount)?;

        Ok(taken_coins)
    }

    fn signer(&mut self) -> Result<Address> {
        self.context::<Signer>()
            .ok_or_else(|| Error::Signer("No Signer context available".into()))?
            .signer
            .ok_or_else(|| Error::Coins("Unauthorized account action".into()))
    }

    #[call]
    pub fn give_from_funding(&mut self, amount: Amount) -> Result<()> {
        let taken_coins = self
            .context::<Paid>()
            .ok_or_else(|| Error::Coins("No Paid context found".into()))?
            .take(amount)?;

        self.give_own_coins(taken_coins)
    }

    #[call]
    pub fn give_from_funding_all(&mut self) -> Result<()> {
        let paid = self
            .context::<Paid>()
            .ok_or_else(|| Error::Coins("No Paid context found".into()))?;
        let balance = paid.balance::<S>()?;
        let taken_coins = paid.take(balance)?;

        self.give_own_coins(taken_coins)
    }

    fn give_own_coins(&mut self, coins: Coin<S>) -> Result<()> {
        let signer = self.signer()?;

        self.accounts
            .entry(signer)?
            .or_insert_default()?
            .give(coins)?;

        Ok(())
    }

    #[query]
    pub fn balance(&self, address: Address) -> Result<Amount> {
        match self.accounts.get(address)? {
            Some(coin) => Ok(coin.amount),
            None => Ok(0.into()),
        }
    }

    #[query]
    pub fn exists(&self, address: Address) -> Result<bool> {
        Ok(self.accounts.get(address)?.is_some())
    }

    pub fn allow_transfers(&mut self, enabled: bool) {
        self.transfers_allowed = enabled;
    }

    pub fn add_transfer_exception(&mut self, address: Address) -> Result<()> {
        self.transfer_exceptions.insert(address, ())
    }

    pub fn deposit(&mut self, address: Address, coins: Coin<S>) -> Result<()> {
        let mut account = self.accounts.entry(address)?.or_insert_default()?;
        account.give(coins)?;

        Ok(())
    }

    pub fn withdraw(&mut self, address: Address, amount: Amount) -> Result<Coin<S>> {
        let mut account = self.accounts.entry(address)?.or_insert_default()?;
        account.take(amount)
    }
}
