use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Error, Context};
use serde::Serialize;
use tokio::sync::{
    broadcast::{Sender as BroadcastSender, Receiver as BroadcastReceiver},
    {Mutex, RwLock}
};
use xelis_common::{
    api::{
        DataElement,
        wallet::{FeeBuilder, NotifyEvent, BalanceChanged}
    },
    asset::AssetWithData,
    config::{XELIS_ASSET, COIN_DECIMALS},
    crypto::{
        address::{Address, AddressType},
        hash::Hash,
        key::{KeyPair, PublicKey, Signature},
    },
    utils::{format_xelis, format_coin},
    network::Network,
    serializer::{Serializer, Writer},
    transaction::{TransactionType, Transfer, Transaction, EXTRA_DATA_LIMIT_SIZE},
};
use crate::{
    cipher::Cipher,
    config::{PASSWORD_ALGORITHM, PASSWORD_HASH_SIZE, SALT_SIZE},
    entry::TransactionEntry,
    network_handler::{NetworkHandler, SharedNetworkHandler, NetworkError},
    storage::{EncryptedStorage, Storage},
    transaction_builder::TransactionBuilder,
    mnemonics,
};
use chacha20poly1305::{aead::OsRng, Error as CryptoError};
use rand::RngCore;
use thiserror::Error;
use log::{debug, error, trace};

#[cfg(feature = "api_server")]
use {
    serde_json::{json, Value},
    async_trait::async_trait,
    crate::api::{
        XSWDNodeMethodHandler,
        register_rpc_methods,
        XSWD,
        WalletRpcServer,
        AuthConfig,
        APIServer,
        AppStateShared,
        PermissionResult,
        PermissionRequest,
        XSWDPermissionHandler
    },
    xelis_common::rpc_server::{
        RPCHandler,
        RpcRequest,
        InternalRpcError,
        RpcResponseError,
        JSON_RPC_VERSION
    },
    tokio::sync::{
        mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel},
        oneshot::{Sender as OneshotSender, channel}
    }
};

#[derive(Error, Debug)]
pub enum WalletError {
    #[error("Transaction too big: {} bytes, max is {} bytes", _0, _1)]
    TransactionTooBig(usize, usize),
    #[error("Invalid key pair")]
    InvalidKeyPair,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Expected a TX")]
    ExpectedOneTx,
    #[error("Too many txs included max is {}", u8::MAX)]
    TooManyTx,
    #[error("Transaction owner is the receiver")]
    TxOwnerIsReceiver,
    #[error("Error from crypto: {}", _0)]
    CryptoError(CryptoError),
    #[error("Unexpected error on database: {}", _0)]
    DatabaseError(#[from] sled::Error),
    #[error("Invalid encrypted value: minimum 25 bytes")]
    InvalidEncryptedValue,
    #[error("No salt found in storage")]
    NoSalt,
    #[error("Error while hashing: {}", _0)]
    AlgorithmHashingError(String),
    #[error("Error while fetching encrypted master key from DB")]
    NoMasterKeyFound,
    #[error("Invalid salt size stored in storage, expected 32 bytes")]
    InvalidSaltSize,
    #[error("Error while fetching password salt from DB")]
    NoSaltFound,
    #[error("Your wallet contains only {} instead of {} for asset {}", format_coin(*_0, *_2), format_coin(*_1, *_2), _3)]
    NotEnoughFunds(u64, u64, u8, Hash),
    #[error("Your wallet don't have enough funds to pay fees: expected {} but have only {}", format_xelis(*_0), format_xelis(*_1))]
    NotEnoughFundsForFee(u64, u64),
    #[error("Invalid address params")]
    InvalidAddressParams,
    #[error("Invalid extra data in this transaction, expected maximum {} bytes but got {} bytes", _0, _1)]
    ExtraDataTooBig(usize, usize),
    #[error("Wallet is not in online mode")]
    NotOnlineMode,
    #[error("Wallet is already in online mode")]
    AlreadyOnlineMode,
    #[error("Asset is already present on disk")]
    AssetAlreadyRegistered,
    #[error("Topoheight is too high to rescan")]
    RescanTopoheightTooHigh,
    #[error(transparent)]
    Any(#[from] Error),
    #[error("No API Server is running")]
    NoAPIServer,
    #[error("RPC Server is not running")]
    RPCServerNotRunning,
    #[error("RPC Server is already running")]
    RPCServerAlreadyRunning,
    #[error("Invalid fees provided, minimum fees calculated: {}, provided: {}", format_xelis(*_0), format_xelis(*_1))]
    InvalidFeeProvided(u64, u64),
    #[error("Wallet name cannot be empty")]
    EmptyName,
    #[cfg(feature = "api_server")]
    #[error("No handler available for this request")]
    NoHandlerAvailable,
    #[error(transparent)]
    NetworkError(#[from] NetworkError),
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum Event {
    // When a TX is detected from daemon and is added in wallet storage
    NewTransaction(TransactionEntry),
    // When a new block is detected from daemon
    // NOTE: Same topoheight can be broadcasted several times if DAG reorg it
    // And some topoheight can be skipped because of DAG reorg
    // Example: two blocks at same height, both got same topoheight 69, next block reorg them together
    // and one of the block get topoheight 69, the other 70, next is 71, but 70 is skipped
    NewTopoHeight {
        topoheight: u64
    },
    // When a balance change occurs on wallet
    BalanceChanged(BalanceChanged),
    // When a new asset is added to wallet
    NewAsset(AssetWithData),
    // When a rescan happened (because of user request or DAG reorg/fork)
    // Value is topoheight until it deleted transactions
    // Next sync will restart at this topoheight
    Rescan {
        start_topoheight: u64   
    },
    // Wallet is now in online mode
    Online,
    // Wallet is now in offline mode
    Offline
}

impl Event {
    pub fn kind(&self) -> NotifyEvent {
        match self {
            Event::NewTransaction(_) => NotifyEvent::NewTransaction,
            Event::NewTopoHeight { .. } => NotifyEvent::NewTopoHeight,
            Event::BalanceChanged(_) => NotifyEvent::BalanceChanged,
            Event::NewAsset(_) => NotifyEvent::NewAsset,
            Event::Rescan { .. } => NotifyEvent::Rescan,
            Event::Online => NotifyEvent::Online,
            Event::Offline => NotifyEvent::Offline
        }
    }

}

pub struct Wallet {
    // Encrypted Wallet Storage
    storage: RwLock<EncryptedStorage>,
    // Private & Public key linked for this wallet
    keypair: KeyPair,
    // network handler for online mode to keep wallet synced
    network_handler: Mutex<Option<SharedNetworkHandler>>,
    // network on which we are connected
    network: Network,
    // RPC Server
    #[cfg(feature = "api_server")]
    api_server: Mutex<Option<APIServer<Arc<Self>>>>,
    // All XSWD requests are routed through this channel
    #[cfg(feature = "api_server")]
    xswd_channel: RwLock<Option<UnboundedSender<XSWDEvent>>>,
    // Event broadcaster
    event_broadcaster: Mutex<Option<BroadcastSender<Event>>>
}

pub fn hash_password(password: String, salt: &[u8]) -> Result<[u8; PASSWORD_HASH_SIZE], WalletError> {
    let mut output = [0; PASSWORD_HASH_SIZE];
    PASSWORD_ALGORITHM.hash_password_into(password.as_bytes(), salt, &mut output).map_err(|e| WalletError::AlgorithmHashingError(e.to_string()))?;
    Ok(output)
}

impl Wallet {
    // Create a new wallet with the specificed storage, keypair and its network
    fn new(storage: EncryptedStorage, keypair: KeyPair, network: Network) -> Arc<Self> {
        let zelf = Self {
            storage: RwLock::new(storage),
            keypair,
            network_handler: Mutex::new(None),
            network,
            #[cfg(feature = "api_server")]
            api_server: Mutex::new(None),
            #[cfg(feature = "api_server")]
            xswd_channel: RwLock::new(None),
            event_broadcaster: Mutex::new(None)
        };

        Arc::new(zelf)
    }

    // Create a new wallet on disk
    pub fn create(name: String, password: String, seed: Option<String>, network: Network) -> Result<Arc<Self>, Error> {
        if name.is_empty() {
            return Err(WalletError::EmptyName.into())
        }

        // generate random keypair or recover it from seed
        let keypair = if let Some(seed) = seed {
        debug!("Retrieving keypair from seed...");
        let words: Vec<String> = seed.split_whitespace().map(str::to_string).collect();
        let key = mnemonics::words_to_key(words)?;
            KeyPair::from_private_key(key)
        } else {
            debug!("Generating a new keypair...");
            KeyPair::new()
        };

        // generate random salt for hashed password
        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut salt);

        // generate hashed password which will be used as key to encrypt master_key
        debug!("hashing provided password");
        let hashed_password = hash_password(password, &salt)?;

        debug!("Creating storage for {}", name);
        let mut inner = Storage::new(name)?;

        // generate the Cipher
        let cipher = Cipher::new(&hashed_password, None)?;

        // save the salt used for password
        debug!("Save password salt in public storage");
        inner.set_password_salt(&salt)?;

        // generate the master key which is used for storage and then save it in encrypted form
        let mut master_key: [u8; 32] = [0; 32];
        OsRng.fill_bytes(&mut master_key);
        let encrypted_master_key = cipher.encrypt_value(&master_key)?;
        debug!("Save encrypted master key in public storage");
        inner.set_encrypted_master_key(&encrypted_master_key)?;
        
        // generate the storage salt and save it in encrypted form
        let mut storage_salt = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut storage_salt);
        let encrypted_storage_salt = cipher.encrypt_value(&storage_salt)?;
        inner.set_encrypted_storage_salt(&encrypted_storage_salt)?;

        debug!("Creating encrypted storage");
        let mut storage = EncryptedStorage::new(inner, &master_key, storage_salt, network)?;

        storage.set_keypair(&keypair)?;

        Ok(Self::new(storage, keypair, network))
    }

    // Open an existing wallet on disk
    pub fn open(name: String, password: String, network: Network) -> Result<Arc<Self>, Error> {
        if name.is_empty() {
            return Err(WalletError::EmptyName.into())
        }

        debug!("Creating storage for {}", name);
        let storage = Storage::new(name)?;
        
        // get password salt for KDF
        debug!("Retrieving password salt from public storage");
        let salt = storage.get_password_salt()?;

        // retrieve encrypted master key from storage
        debug!("Retrieving encrypted master key from public storage");
        let encrypted_master_key = storage.get_encrypted_master_key()?;

        let hashed_password = hash_password(password, &salt)?;

        // decrypt the encrypted master key using the hashed password (used as key)
        let cipher = Cipher::new(&hashed_password, None)?;
        let master_key = cipher.decrypt_value(&encrypted_master_key).context("Invalid password provided for this wallet")?;

        // Retrieve the encrypted storage salt
        let encrypted_storage_salt = storage.get_encrypted_storage_salt()?;
        let storage_salt = cipher.decrypt_value(&encrypted_storage_salt).context("Invalid encrypted storage salt for this wallet")?;
        if storage_salt.len() != SALT_SIZE {
            error!("Invalid size received after decrypting storage salt: {} bytes", storage_salt.len());
            return Err(WalletError::InvalidSaltSize.into());
        }

        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        salt.copy_from_slice(&storage_salt);

        debug!("Creating encrypted storage");
        let storage = EncryptedStorage::new(storage, &master_key, salt, network)?;
        debug!("Retrieving keypair from encrypted storage");
        let keypair =  storage.get_keypair()?;

        Ok(Self::new(storage, keypair, network))
    }

    // Close the wallet
    // this will stop the network handler and the API Server if it's running
    // Because wallet is behind Arc, we need to close differents modules that has a copy of it
    pub async fn close(&self) {
        trace!("Closing wallet");

        #[cfg(feature = "api_server")]
        {
            // Close API server
            {
                let mut lock = self.api_server.lock().await;
                if let Some(server) = lock.take() {
                    server.stop().await;
                }
            }

            // Close XSWD channel in case it exists
            {
                let mut lock = self.xswd_channel.write().await;
                if let Some(sender) = lock.take() {
                    drop(sender);
                }
            }
        }

        // Stop gracefully the network handler
        {
            let mut lock = self.network_handler.lock().await;
            if let Some(handler) = lock.take() {
                if let Err(e) = handler.stop().await {
                    error!("Error while stopping network handler: {}", e);
                }
            }
        }

        // Stop gracefully the storage
        {
            let mut storage = self.storage.write().await;
            storage.stop().await;
        }

        // Close the event broadcaster
        // So all subscribers will be notified
        self.close_events_channel().await;
    }

    // Propagate a new event to registered listeners
    pub async fn propagate_event(&self, event: Event) {
        // Broadcast it to the API Server
        #[cfg(feature = "api_server")]
        {
            let mut lock = self.api_server.lock().await;
            if let Some(server) = lock.as_mut() {
                let kind = event.kind();
                server.notify_event(&kind, &event).await;
            }
        }

        // Broadcast to the event broadcaster
        {
            let mut lock = self.event_broadcaster.lock().await;
            if let Some(broadcaster) = lock.as_ref() {
                // if the receiver is closed, we remove it
                if broadcaster.send(event).is_err() {
                    lock.take();
                }
            }
        }
    }

    // Subscribe to events
    pub async fn subscribe_events(&self) -> BroadcastReceiver<Event> {
        let mut broadcaster = self.event_broadcaster.lock().await;
        match broadcaster.as_ref() {
            Some(broadcaster) => broadcaster.subscribe(),
            None => {
                let (sender, receiver) = tokio::sync::broadcast::channel(10);
                *broadcaster = Some(sender);
                receiver
            }
        }
    }

    // Close events channel
    // This will disconnect all subscribers
    pub async fn close_events_channel(&self) -> bool {
        trace!("Closing events channel");
        let mut broadcaster = self.event_broadcaster.lock().await;
        broadcaster.take().is_some()
    }

    // Enable RPC Server with requested authentication and bind address
    #[cfg(feature = "api_server")]
    pub async fn enable_rpc_server(self: &Arc<Self>, bind_address: String, config: Option<AuthConfig>) -> Result<(), Error> {
        let mut lock = self.api_server.lock().await;
        if lock.is_some() {
            return Err(WalletError::RPCServerAlreadyRunning.into())
        }
        let mut rpc_handler = RPCHandler::new(self.clone());
        register_rpc_methods(&mut rpc_handler);

        let rpc_server = WalletRpcServer::new(bind_address, rpc_handler, config).await?;
        *lock = Some(APIServer::RPCServer(rpc_server));
        Ok(())
    }

    // Enable XSWD Protocol
    #[cfg(feature = "api_server")]
    pub async fn enable_xswd(self: &Arc<Self>) -> Result<UnboundedReceiver<XSWDEvent>, Error> {
        let receiver = {
            let (sender, receiver) = unbounded_channel();
            let mut channel = self.xswd_channel.write().await;
            *channel = Some(sender);
            receiver
        };

        let mut lock = self.api_server.lock().await;
        if lock.is_some() {
            return Err(WalletError::RPCServerAlreadyRunning.into())
        }
        let mut rpc_handler = RPCHandler::new(self.clone());
        register_rpc_methods(&mut rpc_handler);

        *lock = Some(APIServer::XSWD(XSWD::new(rpc_handler)?));
        Ok(receiver)
    }

    #[cfg(feature = "api_server")]
    pub async fn stop_api_server(&self) -> Result<(), Error> {
        let mut lock = self.api_server.lock().await;
        let rpc_server = lock.take().ok_or(WalletError::RPCServerNotRunning)?;
        rpc_server.stop().await;
        Ok(())
    }

    #[cfg(feature = "api_server")]
    pub fn get_api_server<'a>(&'a self) -> &Mutex<Option<APIServer<Arc<Self>>>> {
        &self.api_server
    }

    // Verify if a password is valid or not
    pub async fn is_valid_password(&self, password: String) -> Result<(), Error> {
        let mut encrypted_storage = self.storage.write().await;
        let storage = encrypted_storage.get_mutable_public_storage();
        let salt = storage.get_password_salt()?;
        let hashed_password = hash_password(password, &salt)?;
        let cipher = Cipher::new(&hashed_password, None)?;
        let encrypted_master_key = storage.get_encrypted_master_key()?;
        let _ = cipher.decrypt_value(&encrypted_master_key).context("Invalid password provided")?;
        Ok(())
    }

    // change the current password wallet to a new one
    pub async fn set_password(&self, old_password: String, password: String) -> Result<(), Error> {
        let mut encrypted_storage = self.storage.write().await;
        let storage = encrypted_storage.get_mutable_public_storage();
        let (master_key, storage_salt) = {
            // retrieve old salt to build key from current password
            let salt = storage.get_password_salt()?;
            let hashed_password = hash_password(old_password, &salt)?;

            let encrypted_master_key = storage.get_encrypted_master_key()?;
            let encrypted_storage_salt = storage.get_encrypted_storage_salt()?;

            // decrypt the encrypted master key using the provided password
            let cipher = Cipher::new(&hashed_password, None)?;
            let master_key = cipher.decrypt_value(&encrypted_master_key).context("Invalid password provided")?;
            let storage_salt = cipher.decrypt_value(&encrypted_storage_salt)?;
            (master_key, storage_salt)
        };

        // generate a new salt for password
        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut salt);

        // generate the password-based derivated key to encrypt the master key
        let hashed_password = hash_password(password, &salt)?;
        let cipher = Cipher::new(&hashed_password, None)?;

        // encrypt the master key using the new password
        let encrypted_key = cipher.encrypt_value(&master_key)?;

        // encrypt the salt with the new password
        let encrypted_storage_salt = cipher.encrypt_value(&storage_salt)?;

        // save on disk
        storage.set_password_salt(&salt)?;
        storage.set_encrypted_master_key(&encrypted_key)?;
        storage.set_encrypted_storage_salt(&encrypted_storage_salt)?;

        Ok(())
    }

    // Simple function to make a transfer to the given address by including (if necessary) extra data from it
    pub fn send_to(&self, storage: &EncryptedStorage, asset: Hash, address: Address, amount: u64) -> Result<Transaction, Error> {
        // Verify that we are on the same network as address
        if address.is_mainnet() != self.network.is_mainnet() {
            return Err(WalletError::InvalidAddressParams.into())
        }

        let (key, data) = address.split();
        let extra_data = match data {
            AddressType::Data(data) => Some(data),
            _ => None
        };

        let transfer = self.create_transfer(storage, asset, key, extra_data, amount)?;
        let transaction = self.create_transaction(storage, TransactionType::Transfer(vec![transfer]), FeeBuilder::default())?;
        Ok(transaction)
    }

    // create a transfer from the wallet to the given address to send the given amount of the given asset
    // and include extra data if present
    // TODO encrypt all the extra data for the receiver
    pub fn create_transfer(&self, storage: &EncryptedStorage, asset: Hash, key: PublicKey, extra_data: Option<DataElement>, amount: u64) -> Result<Transfer, WalletError> {
        let balance = storage.get_balance_for(&asset).unwrap_or(0);
        // check if we have enough funds for this asset
        if amount > balance {
            let decimals = storage.get_asset_decimals(&asset).unwrap_or(COIN_DECIMALS);
            return Err(WalletError::NotEnoughFunds(balance, amount, decimals, asset))
        }
        
        // include all extra data in the TX
        let extra_data = if let Some(data) = extra_data {
            let mut writer = Writer::new();
            data.write(&mut writer);

            // TODO encrypt all the extra data for the receiver
            // We can use XChaCha20 with 24 bytes 0 filled Nonce
            // this allow us to prevent saving nonce in it and save space
            // NOTE: We must be sure to have a different key each time

            // Verify the size of the extra data
            if writer.total_write() > EXTRA_DATA_LIMIT_SIZE {
                return Err(WalletError::ExtraDataTooBig(EXTRA_DATA_LIMIT_SIZE, writer.total_write()))
            }

            Some(writer.bytes())
        } else {
            None
        };

        let transfer = Transfer {
            amount,
            asset,
            to: key,
            extra_data
        };
        Ok(transfer)
    }

    // create the final transaction with calculated fees and signature
    // also check that we have enough funds for the transaction
    pub fn create_transaction(&self, storage: &EncryptedStorage, transaction_type: TransactionType, fee: FeeBuilder) -> Result<Transaction, WalletError> {
        let nonce = storage.get_nonce().unwrap_or(0);
        let builder = TransactionBuilder::new(self.keypair.get_public_key().clone(), transaction_type, nonce, fee);
        let assets_spent: HashMap<&Hash, u64> = builder.total_spent();

        // check that we have enough balance for every assets spent
        for (asset, amount) in &assets_spent {
            let asset: &Hash = *asset;
            let balance = storage.get_balance_for(asset).unwrap_or(0);
            if balance < *amount {
                let decimals = storage.get_asset_decimals(asset).unwrap_or(COIN_DECIMALS);
                return Err(WalletError::NotEnoughFunds(balance, *amount, decimals, asset.clone()))
            }
        }

        // now we have to check that we have enough funds for spent + fees
        let total_native_spent = assets_spent.get(&XELIS_ASSET).unwrap_or(&0) +  builder.estimate_fees();
        let native_balance = storage.get_balance_for(&XELIS_ASSET).unwrap_or(0);
        if total_native_spent > native_balance {
            return Err(WalletError::NotEnoughFundsForFee(native_balance, total_native_spent))
        }

        Ok(builder.build(&self.keypair)?)
    }

    // submit a transaction to the network through the connection to daemon
    // It will increase the local nonce by 1 if the TX is accepted by the daemon
    // returns error if the wallet is in offline mode or if the TX is rejected
    pub async fn submit_transaction(&self, transaction: &Transaction) -> Result<(), WalletError> {
        let network_handler = self.network_handler.lock().await;
        if let Some(network_handler) = network_handler.as_ref() {
            network_handler.get_api().submit_transaction(transaction).await?;
            let mut storage = self.storage.write().await;
            storage.set_nonce(transaction.get_nonce() + 1)?;
            Ok(())
        } else {
            Err(WalletError::NotOnlineMode)
        }
    }

    // Estimate fees for a given transaction type
    // Estimated fees returned are the minimum required to be valid on chain
    pub async fn estimate_fees(&self, tx_type: TransactionType) -> Result<u64, WalletError> {
        let storage = self.storage.read().await;
        let builder = TransactionBuilder::new(self.keypair.get_public_key().clone(), tx_type, storage.get_nonce().unwrap_or(0), FeeBuilder::default());
        Ok(builder.estimate_fees())
    }

    // set wallet in online mode: start a communication task which will keep the wallet synced
    pub async fn set_online_mode(self: &Arc<Self>, daemon_address: &String) -> Result<(), WalletError> {
        if self.is_online().await {
            // user have to set in offline mode himself first
            return Err(WalletError::AlreadyOnlineMode)
        }

        // create the network handler
        let network_handler = NetworkHandler::new(Arc::clone(&self), daemon_address).await?;
        // start the task
        network_handler.start().await?;
        *self.network_handler.lock().await = Some(network_handler);

        Ok(())
    }

    // set wallet in offline mode: stop communication task if exists
    pub async fn set_offline_mode(&self) -> Result<(), WalletError> {
        let mut handler = self.network_handler.lock().await;
        if let Some(network_handler) = handler.take() {
            network_handler.stop().await?;
        } else {
            return Err(WalletError::NotOnlineMode)
        }

        Ok(())
    }

    // rescan the wallet from the given topoheight
    // that will delete all transactions above the given topoheight and all balances
    // then it will re-fetch all transactions and balances from daemon
    pub async fn rescan(&self, topoheight: u64) -> Result<(), WalletError> {
        trace!("Rescan wallet from topoheight {}", topoheight);
        if !self.is_online().await {
            // user have to set it online
            return Err(WalletError::NotOnlineMode)
        }

        let handler = self.network_handler.lock().await;
        if let Some(network_handler) = handler.as_ref() {
            debug!("Stopping network handler!");
            network_handler.stop().await?;
            {
                let mut storage = self.get_storage().write().await;
                if topoheight > storage.get_synced_topoheight()? {
                    return Err(WalletError::RescanTopoheightTooHigh)
                }
                debug!("set synced topoheight to {}", topoheight);
                storage.set_synced_topoheight(topoheight)?;
                storage.delete_top_block_hash()?;
                // balances will be re-fetched from daemon
                storage.delete_balances()?;

                debug!("Retrieve current wallet nonce");
                let nonce_result = network_handler.get_api()
                    .get_nonce(&self.get_address()).await
                    // User has no transactions/balances yet, set its nonce to 0
                    .map(|v| v.version.get_nonce()).unwrap_or(0);

                storage.set_nonce(nonce_result)?;

                if topoheight == 0 {
                    debug!("Deleting all transactions for full rescan");
                    storage.delete_transactions()?;
                } else {
                    debug!("Deleting transactions above {} for partial rescan", topoheight);
                    storage.delete_transactions_above_topoheight(topoheight)?;
                }
            }
            debug!("Starting again network handler");
            network_handler.start().await.context("Error while restarting network handler")?;
        } else {
            return Err(WalletError::NotOnlineMode)
        }

        Ok(())
    }

    // Check if the wallet is in online mode
    pub async fn is_online(&self) -> bool {
        if let Some(network_handler) = self.network_handler.lock().await.as_ref() {
            network_handler.is_running().await
        } else {
            false
        }
    }

    // this function allow to user to get the network handler in case in want to stay in online mode
    // but want to pause / resume the syncing task through start/stop functions from it
    pub async fn get_network_handler(&self) -> &Mutex<Option<Arc<NetworkHandler>>> {
        &self.network_handler
    }

    // Create a signature of the given data
    pub fn sign_data(&self, data: &[u8]) -> Signature {
        self.keypair.sign(data)
    }

    // Get the public key of the wallet
    pub fn get_public_key(&self) -> &PublicKey {
        self.keypair.get_public_key()
    }

    // Get the address of the wallet using its network used
    pub fn get_address(&self) -> Address {
        self.keypair.get_public_key().to_address(self.get_network().is_mainnet())
    }

    // Get the address with integrated data and using its network used
    pub fn get_address_with(&self, data: DataElement) -> Address {
        self.keypair.get_public_key().to_address_with(self.get_network().is_mainnet(), data)
    }

    // Returns the seed using the language index provided
    pub fn get_seed(&self, language_index: usize) -> Result<String, Error> {
        let words = mnemonics::key_to_words(self.keypair.get_private_key(), language_index)?;
        Ok(words.join(" "))
    }

    // Current account nonce for transactions
    // Nonce is used against replay attacks on-chain
    pub async fn get_nonce(&self) -> u64 {
        let storage = self.storage.read().await;
        storage.get_nonce().unwrap_or(0)
    }

    // Encrypted storage of the wallet
    pub fn get_storage(&self) -> &RwLock<EncryptedStorage> {
        &self.storage
    }

    // Network that the wallet is using
    pub fn get_network(&self) -> &Network {
        &self.network
    }
}

#[cfg(feature = "api_server")]
pub enum XSWDEvent {
    RequestPermission(AppStateShared, RpcRequest, OneshotSender<Result<PermissionResult, Error>>),
    // bool represents if it was signed or not
    RequestApplication(AppStateShared, bool, OneshotSender<Result<PermissionResult, Error>>),
    CancelRequest(AppStateShared, OneshotSender<Result<(), Error>>)
}

#[cfg(feature = "api_server")]
#[async_trait]
impl XSWDPermissionHandler for Arc<Wallet> {
    async fn request_permission(&self, app_state: &AppStateShared, request: PermissionRequest<'_>) -> Result<PermissionResult, Error> {
        if let Some(sender) = self.xswd_channel.read().await.as_ref() {
            // no other way ?
            let app_state = app_state.clone();
            // create a callback channel to receive the answer
            let (callback, receiver) = channel();
            let event = match request {
                PermissionRequest::Application(signed) => XSWDEvent::RequestApplication(app_state, signed, callback),
                PermissionRequest::Request(request) => XSWDEvent::RequestPermission(app_state, request.clone(), callback)
            };

            // Send the XSWD Message
            sender.send(event)?;

            // Wait on the callback
            return receiver.await?;
        }

        Err(WalletError::NoHandlerAvailable.into())
    }

    // there is a lock to acquire so it make it "single threaded"
    // the one who has the lock is the one who is requesting so we don't need to check and can cancel directly
    async fn cancel_request_permission(&self, app: &AppStateShared) -> Result<(), Error> {
        if let Some(sender) = self.xswd_channel.read().await.as_ref() {
            let (callback, receiver) = channel();
            // Send XSWD Message
            sender.send(XSWDEvent::CancelRequest(app.clone(), callback))?;

            // Wait on callback
            return receiver.await?;
        }

        Err(WalletError::NoHandlerAvailable.into())
    }

    async fn get_public_key(&self) -> Result<&PublicKey, Error> {
        Ok((self as &Wallet).get_public_key())
    }
}

#[cfg(feature = "api_server")]
#[async_trait]
impl XSWDNodeMethodHandler for Arc<Wallet> {
    async fn call_node_with(&self, request: RpcRequest) -> Result<Value, RpcResponseError> {
        let network_handler = self.network_handler.lock().await;
        let id = request.id;
        if let Some(network_handler) = network_handler.as_ref() {
            let api = network_handler.get_api();
            let response = api.call(&request.method, &request.params).await.map_err(|e| RpcResponseError::new(id, InternalRpcError::Custom(e.to_string())))?;
            Ok(json!({
                "jsonrpc": JSON_RPC_VERSION,
                "id": id,
                "result": response
            }))
        } else {
            Err(RpcResponseError::new(id, InternalRpcError::CustomStr("Wallet is not in online mode")))
        }
    }
}