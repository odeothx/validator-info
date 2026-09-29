use subxt::{OnlineClient, PolkadotConfig};
use subxt::utils::AccountId32;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::collections::{HashMap, HashSet};
use chrono::{Utc, Duration};
use futures::future::join_all;
use clap::Parser;
use unicode_width::UnicodeWidthStr;

// Subxt macro to generate types from metadata
#[subxt::subxt(runtime_metadata_path = "metadata.scale")]
pub mod creditcoin {}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Show only validators that I am staking to (nominating)
    #[arg(short, long)]
    mine: bool,

    /// Recommend the best validators based on reward profitability
    #[arg(short, long)]
    recommend: bool,

    /// Show validators with unclaimed payouts for my account
    #[arg(short, long)]
    unpaid: bool,

    /// RPC Node URL to connect to
    #[arg(short = 'u', long, default_value = "wss://mainnet3.creditcoin.network")]
    url: String,

    /// File containing my account addresses
    #[arg(short = 'a', long, default_value = "my_accounts.txt")]
    accounts: String,
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
    last_payout_date: String,
    last_payout_days_ago: Option<i64>,
    is_my_target: bool,
    profitability_score: f64,
    unpaid_eras: Vec<u32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // 1. Load my accounts
    let my_accounts = load_my_accounts(&args.accounts).unwrap_or_default();
    let my_account_ids: HashSet<[u8; 32]> = my_accounts.values()
        .filter_map(|addr_str| addr_str.parse::<AccountId32>().ok().map(|a| a.0))
        .collect();
        
    println!("Loaded {} accounts from {}", my_accounts.len(), args.accounts);

    // 2. Connect to Creditcoin
    let api = OnlineClient::<PolkadotConfig>::from_url(&args.url).await?;
    println!("Connected to {}", args.url);

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
    
    let active_era_query = creditcoin::storage().staking().active_era();
    let active_era_info = api.storage().at_latest().await?.fetch(&active_era_query).await?;
    let active_era = active_era_info.map(|a| a.index).unwrap_or(current_era);
    
    // 5. Fetch reward points for the active era
    let reward_points_query = creditcoin::storage().staking().eras_reward_points(active_era);
    let reward_points = api.storage().at_latest().await?.fetch(&reward_points_query).await?;
    
    let points_map: HashMap<[u8; 32], u32> = reward_points
        .map(|p| p.individual.into_iter().map(|(acc, pts)| (acc.0, pts)).collect())
        .unwrap_or_default();

    // Find the claimable era range once, outside the loop
    let mut claimable_start_era = active_era.saturating_sub(1);
    let mut claimable_end_era = active_era.saturating_sub(1);
    let mut found_any_rewards = false;

    for search_era in (active_era.saturating_sub(10)..active_era).rev() {
        let era_reward_query = creditcoin::storage().staking().eras_validator_reward(search_era);
        if api.storage().at_latest().await?.fetch(&era_reward_query).await?.is_some() {
            claimable_end_era = search_era;
            found_any_rewards = true;
            break;
        }
    }

    if found_any_rewards {
        for search_era in (claimable_end_era.saturating_sub(100)..claimable_end_era).rev() {
            let era_reward_query = creditcoin::storage().staking().eras_validator_reward(search_era);
            if api.storage().at_latest().await?.fetch(&era_reward_query).await?.is_some() {
                claimable_start_era = search_era;
            } else {
                break;
            }
        }
    }

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
        let era_range = if found_any_rewards { Some(claimable_start_era..=claimable_end_era) } else { None };
        
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

            let commission_raw = prefs.unwrap_or(None).map(|p| p.commission.0).unwrap_or(0);
            let commission_percent = commission_raw / 10_000_000;
            
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

            let (last_payout_date, last_payout_days_ago) = last_payout_era.map(|e| {
                let diff = current_era as i64 - e as i64;
                let payout_time = Utc::now() - Duration::days(diff);
                (payout_time.format("%Y-%m-%d").to_string(), Some(diff))
            }).unwrap_or_else(|| ("N/A".to_string(), None));

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

            // Score = (1 - Commission) / Total Stake
            let total_stake_ctc = (total_stake / 1_000_000_000_000_000_000) as f64;
            let profitability_score = if total_stake_ctc > 0.0 {
                (1.0 - (commission_percent as f64 / 100.0)) / total_stake_ctc * 1_000_000.0
            } else {
                0.0
            };

            // Calculate unpaid eras (only for my targets)
            let mut unpaid_eras = Vec::new();
            if is_my_target {
                if let Some(range) = era_range {
                    for era in range {
                        // Verify rewards still exist for this era
                        let era_reward_query = creditcoin::storage().staking().eras_validator_reward(era);
                        let has_era_reward = api_ref.storage().at_latest().await.unwrap()
                            .fetch(&era_reward_query).await.unwrap_or(None).is_some();
                        
                        if !has_era_reward {
                            continue;
                        }
                        
                        // Check if validator was in the active set for this era
                        let era_overview_query = creditcoin::storage().staking().eras_stakers_overview(era, &addr);
                        let overview = api_ref.storage().at_latest().await.unwrap()
                            .fetch(&era_overview_query).await.unwrap_or(None);
                        
                        if let Some(ov) = overview {
                            // Check if payout was claimed for all pages
                            let claimed_query = creditcoin::storage().staking().claimed_rewards(era, &addr);
                            let claimed_pages = api_ref.storage().at_latest().await.unwrap()
                                .fetch(&claimed_query).await
                                .unwrap_or(None)
                                .unwrap_or_default();
                            
                            let total_pages = ov.page_count;
                            let all_claimed = if total_pages == 0 {
                                claimed_pages.contains(&0)
                            } else {
                                (0..total_pages).all(|page| claimed_pages.contains(&page))
                            };
                            
                            if !all_claimed {
                                unpaid_eras.push(era);
                            }
                        }
                    }
                }
            }

            ValidatorInfo {
                address: addr,
                name,
                commission: commission_percent,
                total_stake,
                own_stake,
                my_stake,
                nominator_count,
                era_points: points,
                last_payout_date,
                last_payout_days_ago,
                is_my_target,
                profitability_score,
                unpaid_eras,
            }
        });
    }

    let mut validator_details = join_all(validator_futures).await;

    if args.mine {
        validator_details.retain(|v| v.is_my_target);
    }

    // Handle --unpaid option
    if args.unpaid {
        println!("\n🔍 Checking for unclaimed payouts for your nominated validators...\n");
        
        // Filter to show only my targets with unpaid eras
        validator_details.retain(|v| v.is_my_target && !v.unpaid_eras.is_empty());
        
        if validator_details.is_empty() {
            println!("✅ Good news! All payouts have been claimed for your nominated validators.");
            println!("   (Checked last 84 eras)");
        } else {
            // Sort by number of unpaid eras (most unpaid first)
            validator_details.sort_by(|a, b| b.unpaid_eras.len().cmp(&a.unpaid_eras.len()));
            
            println!("{:<48} | {:<22} | {:>10} | Unpaid Eras", 
                     "Validator Address", "Name", "Unpaid");
            println!("{}", "-".repeat(120));
            
            for v in &validator_details {
                let truncated_name = truncate_string_by_width(&v.name, 22);
                let name_padding = 22_isize - truncated_name.width() as isize;
                let padded_name = if name_padding > 0 {
                    format!("{}{}", truncated_name, " ".repeat(name_padding as usize))
                } else {
                    truncated_name
                };
                
                // Format unpaid eras (show first few and count)
                let unpaid_display = if v.unpaid_eras.len() <= 5 {
                    v.unpaid_eras.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(", ")
                } else {
                    let first_five: Vec<_> = v.unpaid_eras.iter().take(5).map(|e| e.to_string()).collect();
                    format!("{}, ... (+{} more)", first_five.join(", "), v.unpaid_eras.len() - 5)
                };
                
                println!("{:<48} | {} | {:>10} | {}", 
                         v.address.to_string(), 
                         padded_name, 
                         v.unpaid_eras.len(),
                         unpaid_display);
            }
            
            println!("\n⚠️  Found {} validator(s) with unclaimed payouts!", validator_details.len());
            println!("💡 TIP: You can claim these rewards using polkadot.js or creditcoin-cli:");
            println!("   - Each era must be claimed separately");
            println!("   - Anyone can trigger the payout, rewards go to all nominators");
        }
        
        return Ok(());
    }

    if args.recommend {
        println!("Applying recommendation filters: Excluding validators with payouts > 7 days ago or N/A.");
        validator_details.retain(|v| {
            match v.last_payout_days_ago {
                Some(days) => days <= 7,
                None => false,
            }
        });
        validator_details.sort_by(|a, b| b.profitability_score.partial_cmp(&a.profitability_score).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        validator_details.sort_by(|a, b| b.total_stake.cmp(&a.total_stake));
    }

    // 5. Print results
    println!("\n{:<2}{:<48} | {:<22} | {:>4} | {:>14} | {:>14} | {:>12} | {:>5} | {:>6} | {:>8} | {:<12}", 
             " ", "Validator Address", "Name", "Comm", "Total (CTC)", "Own (CTC)", "My (CTC)", "Noms", "Points", "Score", "Last Payout");
    println!("{}", "-".repeat(186));

    for v in validator_details {
        let prefix = if v.is_my_target { "*" } else { " " };
        
        let truncated_name = truncate_string_by_width(&v.name, 22);
        let name_padding = 22_isize - truncated_name.width() as isize;
        let padded_name = if name_padding > 0 {
            format!("{}{}", truncated_name, " ".repeat(name_padding as usize))
        } else {
            truncated_name
        };

        println!("{:<2}{:<48} | {} | {:>3}% | {:>14} | {:>14} | {:>12} | {:>5} | {:>6} | {:>8.2} | {:<12}", 
                 prefix, 
                 v.address.to_string(), 
                 padded_name, 
                 v.commission, 
                 format_number(v.total_stake / 1_000_000_000_000_000_000), 
                 format_number(v.own_stake / 1_000_000_000_000_000_000),
                 format_number(v.my_stake / 1_000_000_000_000_000_000),
                 v.nominator_count, 
                 v.era_points, 
                 v.profitability_score,
                 v.last_payout_date);
    }

    if args.recommend {
        println!("\n[TIP] Recommended validators are sorted by Reward Score.");
        println!("      Formula: (1 - Comm%) / TotalStake. Lower total stake means higher individual share.");
    }

    Ok(())
}

fn load_my_accounts(path: &str) -> std::io::Result<HashMap<String, String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut accounts = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        
        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        
        // Support both "name = address" and "name address" formats
        if trimmed.contains('=') {
            let parts: Vec<&str> = trimmed.splitn(2, '=').collect();
            if parts.len() == 2 {
                let name = parts[0].trim().to_string();
                let addr = parts[1].trim().to_string();
                if !name.is_empty() && !addr.is_empty() {
                    accounts.insert(name, addr);
                }
            }
        } else {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                accounts.insert(parts[0].to_string(), parts[1].to_string());
            }
        }
    }
    Ok(accounts)
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

fn truncate_string_by_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }

    let mut current_width = 0;
    let mut result = String::new();
    let target_width = max_width.saturating_sub(3);

    for c in s.chars() {
        let cw = c.to_string().width();
        if current_width + cw > target_width {
            break;
        }
        result.push(c);
        current_width += cw;
    }

    // Add padding to reach target_width if needed before dots
    let dots_padding = target_width.saturating_sub(current_width);
    if dots_padding > 0 {
        result.push_str(&" ".repeat(dots_padding));
    }

    result + "..."
}
