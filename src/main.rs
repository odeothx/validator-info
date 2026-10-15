use subxt::{OnlineClient, PolkadotConfig};
use subxt::utils::AccountId32;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::collections::{HashMap, HashSet};
use chrono::{Utc, Duration};
use futures::future::join_all;
use clap::Parser;
use unicode_width::UnicodeWidthStr;

mod plan;

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

    /// Plan nominations so all my validators stay elected with maximum reward
    #[arg(short, long)]
    plan: bool,

    /// Minimum safety margin (CTC a rival must gain to push one of my validators out) for --plan
    #[arg(long, default_value_t = 300_000.0)]
    plan_margin: f64,
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

    if args.plan {
        return run_plan(&api, &my_accounts, args.plan_margin).await;
    }

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
            if let Some(reg) = identity.unwrap_or(None) {
                name = parse_data(reg.info.display);
            } else {
                let super_query = creditcoin::storage().identity().super_of(&addr);
                if let Some((parent_addr, data)) = api_ref.storage().at_latest().await.unwrap().fetch(&super_query).await.unwrap_or(None) {
                    let parent_identity_query = creditcoin::storage().identity().identity_of(&parent_addr);
                    if let Some(parent_reg) = api_ref.storage().at_latest().await.unwrap().fetch(&parent_identity_query).await.unwrap_or(None) {
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

const PLANCK_PER_CTC: f64 = 1e18;

/// Fetch the election snapshot from chain, then search and print the best nomination plan.
async fn run_plan(
    api: &OnlineClient<PolkadotConfig>,
    my_accounts: &HashMap<String, String>,
    min_margin: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    use creditcoin::runtime_types::pallet_staking::RewardDestination;
    use plan::{Candidate, Planner, Rng, Snapshot, Voter, Wallet};

    let storage = api.storage().at_latest().await?;
    let account_from_key = |key: &[u8]| -> [u8; 32] { key[key.len() - 32..].try_into().unwrap() };

    // Ledgers: stash -> active bond, controller -> stash
    let mut active_of: HashMap<[u8; 32], f64> = HashMap::new();
    let mut stash_of_controller: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
    let mut iter = storage.iter(creditcoin::storage().staking().ledger_iter()).await?;
    while let Some(kv) = iter.next().await {
        let kv = kv?;
        active_of.insert(kv.value.stash.0, kv.value.active as f64 / PLANCK_PER_CTC);
        stash_of_controller.insert(account_from_key(&kv.key_bytes), kv.value.stash.0);
    }
    let controller_of: HashMap<[u8; 32], [u8; 32]> = stash_of_controller.iter().map(|(c, s)| (*s, *c)).collect();

    let mut payee_of: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
    let mut iter = storage.iter(creditcoin::storage().staking().payee_iter()).await?;
    while let Some(kv) = iter.next().await {
        let kv = kv?;
        let stash = account_from_key(&kv.key_bytes);
        let dest = match kv.value {
            RewardDestination::Account(a) => a.0,
            RewardDestination::Controller => *controller_of.get(&stash).unwrap_or(&stash),
            _ => stash,
        };
        payee_of.insert(stash, dest);
    }

    let my_ids: HashMap<[u8; 32], String> = my_accounts.iter()
        .filter_map(|(name, addr)| addr.parse::<AccountId32>().ok().map(|a| (a.0, name.clone())))
        .collect();

    // Validator candidates
    let mut candidates = Vec::new();
    let mut own = Vec::new();
    let mut iter = storage.iter(creditcoin::storage().staking().validators_iter()).await?;
    while let Some(kv) = iter.next().await {
        let kv = kv?;
        let id = account_from_key(&kv.key_bytes);
        let is_mine = my_ids.contains_key(&id) || payee_of.get(&id).map_or(false, |p| my_ids.contains_key(p));
        if is_mine {
            own.push(candidates.len());
        }
        candidates.push(Candidate {
            id,
            name: String::new(),
            self_bond: *active_of.get(&id).unwrap_or(&0.0),
            commission: kv.value.commission.0 as f64 / 1e9,
        });
    }
    if own.is_empty() {
        println!("No validators found whose stash or reward payee is in the accounts file. Nothing to plan.");
        return Ok(());
    }
    for &c in &own {
        let addr = AccountId32(candidates[c].id);
        let identity = storage.fetch(&creditcoin::storage().identity().identity_of(&addr)).await?;
        let name = identity.map(|reg| parse_data(reg.info.display)).filter(|n| !n.is_empty() && n != "None");
        candidates[c].name = name
            .or_else(|| my_ids.get(&candidates[c].id).cloned())
            .unwrap_or_else(|| addr.to_string()[..8].to_string());
    }
    own.sort_by(|&a, &b| candidates[a].name.cmp(&candidates[b].name));

    // Nominators
    let index_of: HashMap<[u8; 32], usize> = candidates.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    let mut voters = Vec::new();
    let mut wallets = Vec::new();
    let mut iter = storage.iter(creditcoin::storage().staking().nominators_iter()).await?;
    while let Some(kv) = iter.next().await {
        let kv = kv?;
        let id = account_from_key(&kv.key_bytes);
        if let Some(name) = my_ids.get(&id) {
            wallets.push(Wallet { name: name.clone(), voter_idx: voters.len() });
        }
        voters.push(Voter {
            id,
            stake: *active_of.get(&id).unwrap_or(&0.0),
            targets: kv.value.targets.0.iter().filter_map(|t| index_of.get(&t.0).copied()).collect(),
        });
    }
    if wallets.is_empty() {
        println!("None of the accounts in the accounts file is a nominator. Nothing to plan.");
        return Ok(());
    }
    wallets.sort_by(|a, b| voters[b.voter_idx].stake.partial_cmp(&voters[a.voter_idx].stake).unwrap());
    if own.len() > 10 {
        println!("Too many own validators ({}) to search all subsets; at most 10 are supported.", own.len());
        return Ok(());
    }

    // Era reward and era length
    let desired = storage.fetch_or_default(&creditcoin::storage().staking().validator_count()).await? as usize;
    let active_era = storage.fetch(&creditcoin::storage().staking().active_era()).await?.map(|a| a.index).unwrap_or(0);
    let mut era_reward = 0.0;
    for era in (active_era.saturating_sub(5)..active_era).rev() {
        if let Some(r) = storage.fetch(&creditcoin::storage().staking().eras_validator_reward(era)).await? {
            era_reward = r as f64 / PLANCK_PER_CTC;
            break;
        }
    }
    let constants = api.constants();
    let era_ms = constants.at(&creditcoin::constants().staking().sessions_per_era())? as f64
        * constants.at(&creditcoin::constants().babe().epoch_duration())? as f64
        * constants.at(&creditcoin::constants().babe().expected_block_time())? as f64;

    let snap = Snapshot {
        candidates,
        voters,
        desired,
        reward_per_validator: era_reward / desired.max(1) as f64,
        eras_per_year: 365.0 * 24.0 * 3600.0 * 1000.0 / era_ms,
        own,
        wallets,
    };
    let planner = Planner::new(&snap);
    let my_stake = planner.my_total_stake();
    let own_set: HashSet<usize> = snap.own.iter().copied().collect();

    println!("\n=== Nomination Plan (active era {}, {} seats, {} candidates, {} nominators) ===", active_era, desired, snap.candidates.len(), snap.voters.len());
    println!("Election model: sequential Phragmén (no balancing), reward ~{:.1} CTC per validator per era", snap.reward_per_validator);
    println!("My validators : {}", snap.own.iter().map(|&c| snap.candidates[c].name.clone()).collect::<Vec<_>>().join(", "));
    println!("My wallets    : {} (total {} CTC incl. validator self-bond)",
             snap.wallets.iter().map(|w| w.name.clone()).collect::<Vec<_>>().join(", "),
             format_number(my_stake as u128));

    let current: Vec<Vec<usize>> = snap.wallets.iter().map(|w| snap.voters[w.voter_idx].targets.clone()).collect();
    let current_masks: Vec<u32> = current.iter().map(|targets| {
        snap.own.iter().enumerate().filter(|(_, c)| targets.contains(c)).fold(0, |m, (i, _)| m | 1 << i)
    }).map(|m| if m == 0 { (1 << snap.own.len()) - 1 } else { m }).collect();

    println!("\nSearching nomination sets (min margin {} CTC)...", format_number(min_margin as u128));
    let mut rng = Rng::new(0x5eed);
    let best_masks = planner.optimize(min_margin, &current_masks, 6, &mut rng);
    let best: Vec<Vec<usize>> = best_masks.iter().map(|&m| planner.mask_targets(m)).collect();

    let report = |label: &str, config: &[Vec<usize>], rng: &mut Rng| {
        let ev = planner.evaluate(config);
        let margin = planner.margin(config, 1_000_000.0);
        let robust = planner.robustness(config, 500, rng);
        let elected = snap.own.iter().filter(|c| ev.election.winners.contains(c)).count();
        println!("\n[{}] my validators elected {}/{} | reward {:.1} CTC/era | APR {:.2}% | margin {} CTC | all elected in {:.1}% of 500 perturbed runs",
                 label, elected, snap.own.len(), ev.income_per_era,
                 ev.income_per_era * snap.eras_per_year / my_stake * 100.0,
                 format_number(margin as u128), robust * 100.0);
        for &c in &snap.own {
            match ev.election.winners.iter().position(|&w| w == c) {
                Some(pos) => println!("    {:<16} elected  (seat {:>2}/{})  backing {:>12} CTC",
                                      snap.candidates[c].name, pos + 1, desired, format_number(ev.election.backing[c] as u128)),
                None => println!("    {:<16} NOT ELECTED", snap.candidates[c].name),
            }
        }
    };
    report("Current", &current, &mut rng);
    report("Recommended", &best, &mut rng);

    // Wallet x validator table
    let short = |s: &str| truncate_string_by_width(s, 10);
    print!("\n{:<12} | {:>12} |", "Wallet", "Bonded CTC");
    for &c in &snap.own {
        print!(" {:^10} |", short(&snap.candidates[c].name));
    }
    println!(" Change");
    println!("{}", "-".repeat(30 + 13 * snap.own.len() + 8));
    for ((w, targets), cur) in snap.wallets.iter().zip(&best).zip(&current) {
        let stake = snap.voters[w.voter_idx].stake;
        print!("{:<12} | {:>12} |", w.name, format_number(stake as u128));
        for &c in &snap.own {
            print!(" {:^10} |", if targets.contains(&c) { "O" } else { "-" });
        }
        let cur_own: HashSet<usize> = cur.iter().copied().filter(|c| own_set.contains(c)).collect();
        let unchanged = cur_own == targets.iter().copied().collect() && cur.len() == cur_own.len();
        println!(" {}", if unchanged { "keep" } else { "UPDATE" });
    }

    println!("\nstaking.nominate targets:");
    for (w, targets) in snap.wallets.iter().zip(&best) {
        println!("  {} ({}):", w.name, AccountId32(snap.voters[w.voter_idx].id));
        for &c in targets {
            println!("    {}  {}", AccountId32(snap.candidates[c].id), snap.candidates[c].name);
        }
    }
    println!("\n[NOTE] Model assumes equal era points per validator. Re-run after any large stake or nomination change.");
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
