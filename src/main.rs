use subxt::{OnlineClient, PolkadotConfig};
use subxt::utils::AccountId32;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::collections::{HashMap, HashSet};
use chrono::{Utc, Duration};
use futures::future::join_all;
use clap::Parser;

// Subxt macro to generate types from metadata
#[subxt::subxt(runtime_metadata_path = "metadata.scale")]
pub mod creditcoin {}

const NODE_URL: &str = "wss://mainnet3.creditcoin.network";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Show only validators that I am staking to (nominating)
    #[arg(short, long)]
    mine: bool,
}

#[derive(Debug, Clone)]
struct ValidatorInfo {
    address: AccountId32,
    name: String,
    commission: u32,
    total_stake: u128,
    own_stake: u128,
    my_stake: u128,
    nominator_count: u32,
    era_points: u32,
    last_payout_era: Option<u32>,
    last_payout_date: String,
    is_my_target: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // 1. Load my accounts
    let my_accounts = load_my_accounts("my_accounts.txt").unwrap_or_default();
    let my_account_ids: HashSet<[u8; 32]> = my_accounts.values()
        .filter_map(|addr_str| addr_str.parse::<AccountId32>().ok().map(|a| a.0))
        .collect();
        
    println!("Loaded {} accounts from my_accounts.txt", my_accounts.len());

    // 2. Connect to Creditcoin
    let api = OnlineClient::<PolkadotConfig>::from_url(NODE_URL).await?;
    println!("Connected to {}", NODE_URL);

    // 3. Fetch nomination targets for my accounts
    let mut my_targets = HashSet::new();
    for addr_bytes in &my_account_ids {
        let addr = AccountId32(*addr_bytes);
        let nom_query = creditcoin::storage().staking().nominators(&addr);
        if let Some(noms) = api.storage().at_latest().await?.fetch(&nom_query).await? {
            for target in noms.targets.0 {
                my_targets.insert(target.0);
            }
        }
    }
    println!("Identified {} unique nomination targets from your accounts", my_targets.len());

    // 4. Fetch general staking/era info
    let current_era_query = creditcoin::storage().staking().current_era();
    let current_era = api.storage().at_latest().await?.fetch(&current_era_query).await?.unwrap_or(0);
    
    let reward_points_query = creditcoin::storage().staking().eras_reward_points(current_era);
    let reward_points = api.storage().at_latest().await?.fetch(&reward_points_query).await?;
    
    let points_map: HashMap<[u8; 32], u32> = reward_points
        .map(|p| p.individual.into_iter().map(|(acc, pts)| (acc.0, pts)).collect())
        .unwrap_or_default();

    println!("Current Era: {}", current_era);

    // Fetch active validators
    let validators_query = creditcoin::storage().session().validators();
    let active_validators = api.storage().at_latest().await?.fetch(&validators_query).await?.unwrap_or_default();
    println!("Number of active validators: {}", active_validators.len());

    let mut validator_futures = Vec::new();

    for validator_addr in active_validators {
        let api_ref = api.clone();
        let my_targets_ref = &my_targets;
        let my_account_ids_ref = &my_account_ids;
        let points = points_map.get(&validator_addr.0).cloned().unwrap_or(0);
        
        validator_futures.push(async move {
            let addr = validator_addr.clone();
            
            // Parallel sub-queries for this validator
            let prefs_query = creditcoin::storage().staking().validators(&addr);
            let overview_query = creditcoin::storage().staking().eras_stakers_overview(current_era, &addr);
            let bonded_query = creditcoin::storage().staking().bonded(&addr);
            let identity_query = creditcoin::storage().identity().identity_of(&addr);

            let (prefs, overview, bonded, identity) = tokio::join!(
                api_ref.storage().at_latest().await.unwrap().fetch(&prefs_query),
                api_ref.storage().at_latest().await.unwrap().fetch(&overview_query),
                api_ref.storage().at_latest().await.unwrap().fetch(&bonded_query),
                api_ref.storage().at_latest().await.unwrap().fetch(&identity_query)
            );

            let commission = prefs.unwrap_or(None).map(|p| p.commission.0).unwrap_or(0);
            let (total_stake, own_stake, nominator_count, page_count) = match overview.unwrap_or(None) {
                Some(ov) => (ov.total, ov.own, ov.nominator_count, ov.page_count),
                None => (0, 0, 0, 0),
            };

            // Query pages for my stake
            let mut my_stake = 0;
            if my_targets_ref.contains(&addr.0) {
                for page in 0..page_count {
                    let page_query = creditcoin::storage().staking().eras_stakers_paged(current_era, &addr, page);
                    if let Ok(Some(exposure_page)) = api_ref.storage().at_latest().await.unwrap().fetch(&page_query).await {
                        for individual in exposure_page.others {
                            if my_account_ids_ref.contains(&individual.who.0) {
                                my_stake += individual.value;
                            }
                        }
                    }
                }
            }

            let controller_addr = bonded.unwrap_or(None).unwrap_or(addr.clone());
            let ledger_query = creditcoin::storage().staking().ledger(&controller_addr);
            let ledger = api_ref.storage().at_latest().await.unwrap().fetch(&ledger_query).await.unwrap_or(None);

            let mut last_payout_era = None;
            if let Some(l) = ledger {
                last_payout_era = l.legacy_claimed_rewards.0.last().cloned();
            }

            if last_payout_era.is_none() {
                for era in ( (current_era.saturating_sub(15))..(current_era + 1) ).rev() {
                    let claimed_query = creditcoin::storage().staking().claimed_rewards(era, &addr);
                    if let Ok(Some(pages)) = api_ref.storage().at_latest().await.unwrap().fetch(&claimed_query).await {
                        if !pages.is_empty() {
                            last_payout_era = Some(era);
                            break;
                        }
                    }
                }
            }

            let last_payout_date = last_payout_era.map(|e| era_to_date(current_era, e)).unwrap_or_else(|| "N/A".to_string());

            let mut name = "N/A".to_string();
            if let Some((reg, _)) = identity.unwrap_or(None) {
                name = parse_data(reg.info.display);
            } else {
                let super_query = creditcoin::storage().identity().super_of(&addr);
                if let Some((parent_addr, data)) = api_ref.storage().at_latest().await.unwrap().fetch(&super_query).await.unwrap_or(None) {
                    let parent_identity_query = creditcoin::storage().identity().identity_of(&parent_addr);
                    if let Some((parent_reg, _)) = api_ref.storage().at_latest().await.unwrap().fetch(&parent_identity_query).await.unwrap_or(None) {
                        let parent_name = parse_data(parent_reg.info.display);
                        let sub_name = parse_data(data);
                        name = format!("{}/{}", parent_name, sub_name);
                    }
                }
            }

            let is_my_target = my_targets_ref.contains(&addr.0);

            ValidatorInfo {
                address: addr,
                name,
                commission: commission / 10_000_000,
                total_stake,
                own_stake,
                my_stake,
                nominator_count,
                era_points: points,
                last_payout_era,
                last_payout_date,
                is_my_target,
            }
        });
    }

    let mut validator_details = join_all(validator_futures).await;

    if args.mine {
        validator_details.retain(|v| v.is_my_target);
    }

    validator_details.sort_by(|a, b| b.total_stake.cmp(&a.total_stake));

    // 5. Print results
    println!("\n{:<2}{:<48} | {:<22} | {:>4} | {:>14} | {:>14} | {:>12} | {:>5} | {:>6} | {:>5} | {:<12}", 
             " ", "Validator Address", "Name", "Comm", "Total (CTC)", "Own (CTC)", "My (CTC)", "Noms", "Points", "Era", "Last Payout");
    println!("{}", "-".repeat(180));

    for v in validator_details {
        let prefix = if v.is_my_target { "*" } else { " " };
        let era_str = v.last_payout_era.map(|e| e.to_string()).unwrap_or_else(|| "-".to_string());
        println!("{:<2}{:<48} | {:<22} | {:>3}% | {:>14} | {:>14} | {:>12} | {:>5} | {:>6} | {:>5} | {:<12}", 
                 prefix, 
                 v.address.to_string(), 
                 truncate_string(&v.name, 22), 
                 v.commission, 
                 format_number(v.total_stake / 1_000_000_000_000_000_000), 
                 format_number(v.own_stake / 1_000_000_000_000_000_000),
                 format_number(v.my_stake / 1_000_000_000_000_000_000),
                 v.nominator_count, 
                 v.era_points, 
                 era_str,
                 v.last_payout_date);
    }

    Ok(())
}

fn load_my_accounts(path: &str) -> std::io::Result<HashMap<String, String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut accounts = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            accounts.insert(parts[0].to_string(), parts[1].to_string());
        }
    }
    Ok(accounts)
}

fn era_to_date(current_era: u32, claimed_era: u32) -> String {
    let diff = current_era as i64 - claimed_era as i64;
    let payout_time = Utc::now() - Duration::days(diff);
    payout_time.format("%Y-%m-%d").to_string()
}

fn parse_data(data: creditcoin::runtime_types::pallet_identity::types::Data) -> String {
    use creditcoin::runtime_types::pallet_identity::types::Data::*;
    match data {
        Raw0(b) => String::from_utf8_lossy(&b).to_string(),
        Raw1(b) => String::from_utf8_lossy(&b).to_string(),
        Raw2(b) => String::from_utf8_lossy(&b).to_string(),
        Raw3(b) => String::from_utf8_lossy(&b).to_string(),
        Raw4(b) => String::from_utf8_lossy(&b).to_string(),
        Raw5(b) => String::from_utf8_lossy(&b).to_string(),
        Raw6(b) => String::from_utf8_lossy(&b).to_string(),
        Raw7(b) => String::from_utf8_lossy(&b).to_string(),
        Raw8(b) => String::from_utf8_lossy(&b).to_string(),
        Raw9(b) => String::from_utf8_lossy(&b).to_string(),
        Raw10(b) => String::from_utf8_lossy(&b).to_string(),
        Raw11(b) => String::from_utf8_lossy(&b).to_string(),
        Raw12(b) => String::from_utf8_lossy(&b).to_string(),
        Raw13(b) => String::from_utf8_lossy(&b).to_string(),
        Raw14(b) => String::from_utf8_lossy(&b).to_string(),
        Raw15(b) => String::from_utf8_lossy(&b).to_string(),
        Raw16(b) => String::from_utf8_lossy(&b).to_string(),
        Raw17(b) => String::from_utf8_lossy(&b).to_string(),
        Raw18(b) => String::from_utf8_lossy(&b).to_string(),
        Raw19(b) => String::from_utf8_lossy(&b).to_string(),
        Raw20(b) => String::from_utf8_lossy(&b).to_string(),
        Raw21(b) => String::from_utf8_lossy(&b).to_string(),
        Raw22(b) => String::from_utf8_lossy(&b).to_string(),
        Raw23(b) => String::from_utf8_lossy(&b).to_string(),
        Raw24(b) => String::from_utf8_lossy(&b).to_string(),
        Raw25(b) => String::from_utf8_lossy(&b).to_string(),
        Raw26(b) => String::from_utf8_lossy(&b).to_string(),
        Raw27(b) => String::from_utf8_lossy(&b).to_string(),
        Raw28(b) => String::from_utf8_lossy(&b).to_string(),
        Raw29(b) => String::from_utf8_lossy(&b).to_string(),
        Raw30(b) => String::from_utf8_lossy(&b).to_string(),
        Raw31(b) => String::from_utf8_lossy(&b).to_string(),
        Raw32(b) => String::from_utf8_lossy(&b).to_string(),
        _ => "None".to_string(),
    }
}

fn format_number(n: u128) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let len = s.len();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        let mut count = 0;
        let mut result = String::new();
        for c in s.chars() {
            if count >= max_len - 3 {
                break;
            }
            result.push(c);
            count += 1;
        }
        result + "..."
    } else {
        s.to_string()
    }
}
