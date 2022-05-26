use super::{
    adapter::Adapter,
    header_queue::HeaderQueue,
    signatory::SignatorySet,
    threshold_sig::{LengthVec, Pubkey, Signature, ThresholdSig},
    ConsensusKey, Xpub,
};
use crate::error::{Error, Result};
use bitcoin::hashes::Hash;
use bitcoin::Txid;
use derive_more::{Deref, DerefMut};
use orga::{
    call::Call,
    client::Client,
    coins::Address,
    collections::{ChildMut, Deque, Map, Ref},
    context::GetContext,
    encoding::{Decode, Encode},
    plugins::{Signer, Time},
    query::Query,
    state::State,
    Error as OrgaError, Result as OrgaResult,
};

pub const CHECKPOINT_INTERVAL: u64 = 60 * 10;
pub const MAX_INPUTS: u64 = 50;
pub const MAX_OUTPUTS: u64 = 200;

#[derive(Debug, Encode, Decode)]
pub enum CheckpointStatus {
    Building,
    Signing,
    Complete,
}

impl Default for CheckpointStatus {
    fn default() -> Self {
        Self::Building
    }
}

// TODO: make it easy to derive State for simple types like this
impl State for CheckpointStatus {
    type Encoding = Self;

    fn create(_: orga::store::Store, data: Self) -> OrgaResult<Self> {
        Ok(data)
    }

    fn flush(self) -> OrgaResult<Self> {
        Ok(self)
    }
}

impl Query for CheckpointStatus {
    type Query = ();

    fn query(&self, _: ()) -> OrgaResult<()> {
        Ok(())
    }
}

impl Call for CheckpointStatus {
    type Call = ();

    fn call(&mut self, _: ()) -> OrgaResult<()> {
        Ok(())
    }
}

impl<U: Send + Clone> Client<U> for CheckpointStatus {
    type Client = orga::client::PrimitiveClient<Self, U>;

    fn create_client(parent: U) -> Self::Client {
        orga::client::PrimitiveClient::new(parent)
    }
}

#[derive(State, Call, Query, Client, Debug)]
pub struct Input {
    pub prevout: Adapter<bitcoin::OutPoint>,
    pub script_pubkey: Adapter<bitcoin::Script>,
    pub redeem_script: Adapter<bitcoin::Script>,
    pub sigset_index: u32,
    pub dest: Address,
    pub amount: u64,
    pub sigs: ThresholdSig,
}

impl Input {
    pub fn to_txin(&self) -> Result<bitcoin::TxIn> {
        let mut witness = self.sigs.to_witness()?;
        if self.sigs.done() {
            witness.push(self.redeem_script.to_bytes());
        }

        Ok(bitcoin::TxIn {
            previous_output: *self.prevout,
            script_sig: bitcoin::Script::new(),
            sequence: u32::MAX,
            witness,
        })
    }
}

pub type Output = Adapter<bitcoin::TxOut>;

#[derive(State, Call, Query, Client, Debug)]
pub struct Checkpoint {
    pub status: CheckpointStatus,
    pub inputs: Deque<Input>,
    signed_inputs: u16,
    pub outputs: Deque<Output>,
    pub sigset: SignatorySet,
}

impl Checkpoint {
    pub fn create_time(&self) -> u64 {
        self.sigset.create_time()
    }

    pub fn tx(&self) -> Result<(bitcoin::Transaction, u64)> {
        let mut tx = bitcoin::Transaction {
            version: 1,
            lock_time: 0,
            input: vec![],
            output: vec![],
        };

        let mut est_vsize = 0;

        // TODO: use deque iterator
        for i in 0..self.inputs.len() {
            let input = self.inputs.get(i)?.unwrap();
            tx.input.push(input.to_txin()?);
            est_vsize += input.sigs.est_vsize();
        }

        // TODO: use deque iterator
        for i in 0..self.outputs.len() {
            let output = self.outputs.get(i)?.unwrap();
            tx.output.push((**output).clone());
        }

        est_vsize += tx.get_size() as u64;

        Ok((tx, est_vsize))
    }
}

#[derive(State, Call, Query, Client)]
pub struct CheckpointQueue {
    queue: Deque<Checkpoint>,
    index: u32,
}

#[derive(Deref)]
pub struct CompletedCheckpoint<'a>(Ref<'a, Checkpoint>);

#[derive(Deref, Debug)]
pub struct SigningCheckpoint<'a>(Ref<'a, Checkpoint>);

impl<'a, U: Clone> Client<U> for SigningCheckpoint<'a> {
    type Client = ();

    fn create_client(_: U) {}
}

impl<'a> Query for SigningCheckpoint<'a> {
    type Query = ();

    fn query(&self, _: ()) -> OrgaResult<()> {
        Ok(())
    }
}

impl<'a> SigningCheckpoint<'a> {
    #[query]
    pub fn to_sign(&self, xpub: Xpub) -> Result<Vec<([u8; 32], u32)>> {
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();

        let mut msgs = vec![];

        for i in 0..self.inputs.len() {
            let input = self.inputs.get(i)?.unwrap();
            let pubkey = xpub
                .derive_pub(
                    &secp,
                    &[bitcoin::util::bip32::ChildNumber::from_normal_idx(
                        input.sigset_index,
                    )?],
                )?
                .public_key;
            if input.sigs.needs_sig(pubkey.into())? {
                msgs.push((input.sigs.message(), input.sigset_index));
            }
        }

        Ok(msgs)
    }
}

#[derive(Deref, DerefMut)]
pub struct SigningCheckpointMut<'a>(ChildMut<'a, u64, Checkpoint>);

impl<'a> SigningCheckpointMut<'a> {
    pub fn sign(&mut self, xpub: Xpub, sigs: LengthVec<u16, Signature>) -> Result<()> {
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();

        let mut sig_index = 0;
        for i in 0..self.inputs.len() {
            let mut input = self.inputs.get_mut(i)?.unwrap();

            let pubkey = xpub
                .derive_pub(
                    &secp,
                    &[bitcoin::util::bip32::ChildNumber::from_normal_idx(
                        input.sigset_index,
                    )?],
                )?
                .public_key
                .into();

            if !input.sigs.contains_key(pubkey)? {
                continue;
            }

            if input.sigs.done() {
                sig_index += 1;
                continue;
            }

            if sig_index > sigs.len() {
                return Err(OrgaError::App("Not enough signatures supplied".to_string()).into());
            }

            let sig = sigs[sig_index];
            sig_index += 1;

            input.sigs.sign(pubkey, sig)?;

            if input.sigs.done() {
                self.signed_inputs += 1;
            }
        }

        if sig_index != sigs.len() {
            return Err(OrgaError::App("Excess signatures supplied".to_string()).into());
        }

        Ok(())
    }

    pub fn done(&self) -> bool {
        self.signed_inputs as u64 == self.inputs.len()
    }

    pub fn advance(self) -> Result<()> {
        let mut checkpoint = self.0;

        checkpoint.status = CheckpointStatus::Complete;

        Ok(())
    }
}

#[derive(Deref)]
pub struct BuildingCheckpoint<'a>(Ref<'a, Checkpoint>);

#[derive(Deref, DerefMut)]
pub struct BuildingCheckpointMut<'a>(ChildMut<'a, u64, Checkpoint>);

impl<'a> BuildingCheckpointMut<'a> {
    pub fn push_input(
        &mut self,
        prevout: bitcoin::OutPoint,
        sigset: &SignatorySet,
        dest: Address,
        amount: u64,
    ) -> Result<()> {
        let script_pubkey = sigset.output_script(dest)?;
        let redeem_script = sigset.redeem_script(dest)?;

        // TODO: need a better way to initialize state types from values?
        self.inputs.push_back((
            Adapter::new(prevout),
            Adapter::new(script_pubkey),
            Adapter::new(redeem_script),
            sigset.index(),
            dest.into(),
            amount,
            <ThresholdSig as State>::Encoding::default(),
        ))?;

        let inputs_len = self.inputs.len();
        let mut input = self.inputs.get_mut(inputs_len - 1)?.unwrap();
        input.sigs.from_sigset(sigset)?;

        Ok(())
    }

    pub fn advance(self) -> Result<SigningCheckpointMut<'a>> {
        let mut checkpoint = self.0;

        checkpoint.status = CheckpointStatus::Signing;

        let reserve_out = bitcoin::TxOut {
            value: 0, // will be updated after counting ins/outs and fees
            script_pubkey: checkpoint.sigset.output_script(Address::NULL)?,
        };
        checkpoint.outputs.push_front(Adapter::new(reserve_out))?;

        for i in MAX_INPUTS..checkpoint.inputs.len() {
            // TODO: move input to child
            todo!()
        }

        for i in MAX_OUTPUTS..checkpoint.outputs.len() {
            // TODO: move output to child
            todo!()
        }

        let mut in_amount = 0;
        for i in 0..checkpoint.inputs.len() {
            let input = checkpoint.inputs.get(i)?.unwrap();
            in_amount += input.amount;
        }

        let mut out_amount = 0;
        for i in 0..checkpoint.outputs.len() {
            let output = checkpoint.outputs.get(i)?.unwrap();
            out_amount += output.value;
        }

        let mut signing = SigningCheckpointMut(checkpoint);

        let (mut tx, est_vsize) = signing.tx()?;
        let fee = est_vsize * FEE_RATE;
        let reserve_value = in_amount - out_amount - fee;
        let mut reserve_out = signing.outputs.get_mut(0)?.unwrap();
        reserve_out.value = reserve_value;
        tx.output[0].value = reserve_value;

        let mut sc = bitcoin::util::bip143::SigHashCache::new(&tx);
        for i in 0..signing.inputs.len() {
            let mut input = signing.inputs.get_mut(i)?.unwrap();
            let sighash_type = bitcoin::SigHashType::All;
            let sighash = sc.signature_hash(i as usize, &input.redeem_script, input.amount, sighash_type);
            input.sigs.set_message(sighash.into_inner());
        }

        Ok(signing)
    }
}

impl CheckpointQueue {
    #[query]
    pub fn get(&self, index: u32) -> Result<Ref<'_, Checkpoint>> {
        let index = self.get_deque_index(index)?;
        Ok(self.queue.get(index as u64)?.unwrap())
    }

    pub fn get_mut(&mut self, index: u32) -> Result<ChildMut<'_, u64, Checkpoint>> {
        let index = self.get_deque_index(index)?;
        Ok(self.queue.get_mut(index as u64)?.unwrap())
    }

    fn get_deque_index(&self, index: u32) -> Result<u32> {
        let start = self.index + 1 - (self.queue.len() as u32);
        if index > self.index || index < start {
            Err(OrgaError::App("Index out of bounds".to_string()).into())
        } else {
            Ok(index - start)
        }
    }

    #[query]
    pub fn index(&self) -> u32 {
        self.index
    }

    #[query]
    pub fn all(&self) -> Result<Vec<(u32, Ref<'_, Checkpoint>)>> {
        // TODO: return iterator
        // TODO: use Deque iterator

        let mut out = Vec::with_capacity(self.queue.len() as usize);

        for i in 0..self.queue.len() {
            let index = self.index - (i as u32);
            let checkpoint = self.queue.get(index as u64)?.unwrap();
            out.push((index, checkpoint));
        }

        Ok(out)
    }

    #[query]
    pub fn completed(&self) -> Result<Vec<CompletedCheckpoint<'_>>> {
        // TODO: return iterator
        // TODO: use Deque iterator

        let mut out = vec![];

        for i in 0..self.queue.len() {
            let checkpoint = self.queue.get(i)?.unwrap();

            if !matches!(checkpoint.status, CheckpointStatus::Complete) {
                break;
            }

            out.push(CompletedCheckpoint(checkpoint));
        }

        Ok(out)
    }

    #[query]
    pub fn completed_txs(&self) -> Result<Vec<Adapter<bitcoin::Transaction>>> {
        self.completed()?
            .into_iter()
            .map(|c| Ok(Adapter::new(c.tx()?.0)))
            .collect()
    }

    #[query]
    pub fn signing(&self) -> Result<Option<SigningCheckpoint<'_>>> {
        if self.queue.len() < 2 {
            return Ok(None);
        }

        let second = self.get(self.index - 1)?;
        if !matches!(second.status, CheckpointStatus::Signing) {
            return Ok(None);
        }

        Ok(Some(SigningCheckpoint(second)))
    }

    pub fn signing_mut(&mut self) -> Result<Option<SigningCheckpointMut>> {
        if self.queue.len() < 2 {
            return Ok(None);
        }

        let second = self.get_mut(self.index - 1)?;
        if !matches!(second.status, CheckpointStatus::Signing) {
            return Ok(None);
        }

        Ok(Some(SigningCheckpointMut(second)))
    }

    pub fn building(&self) -> Result<BuildingCheckpoint> {
        let last = self.get(self.index)?;
        Ok(BuildingCheckpoint(last))
    }

    pub fn building_mut(&mut self) -> Result<BuildingCheckpointMut> {
        let last = self.get_mut(self.index)?;
        Ok(BuildingCheckpointMut(last))
    }

    pub fn maybe_step(&mut self, sig_keys: &Map<ConsensusKey, Xpub>) -> Result<()> {
        #[cfg(not(feature = "full"))]
        unimplemented!();

        #[cfg(feature = "full")]
        {
            if self.signing()?.is_some() {
                return Ok(());
            }

            if !self.queue.is_empty() {
                let now = self
                    .context::<Time>()
                    .ok_or_else(|| OrgaError::App("No time context".to_string()))?
                    .seconds as u64;
                let elapsed = now - self.building()?.create_time();
                if elapsed < CHECKPOINT_INTERVAL {
                    return Ok(());
                }

                let building = self.building()?;
                let has_pending_deposit = if self.index == 0 {
                    building.inputs.len() > 0
                } else {
                    building.inputs.len() > 1
                };

                let has_pending_withdrawal = building.outputs.len() > 0;

                if !has_pending_deposit && !has_pending_withdrawal {
                    return Ok(());
                }
            }

            if self.maybe_push(sig_keys)?.is_none() {
                return Ok(());
            }

            if self.index > 0 {
                let second = self.get_mut(self.index - 1)?;
                BuildingCheckpointMut(second).advance()?;

                // TODO: do this inside advance()?
                let signing = self.signing()?.unwrap();
                let reserve_value = signing.outputs.front()?.unwrap().value;
                let (signing_tx, _) = signing.tx()?;
                let outpoint = bitcoin::OutPoint {
                    txid: signing_tx.txid(),
                    vout: 0,
                };
                let sigset = signing.sigset.clone();

                let mut building = self.building_mut()?;
                building.push_input(
                    outpoint, &sigset,
                    Address::NULL,
                    reserve_value,
                )?;
            }

            Ok(())
        }
    }

    fn maybe_push(
        &mut self,
        sig_keys: &Map<ConsensusKey, Xpub>,
    ) -> Result<Option<BuildingCheckpointMut>> {
        #[cfg(not(feature = "full"))]
        unimplemented!();

        #[cfg(feature = "full")]
        {
            let mut index = self.index;
            if !self.queue.is_empty() {
                index += 1;
            }

            let sigset = SignatorySet::from_validator_ctx(index, sig_keys)?;

            if sigset.possible_vp() == 0 {
                return Ok(None);
            }

            if !sigset.has_quorum() {
                return Ok(None);
            }

            self.index = index;
            self.queue.push_back(Default::default())?;
            let mut building = self.building_mut()?;

            building.sigset = sigset;

            Ok(Some(building))
        }
    }

    #[query]
    pub fn active_sigset(&self) -> Result<SignatorySet> {
        Ok(self.building()?.sigset.clone())
    }

    #[call]
    pub fn sign(&mut self, xpub: Xpub, sigs: LengthVec<u16, Signature>) -> Result<()> {
        let mut signing = self
            .signing_mut()?
            .ok_or_else(|| Error::Orga(OrgaError::App("No checkpoint to be signed".to_string())))?;

        signing.sign(xpub, sigs)?;

        if signing.done() {
            println!("done. {:?}", signing.tx()?);
            signing.advance()?;
        }

        Ok(())
    }

    #[query]
    pub fn to_sign(&self, xpub: Xpub) -> Result<Vec<([u8; 32], u32)>> {
        self.signing()?
            .ok_or_else(|| OrgaError::App("No checkpoint to be signed".to_string()))?
            .to_sign(xpub)
    }

    pub fn sigset(&self, index: u32) -> Result<SignatorySet> {
        Ok(self.get(index)?.sigset.clone())
    }
}
