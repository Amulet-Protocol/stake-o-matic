use solana_client::rpc_response::RpcInflationReward;
use solana_sdk::epoch_schedule::EpochSchedule;
use rayon::prelude::*;
use std::sync::{Arc, Mutex};
use {
    indicatif::{ProgressBar, ProgressStyle},
    log::*,
    solana_client::{
        pubsub_client::PubsubClientError,
        rpc_client::RpcClient,
        rpc_config::RpcSimulateTransactionConfig,
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSendTransactionConfig},
        rpc_filter,
        rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
        rpc_response::{Fees, RpcVoteAccountInfo, RpcVoteAccountStatus},
        tpu_client::{TpuClient, TpuClientConfig, TpuSenderError},
    },
    serde::{Deserialize, Serialize},
    solana_sdk::{
        clock::Epoch,
        native_token::*,
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
        stake,
        transaction::{Transaction, TransactionError},
        account::from_account,
        account_utils::StateMut,
        clock::Clock,
        stake::state::StakeState,
        commitment_config::CommitmentConfig,
        stake_history::StakeHistory,
        stake::state::StakeActivationStatus,
        sysvar,
    },
    std::{
        collections::{HashMap, HashSet},
        error,
        str::FromStr,
        thread::sleep,
        time::{Duration, Instant}
    },
};

fn new_spinner_progress_bar() -> ProgressBar {
    let progress_bar = ProgressBar::new(42);
    progress_bar
        .set_style(ProgressStyle::default_spinner().template("{spinner:.green} {wide_msg}"));
    progress_bar.enable_steady_tick(100);
    progress_bar
}

fn new_tpu_client_with_retry(
    rpc_client: &Arc<RpcClient>,
    websocket_url: &str,
) -> Result<TpuClient, TpuSenderError> {
    let mut retries = 5;
    let sleep_seconds = 5;
    while retries > 0 {
        match TpuClient::new(
            rpc_client.clone(),
            websocket_url,
            TpuClientConfig::default(),
        ) {
            // only retry on connection error
            Err(TpuSenderError::PubsubError(PubsubClientError::ConnectionError(_))) => {
                warn!(
                    "Error creating Tpu Client, retrying in {}s, {} retries remaining",
                    sleep_seconds, retries
                );
                retries -= 1;
                sleep(Duration::from_secs(sleep_seconds));
            }
            // everything else, Ok or Err, can pass through
            result => return result,
        }
    }
    TpuClient::new(
        rpc_client.clone(),
        websocket_url,
        TpuClientConfig::default(),
    )
}

pub struct SendAndConfirmTransactionResult {
    pub succeeded: HashSet<Signature>,
    pub failed: HashSet<Signature>,
}

pub fn send_and_confirm_transactions_with_spinner(
    rpc_client: Arc<RpcClient>,
    websocket_url: &str,
    dry_run: bool,
    transactions: Vec<Transaction>,
    signer: &Keypair,
) -> Result<Vec<Option<TransactionError>>, Box<dyn error::Error>> {
    if transactions.is_empty() {
        return Ok(vec![]);
    }
    let progress_bar = new_spinner_progress_bar();
    let mut expired_blockhash_retries = 5;
    let send_transaction_interval = Duration::from_millis(10); /* Send at ~100 TPS */
    let transaction_resend_interval = Duration::from_secs(4); /* Retry batch send after 4 seconds */

    progress_bar.set_message("Connecting...");
    let tpu_client = new_tpu_client_with_retry(&rpc_client, websocket_url)?;

    let mut transactions = transactions.into_iter().enumerate().collect::<Vec<_>>();
    let num_transactions = transactions.len() as f64;
    let mut transaction_errors = vec![None; transactions.len()];
    let set_message = |confirmed_transactions,
                       block_height: Option<u64>,
                       last_valid_block_height: u64,
                       status: &str| {
        progress_bar.set_message(format!(
            "{:>5.1}% | {:<40}{}",
            confirmed_transactions as f64 * 100. / num_transactions,
            status,
            match block_height {
                Some(block_height) => format!(
                    " [block height {}; re-sign in {} blocks]",
                    block_height,
                    last_valid_block_height.saturating_sub(block_height),
                ),
                None => String::new(),
            },
        ));
    };

    let mut confirmed_transactions = 0;
    let mut block_height = rpc_client.get_block_height()?;
    while expired_blockhash_retries > 0 {
        let Fees {
            blockhash,
            fee_calculator: _,
            last_valid_block_height,
        } = rpc_client.get_fees()?;

        let mut pending_transactions = HashMap::new();
        for (i, mut transaction) in transactions {
            transaction.try_sign(&[signer], blockhash)?;
            pending_transactions.insert(transaction.signatures[0], (i, transaction));
        }

        let mut last_resend = Instant::now() - transaction_resend_interval;
        while block_height <= last_valid_block_height {
            let num_transactions = pending_transactions.len();

            // Periodically re-send all pending transactions
            if Instant::now().duration_since(last_resend) > transaction_resend_interval {
                for (index, (_i, transaction)) in pending_transactions.values().enumerate() {
                    let method = if dry_run {
                        "DRY RUN"
                    } else if tpu_client.send_transaction(transaction) {
                        "TPU"
                    } else {
                        let _ = rpc_client.send_transaction_with_config(
                            transaction,
                            RpcSendTransactionConfig {
                                skip_preflight: true,
                                ..RpcSendTransactionConfig::default()
                            },
                        );
                        "RPC"
                    };
                    set_message(
                        confirmed_transactions,
                        None, //block_height,
                        last_valid_block_height,
                        &format!(
                            "Sending {}/{} transactions (via {})",
                            index + 1,
                            num_transactions,
                            method
                        ),
                    );
                    sleep(send_transaction_interval);
                }
                last_resend = Instant::now();
            }

            // Wait for the next block before checking for transaction statuses
            set_message(
                confirmed_transactions,
                Some(block_height),
                last_valid_block_height,
                &format!("Waiting for next block, {} pending...", num_transactions),
            );

            let mut new_block_height = block_height;
            while block_height == new_block_height {
                sleep(Duration::from_millis(500));
                new_block_height = rpc_client.get_block_height()?;
            }
            block_height = new_block_height;
            if dry_run {
                return Ok(transaction_errors);
            }

            // Collect statuses for the transactions, drop those that are confirmed
            let pending_signatures = pending_transactions.keys().cloned().collect::<Vec<_>>();
            for pending_signatures_chunk in
            pending_signatures.chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS)
            {
                if let Ok(result) = rpc_client.get_signature_statuses(pending_signatures_chunk) {
                    let statuses = result.value;
                    for (signature, status) in
                    pending_signatures_chunk.iter().zip(statuses.into_iter())
                    {
                        if let Some(status) = status {
                            if status.satisfies_commitment(rpc_client.commitment()) {
                                if let Some((i, _)) = pending_transactions.remove(signature) {
                                    confirmed_transactions += 1;
                                    if status.err.is_some() {
                                        progress_bar
                                            .println(format!("Failed transaction: {:?}", status));
                                    }
                                    transaction_errors[i] = status.err;
                                }
                            }
                        }
                    }
                }
                set_message(
                    confirmed_transactions,
                    Some(block_height),
                    last_valid_block_height,
                    "Checking transaction status...",
                );
            }

            if pending_transactions.is_empty() {
                return Ok(transaction_errors);
            }
        }

        transactions = pending_transactions.into_iter().map(|(_k, v)| v).collect();
        progress_bar.println(format!(
            "Blockhash expired. {} retries remaining",
            expired_blockhash_retries
        ));
        expired_blockhash_retries -= 1;
    }
    Err("Max retries exceeded".into())
}

#[derive(Default, Copy, Clone, Deserialize, Serialize)]
pub struct VoteAccountInfo {
    pub identity: Pubkey,
    pub vote_address: Pubkey,
    pub commission: u8,
    pub active_stake: u64,

    /// Credits earned in the epoch
    pub epoch_credits: u64,
}

#[derive(Default, Copy, Clone, Deserialize, Serialize)]
pub struct InflationRewardInfo {
    pub identity: Pubkey,
    pub effective_slot: u64,
    pub post_balance: f64,
    pub amount: f64,
    pub apy: f64
}

#[derive(Default, Copy, Clone, Deserialize, Serialize, Debug)]
pub struct StakeAccountInfo {
    pub stake_account: Pubkey,
    pub voter_pubkey: Pubkey,
    pub stake: u64,
    pub activation_epoch: Epoch,
    pub deactivation_epoch: Epoch,
    pub warmup_cooldown_rate: f64,
    pub effective: u64,
    pub activating: u64,
    pub deactivating: u64
}

pub fn mean_std(data: &[u64]) -> (u64, u64) {
    let sum = data.iter().sum::<u64>() as f64;
    let count = data.len();
    let mean = sum / count as f64;
    let variance = data.iter().map(|value| {
        let diff = mean - (*value as f64);
        diff * diff
    }).sum::<f64>() / count as f64;

    (mean as u64, variance.sqrt() as u64)
}

pub fn get_epoch_boundary_timestamps(
    rpc_client: &RpcClient,
    reward: &RpcInflationReward,
    epoch_schedule: &EpochSchedule,
) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    let epoch_end_time = rpc_client.get_block_time(reward.effective_slot)?;
    let mut epoch_start_slot = epoch_schedule.get_first_slot_in_epoch(reward.epoch);
    let performance_samples = rpc_client.get_recent_performance_samples(Some(360))?;
    let average_slot_time =
        performance_samples.iter()
            .map(|s| s.sample_period_secs as f32 / s.num_slots as f32 ).sum::<f32>() as f32 /
                performance_samples.len() as f32;
    info!("average_slot_time {}", average_slot_time);
    let epoch_start_time = loop {
        if epoch_start_slot >= reward.epoch + 10 {
            let diff_slot = reward.effective_slot - epoch_start_slot;
            let diff_slot_duration = diff_slot as f64  * average_slot_time as f64;
            let start_time = epoch_end_time - diff_slot_duration as i64;
            break start_time;
        }
        match rpc_client.get_block_time(epoch_start_slot) {
            Ok(block_time) => {
                break block_time;
            }
            Err(_) => {
                epoch_start_slot += 1;
            }
        }
    };
    Ok((epoch_start_time, epoch_end_time))
}

pub fn get_reward_apy(
    reward: &RpcInflationReward,
    epoch_start_time: i64,
    epoch_end_time: i64,
) -> Option<f64> {
    pub const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
    let wallclock_epoch_duration = epoch_end_time.checked_sub(epoch_start_time)?;
    if reward.post_balance > reward.amount {
        let rate_change = reward.amount as f64 / (reward.post_balance - reward.amount) as f64;
        let wallclock_epochs_per_year =
            (SECONDS_PER_DAY * 365) as f64 / wallclock_epoch_duration as f64;
        let apr = rate_change * wallclock_epochs_per_year;

        Some(apr)
    } else {
        None
    }
}

pub fn get_inflation_rewards(
    rpc_client: &RpcClient,
    vote_accounts: &Vec<VoteAccountInfo>,
    epoch: Epoch
) -> Result<HashMap<Pubkey, InflationRewardInfo>, Box<dyn error::Error>>{
    // get all the stake accounts
    let stake_accounts = get_stake_accounts(rpc_client)?;
    info!("Stake accounts len: {}", stake_accounts.len());
    // (vote_account, stake_account)
    let addresses: Vec<(Pubkey, Pubkey)> = vote_accounts.into_iter().flat_map(|vote_account| {
        // filter stake account by the highest stake and not activating or deactivating
        let stakes = stake_accounts.get(&vote_account.vote_address);
        stakes.map(|accounts| {
            let mut accounts = accounts.to_vec();
            accounts.sort_by(|a1, a2| a2.effective.cmp(&a1.effective));
            accounts.into_iter()
                .filter(|s| s.effective > 0 && s.activating == 0
                    && s.deactivating == 0 && s.deactivation_epoch == Epoch::MAX)
                .map(|a| (vote_account.vote_address, a.stake_account)).next()
        }).flatten()
    }).collect();
    let batch_size = 700;
    let batches = addresses.chunks(batch_size);
    let mut rewards = Vec::<(Pubkey, RpcInflationReward)>::new();
    info!("Get inflation rewards: {} addresses", addresses.len());
    for batch in batches {
        let stake_addresses: Vec<Pubkey> = batch.into_iter().map(|b| b.1).collect();
        let inflation_rewards =
            rpc_client.get_inflation_reward(&stake_addresses, Some(epoch))?;
        for (i, (vote_account, stake_account)) in batch.iter().enumerate() {
            if let Some(inflation_reward) = &inflation_rewards[i] {
                rewards.push((vote_account.clone(), inflation_reward.clone()));
            }
        }
    }

    let epoch_schedule = rpc_client.get_epoch_schedule()?;
    // Calculate apy
    info!("Calculate APY");
    let epoch_cache = Arc::new(Mutex::new(HashMap::<u64, (i64, i64)>::new()));
    let inflation_reward_infos: Vec<InflationRewardInfo> = rewards.into_par_iter().flat_map(|r| {
        let (address, inflation_reward) = r;
        let epoch_time = match epoch_cache.lock().unwrap().get(&inflation_reward.effective_slot).cloned() {
            Some(epoch) => Ok(epoch),
            None => {
                get_epoch_boundary_timestamps(rpc_client, &inflation_reward, &epoch_schedule)
            }
        };
        match epoch_time {
            Ok((epoch_start_time, epoch_end_time)) => {
                epoch_cache.lock().unwrap().insert(inflation_reward.effective_slot, (epoch_start_time, epoch_end_time));
                let reward_apy = get_reward_apy(&inflation_reward, epoch_start_time, epoch_end_time);
                Some(InflationRewardInfo {
                    identity: address.clone(),
                    effective_slot: inflation_reward.effective_slot,
                    post_balance: lamports_to_sol(inflation_reward.post_balance),
                    amount: lamports_to_sol(inflation_reward.amount),
                    apy: reward_apy.unwrap_or(0.0)
                })
            }
            _ => None
        }
    }).collect();

    let mut inflation_reward_info = HashMap::<Pubkey, InflationRewardInfo>::new();
    for reward in inflation_reward_infos {
        inflation_reward_info.insert(reward.identity, reward);
    }
    Ok(inflation_reward_info)
}

pub fn get_vote_account_info(
    rpc_client: &RpcClient,
    epoch: Epoch,
) -> Result<(Vec<VoteAccountInfo>, u64, u64, u64), Box<dyn error::Error>> {
    let RpcVoteAccountStatus {
        current,
        delinquent,
    } = rpc_client.get_vote_accounts()?;

    let mut latest_vote_account_info = HashMap::<String, _>::new();

    let mut total_active_stake = 0;
    let mut active_stakes: Vec<u64> = Vec::new();
    for vote_account_info in current.into_iter().chain(delinquent.into_iter()) {
        active_stakes.push(vote_account_info.activated_stake);
        total_active_stake += vote_account_info.activated_stake;

        let entry = latest_vote_account_info
            .entry(vote_account_info.node_pubkey.clone())
            .or_insert_with(|| vote_account_info.clone());

        // If the validator has multiple staked vote accounts then select the vote account that
        // voted most recently
        if entry.last_vote < vote_account_info.last_vote {
            *entry = vote_account_info.clone();
        }
    }

    let (mean_active_stake, std_active_stake) = mean_std(&active_stakes);

    Ok((
        latest_vote_account_info
            .values()
            .map(
                |RpcVoteAccountInfo {
                     commission,
                     node_pubkey,
                     vote_pubkey,
                     epoch_credits,
                     activated_stake,
                     ..
                 }| {
                    let epoch_credits = if let Some((_last_epoch, credits, prev_credits)) =
                        epoch_credits.iter().find(|ec| ec.0 == epoch)
                    {
                        credits.saturating_sub(*prev_credits)
                    } else {
                        0
                    };
                    let identity = Pubkey::from_str(node_pubkey).unwrap();
                    let vote_address = Pubkey::from_str(vote_pubkey).unwrap();

                    VoteAccountInfo {
                        identity,
                        vote_address,
                        active_stake: *activated_stake,
                        commission: *commission,
                        epoch_credits,
                    }
                },
            )
            .collect(),
        total_active_stake,
        mean_active_stake,
        std_active_stake
    ))
}

pub fn get_stake_accounts(
    rpc_client: &RpcClient,
) -> Result<HashMap<Pubkey, Vec<StakeAccountInfo>>, Box<dyn error::Error>> {
    let mut all_stake_addresses = HashMap::<Pubkey, Vec<StakeAccountInfo>>::new();
    let all_stake_accounts = rpc_client.get_program_accounts(
        &stake::program::id()
    )?;
    let stake_history_account = rpc_client
        .get_account_with_commitment(&sysvar::stake_history::id(), CommitmentConfig::finalized())?
        .value
        .unwrap();

    let stake_history: StakeHistory =
        from_account(&stake_history_account).ok_or("Failed to deserialize stake history")?;
    let clock_account = rpc_client.get_account(&sysvar::clock::id())?;
    let clock: Clock = from_account(&clock_account).ok_or("Failed to deserialize clock sysvar")?;
    let current_epoch = clock.epoch;
    for (stake_pubkey, stake_account) in all_stake_accounts {
        if let Ok(stake_state) = stake_account.state() {
            match stake_state {
                StakeState::Stake(_, stake) => {
                    let StakeActivationStatus { effective, activating, deactivating } = stake
                        .delegation
                        .stake_activating_and_deactivating(current_epoch, Some(&stake_history));
                    let stake_account_info = StakeAccountInfo {
                        stake_account: stake_pubkey,
                        voter_pubkey: stake.delegation.voter_pubkey,
                        stake: stake.delegation.stake,
                        activation_epoch: stake.delegation.activation_epoch,
                        deactivation_epoch: stake.delegation.deactivation_epoch,
                        warmup_cooldown_rate: stake.delegation.warmup_cooldown_rate,
                        effective,
                        activating,
                        deactivating
                    };
                    all_stake_addresses.entry(stake.delegation.voter_pubkey)
                        .or_insert_with(Vec::new).push(stake_account_info);
                }
                _ => {}
            }
        }
    };
    Ok(all_stake_addresses)
}

pub fn get_all_stake(
    rpc_client: &RpcClient,
    authorized_staker: Pubkey,
) -> Result<(HashSet<Pubkey>, u64), Box<dyn error::Error>> {
    let mut all_stake_addresses = HashSet::new();
    let mut total_stake_balance = 0;

    let all_stake_accounts = rpc_client.get_program_accounts_with_config(
        &stake::program::id(),
        RpcProgramAccountsConfig {
            filters: Some(vec![
                // Filter by `Meta::authorized::staker`, which begins at byte offset 12
                rpc_filter::RpcFilterType::Memcmp(rpc_filter::Memcmp {
                    offset: 12,
                    bytes: rpc_filter::MemcmpEncodedBytes::Binary(authorized_staker.to_string()),
                    encoding: Some(rpc_filter::MemcmpEncoding::Binary),
                }),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
                commitment: Some(rpc_client.commitment()),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        },
    )?;

    for (address, account) in all_stake_accounts {
        all_stake_addresses.insert(address);
        total_stake_balance += account.lamports;
    }

    Ok((all_stake_addresses, total_stake_balance))
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        borsh::BorshSerialize,
        indicatif::{ProgressBar, ProgressStyle},
        solana_client::client_error,
        solana_sdk::{
            borsh::get_packed_len,
            clock::Epoch,
            program_pack::Pack,
            pubkey::Pubkey,
            stake::{
                instruction as stake_instruction,
                state::{Authorized, Lockup},
            },
            system_instruction,
        },
        solana_vote_program::{vote_instruction, vote_state::VoteInit},
        spl_stake_pool::{
            find_stake_program_address, find_withdraw_authority_program_address,
            state::{Fee, StakePool, ValidatorList},
        },
        spl_token::state::{Account, Mint},
    };

    fn new_spinner_progress_bar() -> ProgressBar {
        let progress_bar = ProgressBar::new(42);
        progress_bar
            .set_style(ProgressStyle::default_spinner().template("{spinner:.green} {wide_msg}"));
        progress_bar.enable_steady_tick(100);
        progress_bar
    }

    pub fn wait_for_next_epoch(rpc_client: &RpcClient) -> client_error::Result<Epoch> {
        let current_epoch = rpc_client.get_epoch_info()?.epoch;

        let progress_bar = new_spinner_progress_bar();
        loop {
            let epoch_info = rpc_client.get_epoch_info()?;
            if epoch_info.epoch > current_epoch {
                return Ok(epoch_info.epoch);
            }
            progress_bar.set_message(&format!(
                "Waiting for epoch {} ({} slots remaining)",
                current_epoch + 1,
                epoch_info
                    .slots_in_epoch
                    .saturating_sub(epoch_info.slot_index),
            ));

            sleep(Duration::from_millis(200));
        }
    }

    pub fn create_vote_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        identity_keypair: &Keypair,
        vote_keypair: &Keypair,
    ) -> client_error::Result<()> {
        let mut transaction = Transaction::new_with_payer(
            &vote_instruction::create_account(
                &payer.pubkey(),
                &vote_keypair.pubkey(),
                &VoteInit {
                    node_pubkey: identity_keypair.pubkey(),
                    authorized_voter: identity_keypair.pubkey(),
                    authorized_withdrawer: identity_keypair.pubkey(),
                    commission: 10,
                },
                sol_to_lamports(1.),
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, identity_keypair, vote_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn create_stake_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        authority: &Pubkey,
        amount: u64,
    ) -> client_error::Result<Keypair> {
        let stake_keypair = Keypair::new();
        let mut transaction = Transaction::new_with_payer(
            &stake_instruction::create_account(
                &payer.pubkey(),
                &stake_keypair.pubkey(),
                &Authorized::auto(authority),
                &Lockup::default(),
                amount,
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, &stake_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| stake_keypair)
    }

    pub fn delegate_stake(
        rpc_client: &RpcClient,
        authority: &Keypair,
        stake_address: &Pubkey,
        vote_address: &Pubkey,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[stake_instruction::delegate_stake(
                stake_address,
                &authority.pubkey(),
                vote_address,
            )],
            Some(&authority.pubkey()),
            &[authority],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub struct ValidatorAddressPair {
        pub identity: Pubkey,
        pub vote_address: Pubkey,
    }

    pub fn create_validators(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        num_validators: u32,
    ) -> client_error::Result<Vec<ValidatorAddressPair>> {
        let mut validators = vec![];

        for _ in 0..num_validators {
            let identity_keypair = Keypair::new();
            let vote_keypair = Keypair::new();

            create_vote_account(
                rpc_client,
                authorized_staker,
                &identity_keypair,
                &vote_keypair,
            )?;

            validators.push(ValidatorAddressPair {
                identity: identity_keypair.pubkey(),
                vote_address: vote_keypair.pubkey(),
            });
        }

        Ok(validators)
    }

    pub fn create_mint(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        manager: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let mint_rent = rpc_client.get_minimum_balance_for_rent_exemption(Mint::LEN)?;
        let mint_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &mint_keypair.pubkey(),
                    mint_rent,
                    Mint::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_mint(
                    &spl_token::id(),
                    &mint_keypair.pubkey(),
                    manager,
                    None,
                    0,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &mint_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| mint_keypair.pubkey())
    }

    pub fn create_token_account(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let account_rent = rpc_client.get_minimum_balance_for_rent_exemption(Account::LEN)?;
        let account_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &account_keypair.pubkey(),
                    account_rent,
                    Account::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_account(
                    &spl_token::id(),
                    &account_keypair.pubkey(),
                    mint,
                    owner,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &account_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| account_keypair.pubkey())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_stake_pool(
        rpc_client: &RpcClient,
        payer: &Keypair,
        stake_pool: &Keypair,
        reserve_stake: &Pubkey,
        pool_mint: &Pubkey,
        pool_token_account: &Pubkey,
        manager: &Keypair,
        staker: &Pubkey,
        max_validators: u32,
    ) -> client_error::Result<()> {
        let stake_pool_size = get_packed_len::<StakePool>();
        let stake_pool_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(stake_pool_size)
            .unwrap();
        let validator_list = ValidatorList::new(max_validators);
        let validator_list_size = validator_list.try_to_vec().unwrap().len();
        let validator_list_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(validator_list_size)
            .unwrap();
        let validator_list = Keypair::new();
        let fee = Fee {
            numerator: 10,
            denominator: 100,
        };

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &payer.pubkey(),
                    &stake_pool.pubkey(),
                    stake_pool_rent,
                    stake_pool_size as u64,
                    &spl_stake_pool::id(),
                ),
                system_instruction::create_account(
                    &payer.pubkey(),
                    &validator_list.pubkey(),
                    validator_list_rent,
                    validator_list_size as u64,
                    &spl_stake_pool::id(),
                ),
                spl_stake_pool::instruction::initialize(
                    &spl_stake_pool::id(),
                    &stake_pool.pubkey(),
                    &manager.pubkey(),
                    staker,
                    &validator_list.pubkey(),
                    reserve_stake,
                    pool_mint,
                    pool_token_account,
                    &spl_token::id(),
                    None,
                    fee,
                    max_validators,
                ),
            ],
            Some(&payer.pubkey()),
        );
        transaction.sign(
            &[payer, stake_pool, &validator_list, manager],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn deposit_into_stake_pool(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        stake_pool_address: &Pubkey,
        stake_pool: &StakePool,
        vote_address: &Pubkey,
        stake_address: &Pubkey,
        pool_token_address: &Pubkey,
    ) -> client_error::Result<()> {
        let validator_stake_address =
            find_stake_program_address(&spl_stake_pool::id(), vote_address, stake_pool_address).0;
        let pool_withdraw_authority =
            find_withdraw_authority_program_address(&spl_stake_pool::id(), stake_pool_address).0;
        let transaction = Transaction::new_signed_with_payer(
            &spl_stake_pool::instruction::deposit(
                &spl_stake_pool::id(),
                stake_pool_address,
                &stake_pool.validator_list,
                &pool_withdraw_authority,
                stake_address,
                &authorized_staker.pubkey(),
                &validator_stake_address,
                &stake_pool.reserve_stake,
                pool_token_address,
                &stake_pool.pool_mint,
                &spl_token::id(),
            ),
            Some(&authorized_staker.pubkey()),
            &[authorized_staker],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn transfer(
        rpc_client: &RpcClient,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        lamports: u64,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &from_keypair.pubkey(),
                to_address,
                lamports,
            )],
            Some(&from_keypair.pubkey()),
            &[from_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }
}
