//! Nomination planner: simulates the on-chain sequential Phragmén election
//! (no balancing) and searches per-wallet nomination sets that keep all of
//! our own validators elected while maximizing our expected reward.

use std::collections::HashSet;

pub type AccountBytes = [u8; 32];

/// Resolution (CTC) of the safety-margin scan.
const MARGIN_STEP: f64 = 20_000.0;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: AccountBytes,
    pub name: String,
    pub self_bond: f64,
    pub commission: f64, // 0.0 ~ 1.0
}

#[derive(Debug, Clone)]
pub struct Voter {
    pub id: AccountBytes,
    pub stake: f64,
    pub targets: Vec<usize>, // indices into candidates
}

#[derive(Debug, Clone)]
pub struct Wallet {
    pub name: String,
    pub voter_idx: usize, // index into Snapshot::voters
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub candidates: Vec<Candidate>,
    /// Nominators only; validator self-votes are added during the election.
    pub voters: Vec<Voter>,
    pub desired: usize,
    pub reward_per_validator: f64, // CTC per era per elected validator
    pub eras_per_year: f64,
    pub own: Vec<usize>, // candidate indices of our own validators
    pub wallets: Vec<Wallet>,
}

pub struct Election {
    pub winners: Vec<usize>, // in election order
    pub backing: Vec<f64>,   // per candidate
    /// Stake assigned by each input voter (same order as `voters`) as (candidate, weight).
    pub assignments: Vec<Vec<(usize, f64)>>,
}

/// Sequential Phragmén as in sp-npos-elections `seq_phragmen`, without balancing.
pub fn seq_phragmen(candidates: &[Candidate], voters: &[Voter], desired: usize) -> Election {
    let n = candidates.len();
    // Every validator votes for itself with its own bond.
    let self_votes: Vec<Voter> = candidates.iter().enumerate()
        .map(|(i, c)| Voter { id: c.id, stake: c.self_bond, targets: vec![i] })
        .collect();
    let all: Vec<&Voter> = self_votes.iter().chain(voters.iter()).collect();

    let mut approval = vec![0.0; n];
    let mut voters_of: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (vi, v) in all.iter().enumerate() {
        for &c in &v.targets {
            approval[c] += v.stake;
            voters_of[c].push(vi);
        }
    }

    let mut elected = vec![false; n];
    let mut voter_load = vec![0.0; all.len()];
    let mut edge_load: Vec<Vec<f64>> = all.iter().map(|v| vec![0.0; v.targets.len()]).collect();
    let mut winners = Vec::with_capacity(desired);

    for _ in 0..desired.min(n) {
        let mut best: Option<(usize, f64)> = None;
        for c in 0..n {
            if elected[c] || approval[c] <= 0.0 {
                continue;
            }
            let mut score = 1.0 / approval[c];
            for &vi in &voters_of[c] {
                score += all[vi].stake * voter_load[vi] / approval[c];
            }
            if best.map_or(true, |(_, s)| score < s) {
                best = Some((c, score));
            }
        }
        let Some((winner, score)) = best else { break };
        elected[winner] = true;
        winners.push(winner);
        for &vi in &voters_of[winner] {
            let pos = all[vi].targets.iter().position(|&t| t == winner).unwrap();
            edge_load[vi][pos] = score - voter_load[vi];
            voter_load[vi] = score;
        }
    }

    let mut backing = vec![0.0; n];
    let mut assignments = Vec::with_capacity(all.len());
    for (vi, v) in all.iter().enumerate() {
        let mut a = Vec::new();
        if voter_load[vi] > 0.0 {
            for (pos, &c) in v.targets.iter().enumerate() {
                if elected[c] {
                    let w = v.stake * edge_load[vi][pos] / voter_load[vi];
                    backing[c] += w;
                    a.push((c, w));
                }
            }
        }
        assignments.push(a);
    }
    // Self-vote stake is part of backing; only nominator assignments are returned.
    assignments.drain(..n);
    Election { winners, backing, assignments }
}

/// Result of evaluating one nomination configuration.
pub struct Evaluation {
    pub all_own_elected: bool,
    pub income_per_era: f64,
    pub election: Election,
}

pub struct Planner<'a> {
    snap: &'a Snapshot,
    mine: HashSet<AccountBytes>,
}

impl<'a> Planner<'a> {
    pub fn new(snap: &'a Snapshot) -> Self {
        let mut mine: HashSet<AccountBytes> = snap.wallets.iter().map(|w| snap.voters[w.voter_idx].id).collect();
        mine.extend(snap.own.iter().map(|&c| snap.candidates[c].id));
        Planner { snap, mine }
    }

    pub fn my_total_stake(&self) -> f64 {
        let wallets: f64 = self.snap.wallets.iter().map(|w| self.snap.voters[w.voter_idx].stake).sum();
        let own: f64 = self.snap.own.iter().map(|&c| self.snap.candidates[c].self_bond).sum();
        wallets + own
    }

    /// Targets for a bitmask over `snap.own`.
    pub fn mask_targets(&self, mask: u32) -> Vec<usize> {
        self.snap.own.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, &c)| c).collect()
    }

    fn voters_with(&self, config: &[Vec<usize>], extra: Option<Voter>) -> Vec<Voter> {
        let mut voters = self.snap.voters.clone();
        for (w, targets) in self.snap.wallets.iter().zip(config) {
            voters[w.voter_idx].targets = targets.clone();
        }
        voters.extend(extra);
        voters
    }

    fn own_elected(&self, e: &Election) -> bool {
        let won: HashSet<usize> = e.winners.iter().copied().collect();
        self.snap.own.iter().all(|c| won.contains(c))
    }

    fn evaluate_voters(&self, voters: &[Voter]) -> Evaluation {
        let election = seq_phragmen(&self.snap.candidates, voters, self.snap.desired);
        let mut mine_backing = vec![0.0; self.snap.candidates.len()];
        for &c in &self.snap.own {
            // Our validators' self-bond counts as our stake on that validator.
            mine_backing[c] += self.snap.candidates[c].self_bond;
        }
        for (v, a) in voters.iter().zip(&election.assignments) {
            if self.mine.contains(&v.id) {
                for &(c, w) in a { mine_backing[c] += w; }
            }
        }
        let r = self.snap.reward_per_validator;
        let mut income = 0.0;
        for &c in &election.winners {
            let comm = self.snap.candidates[c].commission;
            if self.snap.own.contains(&c) {
                income += r * comm;
            }
            if election.backing[c] > 0.0 {
                income += r * (1.0 - comm) * mine_backing[c] / election.backing[c];
            }
        }
        Evaluation { all_own_elected: self.own_elected(&election), income_per_era: income, election }
    }

    pub fn evaluate(&self, config: &[Vec<usize>]) -> Evaluation {
        self.evaluate_voters(&self.voters_with(config, None))
    }

    /// Smallest extra stake on any single non-elected rival that pushes one of our
    /// validators out, scanned upward in `MARGIN_STEP` increments (capped at `cap`).
    /// The election is not monotonic in a rival's stake, so this scans from zero
    /// rather than binary-searching.
    pub fn margin(&self, config: &[Vec<usize>], cap: f64) -> f64 {
        let base = self.evaluate(config);
        if !base.all_own_elected {
            return 0.0;
        }
        let won: HashSet<usize> = base.election.winners.iter().copied().collect();
        let rivals: Vec<usize> = (0..self.snap.candidates.len())
            .filter(|c| !won.contains(c) && self.approval(*c, config) > 0.0)
            .collect();
        let mut amount = MARGIN_STEP;
        while amount <= cap {
            for &rival in &rivals {
                let adv = Voter { id: [0xff; 32], stake: amount, targets: vec![rival] };
                let e = seq_phragmen(&self.snap.candidates, &self.voters_with(config, Some(adv)), self.snap.desired);
                if !self.own_elected(&e) {
                    return amount - MARGIN_STEP;
                }
            }
            amount += MARGIN_STEP;
        }
        cap
    }

    fn approval(&self, c: usize, config: &[Vec<usize>]) -> f64 {
        self.voters_with(config, None).iter()
            .filter(|v| v.targets.contains(&c))
            .map(|v| v.stake)
            .sum::<f64>() + self.snap.candidates[c].self_bond
    }

    /// Hill-climb over per-wallet subsets of own validators.
    /// Prefers configs where all own validators are elected with margin >= `min_margin`,
    /// then maximizes income.
    pub fn optimize(&self, min_margin: f64, current: &[u32], restarts: usize, rng: &mut Rng) -> Vec<u32> {
        let k = self.snap.own.len();
        let full = (1u32 << k) - 1;
        let mut cache = std::collections::HashMap::new();
        let mut score = |masks: &[u32]| -> (i32, f64) {
            if let Some(s) = cache.get(masks) {
                return *s;
            }
            let cfg: Vec<Vec<usize>> = masks.iter().map(|&m| self.mask_targets(m)).collect();
            let ev = self.evaluate(&cfg);
            let s = if !ev.all_own_elected {
                (-1, ev.income_per_era)
            } else {
                let m = self.margin(&cfg, min_margin);
                if m >= min_margin { (1, ev.income_per_era) } else { (0, m) }
            };
            cache.insert(masks.to_vec(), s);
            s
        };
        let better = |a: (i32, f64), b: (i32, f64)| a.0 > b.0 || (a.0 == b.0 && a.1 > b.1 + 1e-9);

        let n = self.snap.wallets.len();
        let mut best_state = vec![full; n];
        let mut best_score = (i32::MIN, 0.0);
        for restart in 0..restarts.max(2) {
            let mut state: Vec<u32> = match restart {
                0 => vec![full; n],
                1 => current.to_vec(),
                _ => (0..n).map(|_| 1 + rng.below(full as u64) as u32).collect(),
            };
            let mut cur = score(&state);
            let mut improved = true;
            while improved {
                improved = false;
                for w in 0..n {
                    for m in 1..=full {
                        if m == state[w] { continue; }
                        let prev = state[w];
                        state[w] = m;
                        let s = score(&state);
                        if better(s, cur) { cur = s; improved = true; } else { state[w] = prev; }
                    }
                }
            }
            if better(cur, best_score) {
                best_score = cur;
                best_state = state;
            }
        }
        best_state
    }

    /// Fraction of randomly perturbed scenarios in which all own validators stay elected.
    /// Other nominators: stake x U(0.9, 1.1), 5% leave, 5% re-nominate randomly.
    pub fn robustness(&self, config: &[Vec<usize>], runs: usize, rng: &mut Rng) -> f64 {
        let base = self.voters_with(config, None);
        let n = self.snap.candidates.len();
        let mut ok = 0;
        for _ in 0..runs {
            let voters: Vec<Voter> = base.iter().map(|v| {
                if self.mine.contains(&v.id) {
                    return v.clone();
                }
                let mut v = v.clone();
                v.stake *= 0.9 + 0.2 * rng.unit();
                if rng.unit() < 0.05 { v.stake = 0.0; }
                if rng.unit() < 0.05 {
                    let size = (v.targets.len() + rng.below(4) as usize).min(16).min(n);
                    let mut pool: Vec<usize> = (0..n).collect();
                    for i in 0..size {
                        let j = i + rng.below((n - i) as u64) as usize;
                        pool.swap(i, j);
                    }
                    v.targets = pool[..size].to_vec();
                }
                v
            }).collect();
            let e = seq_phragmen(&self.snap.candidates, &voters, self.snap.desired);
            if self.own_elected(&e) { ok += 1; }
        }
        ok as f64 / runs as f64
    }
}

/// Small deterministic xorshift PRNG so results are reproducible without extra dependencies.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self { Rng(seed.max(1)) }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn unit(&mut self) -> f64 { (self.next() >> 11) as f64 / (1u64 << 53) as f64 }
    pub fn below(&mut self, n: u64) -> u64 { if n == 0 { 0 } else { self.next() % n } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(i: u8, self_bond: f64) -> Candidate {
        Candidate { id: [i; 32], name: format!("c{i}"), self_bond, commission: 0.0 }
    }

    #[test]
    fn loaded_voter_splits_evenly_across_two_seats() {
        // c0 and c1 share one 100-CTC voter; rival c2 has only 40.
        // Round 1: c0 (score 1/100). Round 2: c1 = 0.02 < c2 = 0.025, so c1 wins.
        let cands = vec![cand(1, 0.0), cand(2, 0.0), cand(3, 0.0)];
        let voters = vec![
            Voter { id: [10; 32], stake: 100.0, targets: vec![0, 1] },
            Voter { id: [11; 32], stake: 40.0, targets: vec![2] },
        ];
        let e = seq_phragmen(&cands, &voters, 2);
        assert_eq!(e.winners, vec![0, 1]);
        assert!((e.backing[0] - 50.0).abs() < 1e-9);
        assert!((e.backing[1] - 50.0).abs() < 1e-9);
        assert!(e.assignments[1].is_empty());
    }

    #[test]
    fn rival_beats_our_loaded_second_candidate() {
        // Same shape but the rival has 60: c1 = 0.02 > c2 = 1/60, so the rival takes the seat.
        let cands = vec![cand(1, 0.0), cand(2, 0.0), cand(3, 0.0)];
        let voters = vec![
            Voter { id: [10; 32], stake: 100.0, targets: vec![0, 1] },
            Voter { id: [11; 32], stake: 60.0, targets: vec![2] },
        ];
        let e = seq_phragmen(&cands, &voters, 2);
        assert_eq!(e.winners, vec![0, 2]);
        assert!((e.backing[0] - 100.0).abs() < 1e-9);
        assert!((e.backing[2] - 60.0).abs() < 1e-9);
    }
}
