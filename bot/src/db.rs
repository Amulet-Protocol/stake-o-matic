use {
    crate::{
        data_center_info::{DataCenterId, DataCenterInfo},
        generic_stake_pool::ValidatorStakeState,
        Config,
    },
    log::*,
    serde::{Deserialize, Serialize},
    solana_sdk::{clock::Epoch, pubkey::Pubkey},
    std::{
        collections::HashMap,
        fs::{self, File},
        io::{self, Write},
        path::{Path, PathBuf},
    },
};
use crate::InflationRewardInfo;

#[derive(Default, Clone, Deserialize, Serialize)]
pub struct ScoreDiscounts {
    pub can_halt_the_network_group: bool,
    pub is_registered_delegation_group: bool
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct ByIdentityInfo {
    pub data_center_id: DataCenterId,
    pub keybase_id: String,
    pub name: String,
    pub www_url: String,
    pub description: String,
    pub software_version: String
}

#[derive(Default, Clone, Deserialize, Serialize)]
/// computed score (more granular than ValidatorStakeState)
pub struct ScoreData {
    /// epoch_credits is the base score
    pub epoch_credits: u64,
    pub adj_credits: u64,
    /// 50 => Average, 0=>worst, 100=twice the average
    pub average_position: f64,
    pub score_discounts: ScoreDiscounts,
    pub commission: u8,
    pub active_stake: u64,
    pub data_center_concentration: f64,
    pub validators_app_info: ByIdentityInfo,
    pub total_active_stake: u64,
    pub mean_active_stake: u64,
    pub std_active_stake: u64,
    pub inflation_reward: Option<InflationRewardInfo>
}

#[derive(Default, Clone, Deserialize, Serialize)]
pub struct ValidatorClassification {
    pub identity: Pubkey, // Validator identity
    pub vote_address: Pubkey,

    pub stake_state: ValidatorStakeState,
    pub stake_state_reason: String,

    // added optional validator scoring data
    pub score_data: Option<ScoreData>,

    // Summary of the action was taken this epoch to advance the validator's stake
    pub stake_action: Option<String>,

    // History of stake states, newest first, including (`stake_state`, `stake_state_reason`) at index 0
    pub stake_states: Option<Vec<(ValidatorStakeState, String)>>,

    // Informational notes regarding this validator
    pub notes: Vec<String>,

    // Map of data center to number of times the validator has been observed there.
    pub data_center_residency: Option<HashMap<DataCenterId, usize>>,

    // The data center that the validator was observed at for this classification
    pub current_data_center: Option<DataCenterId>,

    // The identity of the staking program participant, used to establish a link between
    // testnet and mainnet validator classifications
    pub participant: Option<Pubkey>,

    // The validator was not funded this epoch and should be prioritized next epoch
    pub prioritize_funding_in_next_epoch: Option<bool>,
}

impl ScoreData {
    pub fn score(&self, config: &Config, is_reward_missing: bool) -> u64 {
        if self.score_discounts.can_halt_the_network_group
            || self.active_stake < config.score_min_stake
            || self.average_position < config.min_avg_position
            // if config.min_avg_position=100 => everybody passes
            // if config.min_avg_position=50 => only validators above avg pass
            || self.commission > config.score_max_commission
            || (!is_reward_missing && self.inflation_reward.map(|i| i.apy).is_none())
        {
            0
        } else {
            // if data_center_concentration = 25%, lose all score,
            // data_center_concentration = 10%, lose 40% (rounded)
            let discount_because_data_center_concentration = self.data_center_concentration
                * config.score_concentration_point_discount as f64;

            // score discounts according to commission
            // apply commission % as a discount to credits_observed.
            // The rationale es:
            // If you're the top performer validator and get 300K credits, but you have 50% commission,
            // from our user's point of view, it's the same as a 150K credits validator with 0% commission,
            // both represent the same APY for the user.
            // So to treat both the same we apply commission to self.epoch_credits
            let discount_because_commission = self.commission as f64;

            // score discount according to the active stake
            // if the stake percentage is too high, more than 3 std dev,
            // compared to the total active stake, add discount
            let active_stake_position = (self.active_stake * 50) as f64 /
                (config.active_stake_std_multiplier as f64 * self.std_active_stake as f64);
            let discount_because_active_stake = if active_stake_position - 49.0 < 0.0 {
                0.0
            } else {
                active_stake_position - 49 as f64
            };

            // give bonus when the validator is part of the Solana delegation program
            let bonus_because_validator_program = (if self.score_discounts.is_registered_delegation_group {
                config.score_registered_validator_bonus
            } else {
                0
            }) as f64;

            let avg_position_bonus = if self.average_position - 49.0 > 0.0 {
                self.average_position - 49.0
            } else {
                0.0
            };
            let total_percentage = (100.0 - discount_because_commission - discount_because_active_stake
                - discount_because_data_center_concentration + avg_position_bonus + bonus_because_validator_program) / 100.0;
            //result
            (self.epoch_credits as f64 * total_percentage) as u64
        }
    }
}

impl ValidatorClassification {
    pub fn stake_state_streak(&self) -> usize {
        let mut streak = 1;

        if let Some(ref stake_states) = self.stake_states {
            while streak < stake_states.len() && stake_states[0].0 == stake_states[streak].0 {
                streak += 1;
            }
        }
        streak
    }

    // Was the validator staked for at last `n` of the last `m` epochs?
    pub fn staked_for(&self, n: usize, m: usize) -> bool {
        self.stake_states
            .as_ref()
            .map(|stake_states| {
                stake_states
                    .iter()
                    .take(m)
                    .filter(|(stake_state, _)| *stake_state != ValidatorStakeState::None)
                    .count()
                    >= n
            })
            .unwrap_or_default()
    }
}

pub type ValidatorClassificationByIdentity =
    HashMap<solana_sdk::pubkey::Pubkey, ValidatorClassification>;

#[derive(Default, Deserialize, Serialize, Clone)]
pub struct ClusterState {
    pub min_epoch_credits: u64,
    pub avg_epoch_credits: u64,
    pub poor_voter_percentage: usize,
    pub too_many_poor_voters: bool,
    pub cluster_average_skip_rate: usize,
    pub poor_block_producer_percentage: usize,
    pub too_many_poor_block_producers: bool,
    pub too_many_old_validators: bool
}

#[derive(Default, Deserialize, Serialize, Clone)]
pub struct EpochClassificationV1 {
    // Data Center observations for this epoch
    pub data_center_info: Vec<DataCenterInfo>,

    // `None` indicates a pause due to unusual observations during classification
    pub validator_classifications: Option<ValidatorClassificationByIdentity>,

    // Informational notes regarding this epoch
    pub notes: Vec<String>,

    pub cluster_state: ClusterState
}

#[derive(Deserialize, Serialize, Clone)]
pub enum EpochClassification {
    V1(EpochClassificationV1),
}

impl Default for EpochClassification {
    fn default() -> Self {
        Self::V1(EpochClassificationV1::default())
    }
}

impl EpochClassification {
    pub fn new(v1: EpochClassificationV1) -> Self {
        EpochClassification::V1(v1)
    }

    pub fn into_current(self) -> EpochClassificationV1 {
        match self {
            EpochClassification::V1(v1) => v1,
        }
    }

    fn file_name<P>(epoch: Epoch, path: P) -> PathBuf
    where
        P: AsRef<Path>,
    {
        path.as_ref().join(format!("epoch-{}.yml", epoch))
    }

    pub fn exists<P>(epoch: Epoch, path: P) -> bool
    where
        P: AsRef<Path>,
    {
        Self::file_name(epoch, path).exists()
    }

    pub fn load<P>(epoch: Epoch, path: P) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let file = File::open(Self::file_name(epoch, path))?;
        serde_yaml::from_reader(file)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{:?}", err)))
    }

    // Loads the first epoch older than `epoch` that contains `Some(validator_classifications)`.
    // Returns `Ok(None)` if no previous epochs are available
    pub fn load_previous<P>(epoch: Epoch, path: P) -> Result<Option<(Epoch, Self)>, io::Error>
    where
        P: AsRef<Path>,
    {
        let mut previous_epoch = epoch;
        loop {
            if previous_epoch == 0 {
                info!(
                    "No previous EpochClassification found at {}",
                    path.as_ref().display()
                );
                return Ok(None);
            }
            previous_epoch -= 1;

            if Self::exists(previous_epoch, &path) {
                let previous_epoch_classification =
                    Self::load(previous_epoch, &path)?.into_current();

                if previous_epoch_classification
                    .validator_classifications
                    .is_some()
                {
                    info!(
                        "Previous EpochClassification found for epoch {} at {}",
                        previous_epoch,
                        path.as_ref().display()
                    );
                    return Ok(Some((
                        previous_epoch,
                        Self::V1(previous_epoch_classification),
                    )));
                } else {
                    info!(
                        "Skipping previous EpochClassification for epoch {}",
                        previous_epoch
                    );
                }
            }
        }
    }

    // Loads the latest epoch that contains `Some(validator_classifications)`
    // Returns `Ok(None)` if no epoch is available
    pub fn load_latest<P>(path: P) -> Result<Option<(Epoch, Self)>, io::Error>
    where
        P: AsRef<Path>,
    {
        let epoch_filename_regex = regex::Regex::new(r"^epoch-(\d+).yml$").unwrap();

        let mut epochs = vec![];
        if let Ok(entries) = fs::read_dir(&path) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                if entry.path().is_file() {
                    let filename = entry
                        .file_name()
                        .into_string()
                        .unwrap_or_else(|_| String::new());

                    if let Some(captures) = epoch_filename_regex.captures(&filename) {
                        epochs.push(captures.get(1).unwrap().as_str().parse::<u64>().unwrap());
                    }
                }
            }
        }
        epochs.sort_unstable();

        if let Some(latest_epoch) = epochs.last() {
            Self::load_previous(*latest_epoch + 1, path)
        } else {
            Ok(None)
        }
    }

    pub fn save<P>(&self, epoch: Epoch, path: P) -> Result<(), io::Error>
    where
        P: AsRef<Path>,
    {
        let serialized = serde_yaml::to_string(self)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{:?}", err)))?;

        fs::create_dir_all(&path)?;
        let mut file = File::create(Self::file_name(epoch, path))?;
        file.write_all(&serialized.into_bytes())?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use solana_account_decoder::parse_account_data::ParsableAccount::Config;
    use solana_sdk::native_token::sol_to_lamports;
    use super::*;

    #[test]
    fn test_staked_for() {
        let mut vc = ValidatorClassification::default();

        assert!(!vc.staked_for(0, 0));
        assert!(!vc.staked_for(1, 0));
        assert!(!vc.staked_for(0, 1));

        vc.stake_states = Some(vec![
            (ValidatorStakeState::None, String::new()),
            (ValidatorStakeState::Baseline, String::new()),
            (ValidatorStakeState::Bonus, String::new()),
        ]);
        assert!(!vc.staked_for(3, 3));
        assert!(vc.staked_for(2, 3));
    }

    #[test]
    fn test_score_data_calculation() {
        let score = ScoreData {
            epoch_credits: 1000,
            adj_credits: 900,
            average_position: 52.5,
            score_discounts: ScoreDiscounts {
                can_halt_the_network_group: false,
                is_registered_delegation_group: true
            },
            commission: 10,
            active_stake: sol_to_lamports(54000.0),
            data_center_concentration: 0.5,
            validators_app_info: ByIdentityInfo::default(),
            total_active_stake: sol_to_lamports(500000.0),
            mean_active_stake: sol_to_lamports(32000.0),
            std_active_stake: sol_to_lamports(45000.0)
        };
        let score = score.score(&crate::Config::default_for_test());
        let expected_score = 1020;
        assert_eq!(score, expected_score);
    }
}
