use std::{cell::RefCell, rc::Rc, time::Duration};

use miden_lib::{
    accounts::faucets::create_basic_fungible_faucet, notes::create_p2id_note, AuthScheme,
};
use miden_node_proto::generated::{
    requests::{GetBlockHeaderByNumberRequest, SubmitProvenTransactionRequest},
    rpc::api_client::ApiClient,
};
use miden_objects::{
    accounts::{Account, AccountDelta, AccountId, AccountStorageType, AuthSecretKey},
    assembly::{ModuleAst, ProgramAst},
    assets::{FungibleAsset, TokenSymbol},
    crypto::{
        dsa::rpo_falcon512::{self, Polynomial, SecretKey},
        merkle::{MmrPeaks, PartialMmr},
        rand::RpoRandomCoin,
    },
    notes::{Note, NoteId, NoteType},
    transaction::{ChainMmr, ExecutedTransaction, InputNotes, TransactionArgs},
    vm::AdviceMap,
    BlockHeader, Felt, Word,
};
use miden_tx::{
    utils::Serializable, AuthenticationError, DataStore, DataStoreError, ProvingOptions,
    TransactionAuthenticator, TransactionExecutor, TransactionInputs, TransactionProver,
};
use rand::{thread_rng, Rng};
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use tonic::transport::Channel;

use crate::{config::FaucetConfig, errors::FaucetError};

pub const DISTRIBUTE_FUNGIBLE_ASSET_SCRIPT: &str =
    include_str!("transaction_scripts/distribute_fungible_asset.masm");

pub struct FaucetClient {
    rpc_api: ApiClient<Channel>,
    executor: TransactionExecutor<FaucetDataStore, FaucetAuthenticator>,
    data_store: FaucetDataStore,
    id: AccountId,
    rng: RpoRandomCoin,
}

unsafe impl Send for FaucetClient {}
unsafe impl Sync for FaucetClient {}

impl FaucetClient {
    pub async fn new(config: FaucetConfig) -> Result<Self, FaucetError> {
        let (rpc_api, root_block_header, root_chain_mmr) =
            initialize_faucet_client(config.clone()).await?;

        let (faucet_account, account_seed, secret) = build_account(config.clone())?;
        let faucet_account = Rc::new(RefCell::new(faucet_account));
        let id = faucet_account.borrow().id();

        let data_store = FaucetDataStore::new(
            faucet_account.clone(),
            account_seed,
            root_block_header,
            root_chain_mmr,
        );
        let authenticator = FaucetAuthenticator::new(secret);
        let mut executor =
            TransactionExecutor::new(data_store.clone(), Some(Rc::new(authenticator)));

        executor
            .load_account(id)
            .map_err(|err| FaucetError::InternalServerError(err.to_string()))?;

        let mut rng = thread_rng();
        let coin_seed: [u64; 4] = rng.gen();
        let rng = RpoRandomCoin::new(coin_seed.map(Felt::new));
        Ok(Self { data_store, rpc_api, executor, id, rng })
    }

    pub fn execute_mint_transaction(
        &mut self,
        target_account_id: AccountId,
        is_private_note: bool,
        asset_amount: u64,
    ) -> Result<(ExecutedTransaction, Note), FaucetError> {
        let asset = FungibleAsset::new(self.id, asset_amount)
            .map_err(|err| FaucetError::InternalServerError(err.to_string()))?;

        let note_type = if is_private_note {
            NoteType::OffChain
        } else {
            NoteType::Public
        };

        let output_note =
            create_p2id_note(self.id, target_account_id, vec![asset.into()], note_type, self.rng)
                .map_err(|err| FaucetError::InternalServerError(err.to_string()))?;

        let transaction_args =
            build_transaction_arguments(&output_note, &self.executor, note_type, asset)?;

        let executed_tx = self
            .executor
            .execute_transaction(self.id, 0, &[], transaction_args)
            .map_err(|err| {
                FaucetError::InternalServerError(format!("Failed to execute transaction: {}", err))
            })?;

        Ok((executed_tx, output_note))
    }

    pub async fn prove_and_submit_transaction(
        &mut self,
        executed_tx: ExecutedTransaction,
    ) -> Result<(), FaucetError> {
        let transaction_prover = TransactionProver::new(ProvingOptions::default());

        let delta = executed_tx.account_delta().clone();

        let proven_transaction =
            transaction_prover.prove_transaction(executed_tx).map_err(|err| {
                FaucetError::InternalServerError(format!("Failed to prove transaction: {}", err))
            })?;

        let request = SubmitProvenTransactionRequest {
            transaction: proven_transaction.to_bytes(),
        };

        self.rpc_api
            .submit_proven_transaction(request)
            .await
            .map_err(|err| FaucetError::InternalServerError(err.to_string()))?;

        self.data_store.update_faucet_account(&delta).map_err(|err| {
            FaucetError::InternalServerError(format!("Failed to update account: {}", err))
        })?;

        Ok(())
    }

    pub fn get_faucet_id(&self) -> AccountId {
        self.id
    }
}

#[derive(Clone)]
pub struct FaucetDataStore {
    faucet_account: Rc<RefCell<Account>>,
    seed: Word,
    block_header: BlockHeader,
    chain_mmr: ChainMmr,
}

impl FaucetDataStore {
    pub fn new(
        faucet_account: Rc<RefCell<Account>>,
        seed: Word,
        root_block_header: BlockHeader,
        root_chain_mmr: ChainMmr,
    ) -> Self {
        Self {
            faucet_account,
            seed,
            block_header: root_block_header,
            chain_mmr: root_chain_mmr,
        }
    }

    fn update_faucet_account(&mut self, delta: &AccountDelta) -> Result<(), FaucetError> {
        self.faucet_account
            .borrow_mut()
            .apply_delta(delta)
            .map_err(|err| FaucetError::InternalServerError(err.to_string()))
    }
}

impl DataStore for FaucetDataStore {
    fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        _block_ref: u32,
        _notes: &[NoteId],
    ) -> Result<TransactionInputs, DataStoreError> {
        let account = self.faucet_account.borrow();
        if account_id != account.id() {
            return Err(DataStoreError::AccountNotFound(account_id));
        }

        let empty_input_notes =
            InputNotes::new(Vec::new()).map_err(DataStoreError::InvalidTransactionInput)?;

        TransactionInputs::new(
            account.clone(),
            account.is_new().then_some(self.seed),
            self.block_header,
            self.chain_mmr.clone(),
            empty_input_notes,
        )
        .map_err(DataStoreError::InvalidTransactionInput)
    }

    fn get_account_code(&self, account_id: AccountId) -> Result<ModuleAst, DataStoreError> {
        let account = self.faucet_account.borrow();
        if account_id != account.id() {
            return Err(DataStoreError::AccountNotFound(account_id));
        }

        let module_ast = account.code().module().clone();
        Ok(module_ast)
    }
}

struct FaucetAuthenticator {
    secret_key: AuthSecretKey,
    rng: RefCell<ChaCha20Rng>,
}

impl FaucetAuthenticator {
    pub fn new(secret_key: SecretKey) -> Self {
        Self {
            secret_key: AuthSecretKey::RpoFalcon512(secret_key),
            rng: RefCell::new(ChaCha20Rng::from_entropy()),
        }
    }
}

impl TransactionAuthenticator for FaucetAuthenticator {
    fn get_signature(
        &self,
        _pub_key: Word,
        message: Word,
        _account_delta: &AccountDelta,
    ) -> Result<Vec<Felt>, AuthenticationError> {
        let mut rng = self.rng.borrow_mut();
        let AuthSecretKey::RpoFalcon512(k) = &self.secret_key;
        get_falcon_signature(k, message, &mut rng)
    }
}

// HELPER FUNCTIONS
// ================================================================================================

fn build_account(config: FaucetConfig) -> Result<(Account, Word, SecretKey), FaucetError> {
    let token_symbol = TokenSymbol::new(config.token_symbol.as_str())
        .map_err(|err| FaucetError::AccountCreationError(err.to_string()))?;

    let seed: [u8; 32] = [0; 32];

    // Instantiate keypair and authscheme
    let mut rng = ChaCha20Rng::from_seed(seed);
    let secret = SecretKey::with_rng(&mut rng);
    let auth_scheme = AuthScheme::RpoFalcon512 { pub_key: secret.public_key() };

    let (faucet_account, account_seed) = create_basic_fungible_faucet(
        seed,
        token_symbol,
        config.decimals,
        Felt::try_from(config.max_supply)
            .map_err(|err| FaucetError::InternalServerError(err.to_string()))?,
        AccountStorageType::OffChain,
        auth_scheme,
    )
    .map_err(|err| FaucetError::AccountCreationError(err.to_string()))?;

    Ok((faucet_account, account_seed, secret))
}

pub async fn initialize_faucet_client(
    config: FaucetConfig,
) -> Result<(ApiClient<Channel>, BlockHeader, ChainMmr), FaucetError> {
    let endpoint = tonic::transport::Endpoint::try_from(config.node_url.clone())
        .map_err(|_| FaucetError::InternalServerError("Failed to connect to node.".to_string()))?
        .timeout(Duration::from_millis(config.timeout_ms));

    let mut rpc_api = ApiClient::connect(endpoint)
        .await
        .map_err(|err| FaucetError::InternalServerError(err.to_string()))?;

    let request = GetBlockHeaderByNumberRequest {
        block_num: Some(0),
        include_mmr_proof: Some(true),
    };
    let response = rpc_api.get_block_header_by_number(request).await.map_err(|err| {
        FaucetError::InternalServerError(format!("Failed to get block header: {}", err))
    })?;
    let root_block_header: BlockHeader =
        response.into_inner().block_header.unwrap().try_into().map_err(|err| {
            FaucetError::InternalServerError(format!("Failed to convert block header: {}", err))
        })?;

    let root_chain_mmr = ChainMmr::new(
        PartialMmr::from_peaks(
            MmrPeaks::new(0, Vec::new()).expect("Empty MmrPeak should be valid"),
        ),
        Vec::new(),
    )
    .expect("Empty ChainMmr should be valid");

    Ok((rpc_api, root_block_header, root_chain_mmr))
}

fn build_transaction_arguments(
    output_note: &Note,
    executor: &TransactionExecutor<FaucetDataStore, FaucetAuthenticator>,
    note_type: NoteType,
    asset: FungibleAsset,
) -> Result<TransactionArgs, FaucetError> {
    let recipient = output_note
        .recipient()
        .digest()
        .iter()
        .map(|x| x.as_int().to_string())
        .collect::<Vec<_>>()
        .join(".");

    let tag = output_note.metadata().tag().inner();

    let script = ProgramAst::parse(
        &DISTRIBUTE_FUNGIBLE_ASSET_SCRIPT
            .replace("{recipient}", &recipient)
            .replace("{note_type}", &Felt::new(note_type as u64).to_string())
            .replace("{tag}", &Felt::new(tag.into()).to_string())
            .replace("{amount}", &Felt::new(asset.amount()).to_string()),
    )
    .expect("shipped MASM is well-formed");

    let script = executor.compile_tx_script(script, vec![], vec![]).map_err(|err| {
        FaucetError::InternalServerError(format!("Failed to compile script: {}", err))
    })?;

    let mut transaction_args = TransactionArgs::new(Some(script), None, AdviceMap::new());
    transaction_args.extend_expected_output_notes(vec![output_note.clone()]);

    Ok(transaction_args)
}

// TODO: Remove the falcon signature function once it's available on base and made public

/// Retrieves a falcon signature over a message.
/// Gets as input a [Word] containing a secret key, and a [Word] representing a message and
/// outputs a vector of values to be pushed onto the advice stack.
/// The values are the ones required for a Falcon signature verification inside the VM and they are:
///
/// 1. The nonce represented as 8 field elements.
/// 2. The expanded public key represented as the coefficients of a polynomial of degree < 512.
/// 3. The signature represented as the coefficients of a polynomial of degree < 512.
/// 4. The product of the above two polynomials in the ring of polynomials with coefficients
/// in the Miden field.
///
/// # Errors
/// Will return an error if either:
/// - The secret key is malformed due to either incorrect length or failed decoding.
/// - The signature generation failed.
///
/// TODO: once this gets made public in miden base, remve this implementation and use the one from
/// base
fn get_falcon_signature(
    key: &rpo_falcon512::SecretKey,
    message: Word,
    rng: &mut ChaCha20Rng,
) -> Result<Vec<Felt>, AuthenticationError> {
    // Generate the signature
    let sig = key.sign_with_rng(message, rng);
    // The signature is composed of a nonce and a polynomial s2
    // The nonce is represented as 8 field elements.
    let nonce = sig.nonce();
    // We convert the signature to a polynomial
    let s2 = sig.sig_poly();
    // We also need in the VM the expanded key corresponding to the public key the was provided
    // via the operand stack
    let h = key.compute_pub_key_poly().0;
    // Lastly, for the probabilistic product routine that is part of the verification procedure,
    // we need to compute the product of the expanded key and the signature polynomial in
    // the ring of polynomials with coefficients in the Miden field.
    let pi = Polynomial::mul_modulo_p(&h, s2);
    // We now push the nonce, the expanded key, the signature polynomial, and the product of the
    // expanded key and the signature polynomial to the advice stack.
    let mut result: Vec<Felt> = nonce.to_elements().to_vec();

    result.extend(h.coefficients.iter().map(|a| Felt::from(a.value() as u32)));
    result.extend(s2.coefficients.iter().map(|a| Felt::from(a.value() as u32)));
    result.extend(pi.iter().map(|a| Felt::new(*a)));
    result.reverse();
    Ok(result)
}
