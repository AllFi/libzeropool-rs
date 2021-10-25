use std::collections::HashMap;
use std::{cell::RefCell, convert::TryInto};
use std::rc::Rc;

use js_sys::{Array, Promise};
use kvdb_web::Database;
use libzeropool::{
    constants::{self, OUTPLUSONELOG},
    fawkes_crypto::{
        core::sizedvec::SizedVec,
        ff_uint::Num,
        ff_uint::{NumRepr, Uint},
    },
    native::{
        account::Account as NativeAccount,
        boundednum::BoundedNum,
        note::Note as NativeNote,
        tx::{parse_delta, TransferPub as NativeTransferPub, TransferSec as NativeTransferSec},
    },
};
use libzeropool_rs::{
    client::{TxOutput, TxType as NativeTxType, UserAccount as NativeUserAccount},
    libzeropool::fawkes_crypto::borsh::BorshDeserialize,
    merkle::Hash,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::future_to_promise;

use crate::IndexedNote;
use crate::ts_types::Hash as JsHash;
use crate::{
    keys::reduce_sk, Account, Fr, Fs, Hashes, IndexedNotes, MerkleProof, Note, Pair, PoolParams,
    TxOutputs, UserState, POOL_PARAMS,
};

// TODO: Find a way to expose MerkleTree,

#[wasm_bindgen]
pub struct UserAccount {
    inner: Rc<RefCell<NativeUserAccount<Database, PoolParams>>>,
}

#[wasm_bindgen]
pub enum TxType {
    Transfer = "transfer",
    Deposit = "deposit",
    Withdraw = "withdraw",
}

#[wasm_bindgen]
impl UserAccount {
    #[wasm_bindgen(constructor)]
    /// Initializes UserAccount with a spending key that has to be an element of the prime field Fs (p = 6554484396890773809930967563523245729705921265872317281365359162392183254199).
    pub fn new(sk: &[u8], state: UserState) -> Result<UserAccount, JsValue> {
        crate::utils::set_panic_hook();

        let sk = Num::<Fs>::from_uint(NumRepr(Uint::from_little_endian(sk)))
            .ok_or_else(|| js_err!("Invalid spending key"))?;
        let account = NativeUserAccount::new(sk, state.inner, POOL_PARAMS.clone());

        Ok(UserAccount {
            inner: Rc::new(RefCell::new(account)),
        })
    }

    // TODO: Is this safe?
    #[wasm_bindgen(js_name = fromSeed)]
    /// Same as constructor but accepts arbitrary data as spending key.
    pub fn from_seed(seed: &[u8], state: UserState) -> Result<UserAccount, JsValue> {
        let sk = reduce_sk(seed);
        Self::new(&sk, state)
    }

    #[wasm_bindgen(js_name = generateAddress)]
    /// Generates a new private address.
    pub fn generate_address(&self) -> String {
        self.inner.borrow().generate_address()
    }

    #[wasm_bindgen(js_name = decryptNotes)]
    /// Attempts to decrypt notes.
    pub fn decrypt_notes(&self, data: Vec<u8>) -> Result<IndexedNotes, JsValue> {
        let notes = self
            .inner
            .borrow()
            .decrypt_notes(data)
            .into_iter()
            .enumerate()
            .filter_map(|(index, note)| {
                let note = IndexedNote {
                    index: index as u64,
                    note: note?,
                };

                Some(serde_wasm_bindgen::to_value(&note).unwrap())
            })
            .collect::<Array>()
            .unchecked_into::<IndexedNotes>();

        Ok(notes)
    }

    #[wasm_bindgen(js_name = decryptPair)]
    /// Attempts to decrypt account and notes.
    pub fn decrypt_pair(&self, data: Vec<u8>) -> Result<Option<Pair>, JsValue> {
        #[derive(Serialize)]
        struct SerPair {
            account: NativeAccount<Fr>,
            notes: Vec<NativeNote<Fr>>,
        }

        let pair = self
            .inner
            .borrow()
            .decrypt_pair(data)
            .map(|(account, notes)| {
                let pair = SerPair { account, notes };

                serde_wasm_bindgen::to_value(&pair)
                    .unwrap()
                    .unchecked_into::<Pair>()
            });

        Ok(pair)
    }

    #[wasm_bindgen(js_name = "createTx")]
    /// Constructs a transaction.
    pub fn create_tx(
        &self,
        ty: TxType,
        value: TxOutputs,
        data: Option<Vec<u8>>,
        delta_index: Option<u64>,
    ) -> Promise {
        #[derive(Deserialize)]
        struct Output {
            to: String,
            amount: BoundedNum<Fr, { constants::BALANCE_SIZE_BITS }>,
        }

        #[derive(Serialize)]
        struct ParsedDelta {
            v: i64,
            e: i64,
            index: u64,
        }

        #[derive(Serialize)]
        struct TransactionData {
            public: NativeTransferPub<Fr>,
            secret: NativeTransferSec<Fr>,
            #[serde(with = "hex")]
            ciphertext: Vec<u8>,
            #[serde(with = "hex")]
            memo: Vec<u8>,
            commitment_root: Num<Fr>,
            out_hashes: SizedVec<Num<Fr>, { constants::OUT + 1 }>,
            parsed_delta: ParsedDelta,
        }

        // TODO: Signature callback

        let account = self.inner.clone();

        future_to_promise(async move {
            let tx = match ty {
                TxType::Transfer => {
                    let js_outputs: JsValue = value.into();
                    let outputs = serde_wasm_bindgen::from_value::<Vec<Output>>(js_outputs)?
                        .into_iter()
                        .map(|out| TxOutput {
                            to: out.to,
                            amount: out.amount,
                        })
                        .collect::<Vec<_>>();

                    account
                        .borrow()
                        .create_tx(NativeTxType::Transfer(outputs), data, delta_index)
                }
                TxType::Deposit => {
                    let js_amount: JsValue = value.into();
                    let amount = serde_wasm_bindgen::from_value(js_amount)?;

                    account
                        .borrow()
                        .create_tx(NativeTxType::Deposit(amount), data, delta_index)
                }
                TxType::Withdraw => {
                    let js_amount: JsValue = value.into();
                    let amount = serde_wasm_bindgen::from_value(js_amount)?;

                    account
                        .borrow()
                        .create_tx(NativeTxType::Withdraw(amount), data, delta_index)
                }
                _ => panic!("unknow tx type"),
            }
            .map_err(|err| js_err!("{}", err))?;

            let (v, e, index) = parse_delta(tx.public.delta);
            let parsed_delta = ParsedDelta {
                v: v.try_into().unwrap(),
                e: e.try_into().unwrap(),
                index: index.try_into().unwrap(),
            };

            let tx = TransactionData {
                public: tx.public,
                secret: tx.secret,
                ciphertext: tx.ciphertext,
                memo: tx.memo,
                out_hashes: tx.out_hashes,
                commitment_root: tx.commitment_root,
                parsed_delta,
            };

            Ok(serde_wasm_bindgen::to_value(&tx).unwrap())
        })
    }

    #[wasm_bindgen(js_name = "addAccount")]
    /// Cache account at specified index.
    pub fn add_account(
        &mut self,
        at_index: u64,
        hashes: Hashes,
        account: Account,
        notes: IndexedNotes,
    ) -> Result<(), JsValue> {
        let account = serde_wasm_bindgen::from_value(account.into())?;
        let hashes: Vec<_> = serde_wasm_bindgen::from_value(hashes.unchecked_into())?;
        let notes: Vec<_> =
            serde_wasm_bindgen::from_value::<Vec<IndexedNote>>(notes.unchecked_into())?
                .into_iter()
                .map(|note| (note.index, note.note))
                .collect();

        self.inner
            .borrow_mut()
            .state
            .add_own_tx(at_index, &hashes, account, &notes);

        Ok(())
    }

    #[wasm_bindgen(js_name = "addHashes")]
    pub fn add_hashes(&mut self, at_index: u64, hashes: Hashes) -> Result<(), JsValue> {
        let hashes: Vec<_> = serde_wasm_bindgen::from_value(hashes.unchecked_into())?;

        self.inner.borrow_mut().state.add_hashes(at_index, &hashes);

        Ok(())
    }

    #[wasm_bindgen(js_name = "addNotes")]
    pub fn add_notes(
        &mut self,
        at_index: u64,
        hashes: Hashes,
        notes: IndexedNotes,
    ) -> Result<(), JsValue> {
        let hashes: Vec<_> = serde_wasm_bindgen::from_value(hashes.unchecked_into())?;
        let notes: Vec<_> =
            serde_wasm_bindgen::from_value::<Vec<IndexedNote>>(notes.unchecked_into())?
                .into_iter()
                .map(|note| (note.index, note.note))
                .collect();

        self.inner
            .borrow_mut()
            .state
            .add_notes(at_index, &hashes, &notes);

        Ok(())
    }

    #[wasm_bindgen(js_name = "getRoot")]
    pub fn get_root(&mut self) -> String {
        let root = self.inner.borrow_mut().state.tree.get_root().to_string();

        root
    }

    #[wasm_bindgen(js_name = "addReceivedNote")]
    /// Caches a note at specified index.
    /// Only cache received notes.
    pub fn add_received_note(&mut self, at_index: u64, note: Note) -> Result<(), JsValue> {
        let note = serde_wasm_bindgen::from_value(note.into())?;
        self.inner.borrow_mut().state.add_received_note(at_index, note);

        Ok(())
    }

    #[wasm_bindgen(js_name = "addCommitment")]
    /// Add out commitment hash to the tree.
    pub fn add_commitment(&mut self, index: u64, commitment: Vec<u8>) -> Result<(), JsValue> {
        self.inner.borrow_mut().state.tree.add_hash_at_height(
            OUTPLUSONELOG as u32,
            index,
            Num::try_from_slice(commitment.as_slice()).unwrap(),
            false,
        );

        Ok(())
    }

    #[wasm_bindgen(js_name = "totalBalance")]
    /// Returns user's total balance (account + available notes).
    pub fn total_balance(&self) -> String {
        self.inner.borrow().state.total_balance().to_string()
    }

    #[wasm_bindgen(js_name = "nextTreeIndex")]
    pub fn next_tree_index(&self) -> u64 {
        self.inner.borrow().state.tree.next_index()
    }

    // TODO: Temporary method, try to expose the whole tree
    #[wasm_bindgen(js_name = "getLastLeaf")]
    pub fn get_last_leaf(&self) -> String {
        self.inner.borrow().state.tree.last_leaf().to_string()
    }

    #[wasm_bindgen(js_name = "getMerkleNode")]
    pub fn get_merkle_node(&self, height: u32, index: u64) -> String {
        let node = self
            .inner
            .borrow()
            .state
            .tree
            .get(height, index);

        node.to_string()
    }

    #[wasm_bindgen(js_name = "getMerkleProof")]
    /// Returns merkle proof for the specified index in the tree.
    pub fn get_merkle_proof(&self, index: u64) -> MerkleProof {
        let proof = self
            .inner
            .borrow()
            .state
            .tree
            .get_proof_unchecked::<{ constants::HEIGHT }>(index);

        serde_wasm_bindgen::to_value(&proof)
            .unwrap()
            .unchecked_into::<MerkleProof>()
    }

    // TODO: This is a temporary method
    #[wasm_bindgen(js_name = "getMerkleRootAfterCommitment")]
    pub fn get_merkle_root_after_commitment(
        &self,
        commitment_index: u64,
        commitment: JsHash,
    ) -> Result<String, JsValue> {
        let hash: Hash<Fr> = serde_wasm_bindgen::from_value(commitment.unchecked_into())?;
        let mut nodes = HashMap::new();
        nodes.insert((constants::OUTPLUSONELOG as u32, commitment_index), hash);

        let left_index = commitment_index * (2u64.pow(constants::OUTPLUSONELOG as u32));
        let node = self.inner.borrow().state.tree.get_virtual_node(
            constants::HEIGHT as u32,
            0,
            &mut nodes,
            left_index,
            left_index + constants::OUT as u64 + 1,
        );

        Ok(node.to_string())
    }

    #[wasm_bindgen(js_name = "getMerkleProofAfter")]
    /// Returns merkle proofs for the specified leafs (hashes) as if they were appended to the tree.
    pub fn get_merkle_proof_after(&self, hashes: Hashes) -> Result<Vec<MerkleProof>, JsValue> {
        let hashes: Vec<Hash<Fr>> = serde_wasm_bindgen::from_value(hashes.unchecked_into())?;

        let proofs = self
            .inner
            .borrow_mut()
            .state
            .tree
            .get_proof_after_virtual(hashes)
            .into_iter()
            .map(|proof| {
                serde_wasm_bindgen::to_value(&proof)
                    .unwrap()
                    .unchecked_into::<MerkleProof>()
            })
            .collect();

        Ok(proofs)
    }

    #[wasm_bindgen(js_name = "getCommitmentMerkleProof")]
    pub fn get_commitment_merkle_proof(&self, index: u64) -> MerkleProof {
        let proof = self
            .inner
            .borrow()
            .state
            .tree
            .get_proof_unchecked::<{ constants::HEIGHT - constants::OUTPLUSONELOG }>(index);

        serde_wasm_bindgen::to_value(&proof)
            .unwrap()
            .unchecked_into::<MerkleProof>()
    }
}
