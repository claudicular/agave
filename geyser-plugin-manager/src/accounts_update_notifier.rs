/// Module responsible for notifying plugins of account updates
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaAccountInfoV3, ReplicaAccountInfoVersions, ReplicaTransactionAccountsInfo,
        ReplicaTransactionAccountsInfoVersions,
    },
    crossbeam_channel::{bounded, Receiver, Sender, TrySendError},
    log::*,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_metrics::*,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        sync::{Arc, Mutex, RwLock},
        thread::{Builder, JoinHandle},
        time::Instant,
    },
};
#[derive(Debug)]
pub(crate) struct AccountsUpdateNotifierImpl {
    plugin_manager: Arc<RwLock<GeyserPluginManager>>,
    snapshot_notifications_enabled: bool,
    async_dispatch: Option<AsyncAccountsDispatch>,
    enable_transaction_accounts_notify: bool,
}

const ASYNC_ACCOUNTS_DISPATCH_CHANNEL_CAPACITY: usize = 16_384;

#[derive(Debug)]
struct QueuedAccountUpdate {
    slot: Slot,
    pubkey: Pubkey,
    account: AccountSharedData,
    txn: Option<SanitizedTransaction>,
    write_version: u64,
    is_startup: bool,
    enqueue_at: Instant,
}

#[derive(Debug)]
enum DispatchMessage {
    Account(QueuedAccountUpdate),
}

#[derive(Debug)]
struct AsyncAccountsDispatch {
    sender: Mutex<Option<Sender<DispatchMessage>>>,
    thread_hdl: Mutex<Option<JoinHandle<()>>>,
}

impl AsyncAccountsDispatch {
    fn try_send(&self, message: DispatchMessage) -> Result<(), TrySendError<DispatchMessage>> {
        let sender = self.sender.lock().unwrap();
        if let Some(sender) = sender.as_ref() {
            sender.try_send(message)
        } else {
            Err(TrySendError::Disconnected(message))
        }
    }

    fn stop(&self) {
        // Drop sender first so receiver exits after draining queued work.
        self.sender.lock().unwrap().take();
        if let Some(thread_hdl) = self.thread_hdl.lock().unwrap().take() {
            let _ = thread_hdl.join();
        }
    }
}

impl AccountsUpdateNotifierInterface for AccountsUpdateNotifierImpl {
    fn snapshot_notifications_enabled(&self) -> bool {
        self.snapshot_notifications_enabled
    }

    fn notify_account_update(
        &self,
        slot: Slot,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        if let Some(async_dispatch) = &self.async_dispatch {
            let message = DispatchMessage::Account(QueuedAccountUpdate {
                slot,
                pubkey: *pubkey,
                account: account.clone(),
                txn: txn.as_ref().map(|tx| (*tx).clone()),
                write_version,
                is_startup: false,
                enqueue_at: Instant::now(),
            });
            match async_dispatch.try_send(message) {
                Ok(()) => {
                    inc_new_counter_debug!("geyser-plugin-async-account-dispatch-queued", 1);
                    return;
                }
                Err(TrySendError::Full(_)) => {
                    inc_new_counter_warn!("geyser-plugin-async-account-dispatch-overflow", 1);
                }
                Err(TrySendError::Disconnected(_)) => {
                    inc_new_counter_warn!("geyser-plugin-async-account-dispatch-disconnected", 1);
                }
            }
        }

        let account_info =
            self.accountinfo_from_shared_account_data(account, txn, pubkey, write_version);
        self.notify_plugins_of_account_update(account_info, slot, false);
    }

    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    ) {
        // Since the counter increment calls (below) are at Debug log level,
        // do not get the time (Instant::now()) unless logging is at Debug level.
        // With ~1 billion accounts on mnb, this is a non-negligible amount of work.
        let start = log_enabled!(Level::Debug).then(Instant::now);

        let mut account = self.accountinfo_from_account_for_geyser(account);
        account.write_version = write_version;
        let time_copy = log_enabled!(Level::Debug).then(|| start.unwrap().elapsed());

        self.notify_plugins_of_account_update(account, slot, true);

        let time_all = log_enabled!(Level::Debug).then(|| start.unwrap().elapsed());

        inc_new_counter_debug!(
            "geyser-plugin-copy-stored-account-info-us",
            time_copy.unwrap().as_micros() as usize,
            100000,
            100000
        );

        inc_new_counter_debug!(
            "geyser-plugin-notify-account-restore-all-us",
            time_all.unwrap().as_micros() as usize,
            100000,
            100000
        );
    }

    fn notify_end_of_restore_from_snapshot(&self) {
        let plugin_manager = self.plugin_manager.read().unwrap();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            let mut measure = Measure::start("geyser-plugin-end-of-restore-from-snapshot");
            match plugin.notify_end_of_startup() {
                Err(err) => {
                    error!(
                        "Failed to notify the end of restore from snapshot, error: {} to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified the end of restore from snapshot to plugin {}",
                        plugin.name()
                    );
                }
            }
            measure.stop();
            inc_new_counter_debug!(
                "geyser-plugin-end-of-restore-from-snapshot",
                measure.as_us() as usize
            );
        }
    }

    fn notify_transaction_accounts(
        &self,
        slot: Slot,
        signature: &Signature,
        transaction_index: usize,
        accounts: &[(&Pubkey, &AccountSharedData)],
        write_version_start: u64,
    ) {
        if !self.enable_transaction_accounts_notify {
            return;
        }
        let plugin_manager = self.plugin_manager.read().unwrap();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        // Build ReplicaAccountInfoV3 for each account
        let account_infos: Vec<ReplicaAccountInfoV3> = accounts
            .iter()
            .enumerate()
            .map(|(i, (pubkey, account))| ReplicaAccountInfoV3 {
                pubkey: pubkey.as_ref(),
                lamports: account.lamports(),
                owner: account.owner().as_ref(),
                executable: account.executable(),
                rent_epoch: account.rent_epoch(),
                data: account.data(),
                write_version: write_version_start.saturating_add(i as u64),
                txn: None, // Transaction reference not needed in grouped notification
            })
            .collect();

        let transaction_accounts_info = ReplicaTransactionAccountsInfo {
            signature,
            slot,
            index: transaction_index,
            accounts: &account_infos,
        };

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.transaction_accounts_notifications_enabled() {
                continue;
            }

            let mut measure = Measure::start("geyser-plugin-notify-transaction-accounts");
            match plugin.notify_transaction_accounts(
                ReplicaTransactionAccountsInfoVersions::V0_0_1(&transaction_accounts_info),
            ) {
                Err(err) => {
                    error!(
                        "Failed to notify transaction accounts for signature {} at slot {}, error: {} to plugin {}",
                        signature,
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified transaction accounts for signature {} at slot {} to plugin {}",
                        signature,
                        slot,
                        plugin.name()
                    );
                }
            }
            measure.stop();
            inc_new_counter_debug!(
                "geyser-plugin-notify-transaction-accounts-us",
                measure.as_us() as usize,
                100000,
                100000
            );
        }
    }

    fn transaction_accounts_notifications_enabled(&self) -> bool {
        if !self.enable_transaction_accounts_notify {
            return false;
        }
        let plugin_manager = self.plugin_manager.read().unwrap();
        plugin_manager
            .plugins
            .iter()
            .any(|plugin| plugin.transaction_accounts_notifications_enabled())
    }

    fn transaction_accounts_include_readonly_owners(&self) -> Vec<Pubkey> {
        if !self.enable_transaction_accounts_notify {
            return vec![];
        }
        let plugin_manager = self.plugin_manager.read().unwrap();
        // Collect all unique owners from all plugins
        let mut owners: Vec<Pubkey> = plugin_manager
            .plugins
            .iter()
            .flat_map(|plugin| plugin.transaction_accounts_include_readonly_owners())
            .collect();
        owners.sort();
        owners.dedup();
        owners
    }
}

impl AccountsUpdateNotifierImpl {
    pub fn new(
        plugin_manager: Arc<RwLock<GeyserPluginManager>>,
        snapshot_notifications_enabled: bool,
        accounts_notify_async: bool,
        enable_transaction_accounts_notify: bool,
    ) -> Self {
        let async_dispatch = accounts_notify_async.then(|| {
            let (sender, receiver) = bounded(ASYNC_ACCOUNTS_DISPATCH_CHANNEL_CAPACITY);
            let plugin_manager = plugin_manager.clone();
            let thread_hdl = Builder::new()
                .name("solGeyserAcctAsync".to_string())
                .spawn(move || Self::run_async_dispatch(receiver, plugin_manager))
                .expect("spawn geyser async account notifier");
            AsyncAccountsDispatch {
                sender: Mutex::new(Some(sender)),
                thread_hdl: Mutex::new(Some(thread_hdl)),
            }
        });

        AccountsUpdateNotifierImpl {
            plugin_manager,
            snapshot_notifications_enabled,
            async_dispatch,
            enable_transaction_accounts_notify,
        }
    }

    fn run_async_dispatch(
        receiver: Receiver<DispatchMessage>,
        plugin_manager: Arc<RwLock<GeyserPluginManager>>,
    ) {
        while let Ok(message) = receiver.recv() {
            match message {
                DispatchMessage::Account(update) => {
                    inc_new_counter_debug!(
                        "geyser-plugin-async-account-dispatch-latency-us",
                        update.enqueue_at.elapsed().as_micros() as usize,
                        100000,
                        100000
                    );
                    let account_info = ReplicaAccountInfoV3 {
                        pubkey: update.pubkey.as_ref(),
                        lamports: update.account.lamports(),
                        owner: update.account.owner().as_ref(),
                        executable: update.account.executable(),
                        rent_epoch: update.account.rent_epoch(),
                        data: update.account.data(),
                        write_version: update.write_version,
                        txn: update.txn.as_ref(),
                    };
                    Self::notify_plugins_of_account_update_inner(
                        &plugin_manager,
                        account_info,
                        update.slot,
                        update.is_startup,
                    );
                    inc_new_counter_debug!("geyser-plugin-async-account-dispatch-drained", 1);
                }
            }
        }
    }

    fn accountinfo_from_shared_account_data<'a>(
        &self,
        account: &'a AccountSharedData,
        txn: &'a Option<&'a SanitizedTransaction>,
        pubkey: &'a Pubkey,
        write_version: u64,
    ) -> ReplicaAccountInfoV3<'a> {
        ReplicaAccountInfoV3 {
            pubkey: pubkey.as_ref(),
            lamports: account.lamports(),
            owner: account.owner().as_ref(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
            data: account.data(),
            write_version,
            txn: *txn,
        }
    }

    fn accountinfo_from_account_for_geyser<'a>(
        &self,
        account: &'a AccountForGeyser<'_>,
    ) -> ReplicaAccountInfoV3<'a> {
        ReplicaAccountInfoV3 {
            pubkey: account.pubkey.as_ref(),
            lamports: account.lamports(),
            owner: account.owner().as_ref(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
            data: account.data(),
            write_version: 0, // can/will be populated afterwards
            txn: None,
        }
    }

    fn notify_plugins_of_account_update(
        &self,
        account: ReplicaAccountInfoV3,
        slot: Slot,
        is_startup: bool,
    ) {
        Self::notify_plugins_of_account_update_inner(
            &self.plugin_manager,
            account,
            slot,
            is_startup,
        );
    }

    fn notify_plugins_of_account_update_inner(
        plugin_manager: &Arc<RwLock<GeyserPluginManager>>,
        account: ReplicaAccountInfoV3,
        slot: Slot,
        is_startup: bool,
    ) {
        let mut measure2 = Measure::start("geyser-plugin-notify_plugins_of_account_update");
        let plugin_manager = plugin_manager.read().unwrap();

        if plugin_manager.plugins.is_empty() {
            return;
        }
        for plugin in plugin_manager.plugins.iter() {
            let mut measure = Measure::start("geyser-plugin-update-account");
            match plugin.update_account(
                ReplicaAccountInfoVersions::V0_0_3(&account),
                slot,
                is_startup,
            ) {
                Err(err) => {
                    error!(
                        "Failed to update account {} at slot {}, error: {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated account {} at slot {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        plugin.name()
                    );
                }
            }
            measure.stop();
            inc_new_counter_debug!(
                "geyser-plugin-update-account-us",
                measure.as_us() as usize,
                100000,
                100000
            );
        }
        measure2.stop();
        inc_new_counter_debug!(
            "geyser-plugin-notify_plugins_of_account_update-us",
            measure2.as_us() as usize,
            100000,
            100000
        );
    }
}

impl Drop for AccountsUpdateNotifierImpl {
    fn drop(&mut self) {
        if let Some(async_dispatch) = &self.async_dispatch {
            async_dispatch.stop();
        }
    }
}
