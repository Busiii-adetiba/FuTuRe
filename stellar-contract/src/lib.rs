#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, vec, Address, Env, IntoVal, Map,
    Symbol, Val, Vec,
};

// ── Storage keys ────────────────────────────────────────────────────────────

const ADMIN: Symbol = symbol_short!("ADMIN");
const PAUSED: Symbol = symbol_short!("PAUSED");
const MKT_CNT: Symbol = symbol_short!("MKT_CNT");
const TREASURY: Symbol = symbol_short!("TREASURY");
const TOKEN: Symbol = symbol_short!("TOKEN");

// ── State TTL (ledgers; ~5s per ledger → 17_280 ledgers per day) ─────────────

const DAY_IN_LEDGERS: u32 = 17_280;
/// Instance storage is extended once its TTL falls below this threshold.
pub const INSTANCE_BUMP_THRESHOLD: u32 = 7 * DAY_IN_LEDGERS;
/// Instance storage TTL target after an extension.
pub const INSTANCE_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;
/// Market / position entries are extended once their TTL falls below this threshold.
pub const PERSISTENT_BUMP_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
/// Market / position TTL target after an extension.
pub const PERSISTENT_EXTEND_TO: u32 = 120 * DAY_IN_LEDGERS;

// ── Types ────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum MarketStatus {
    Open,
    Closed,
    Disputed,
    Resolved,
    Cancelled,
    EmergencyResolved,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Market {
    pub creator: Address,
    pub question: soroban_sdk::String,
    pub yes_shares: i128,
    pub no_shares: i128,
    pub yes_pool: i128,
    pub no_pool: i128,
    /// Total asset value (XLM/tokens) deposited into the LP pool.
    pub lp_pool: i128,
    /// Accumulated trading fees claimable by LPs.
    pub lp_fees: i128,
    pub status: MarketStatus,
    pub outcome: Option<bool>, // true = YES won
    pub dispute_bond: i128,
    pub disputer: Option<Address>,
    pub oracle: Option<Address>,
    /// Total LP shares outstanding. Tracked separately from `lp_pool`
    /// so that the share/value ratio can diverge as trading changes pool value.
    pub total_lp_shares: i128,
    /// Ledger timestamp after which trading stops and the oracle may report.
    pub close_time: u64,
    /// Ledger timestamp after which an unreported market can be cancelled by anyone.
    pub resolution_deadline: u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Position {
    pub yes_shares: i128,
    pub no_shares: i128,
    pub lp_shares: i128,
    pub split_tokens: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct RedeemOutcome {
    pub market_id: u32,
    pub payout: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct RedeemFailure {
    pub market_id: u32,
    pub error: Error,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchRedeemResult {
    pub successes: Vec<RedeemOutcome>,
    pub failures: Vec<RedeemFailure>,
    pub total_payout: i128,
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    MarketNotFound = 4,
    MarketNotOpen = 5,
    MarketNotClosed = 6,
    MarketNotResolved = 7,
    MarketAlreadyCancelled = 8,
    SlippageExceeded = 9,
    InsufficientFunds = 10,
    ContractPaused = 11,
    InvalidOutcome = 12,
    DisputeWindowOpen = 13,
    NothingToRedeem = 14,
    InvalidAmount = 15,
    TradingClosed = 16,
    MarketNotExpired = 17,
    InvalidDeadline = 18,
    ResolutionDeadlineNotReached = 19,
}

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct PredictionMarket;

#[contractimpl]
impl PredictionMarket {
    // ── Admin ────────────────────────────────────────────────────────────────

    pub fn init(env: Env, admin: Address, treasury: Address, token: Address) -> Result<(), Error> {
        if env.storage().instance().has(&ADMIN) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&ADMIN, &admin);
        env.storage().instance().set(&TREASURY, &treasury);
        env.storage().instance().set(&TOKEN, &token);
        env.storage().instance().set(&PAUSED, &false);
        env.storage().instance().set(&MKT_CNT, &0u32);
        Self::extend_instance(&env);
        Self::emit_admin(&env, symbol_short!("init"), (admin, treasury));
        Ok(())
    }

    pub fn pause(env: Env, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&PAUSED, &true);
        Self::emit_admin(&env, symbol_short!("paused"), caller);
        Ok(())
    }

    pub fn unpause(env: Env, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&PAUSED, &false);
        Self::emit_admin(&env, symbol_short!("unpaused"), caller);
        Ok(())
    }

    // ── Market lifecycle ─────────────────────────────────────────────────────

    pub fn create_market(
        env: Env,
        creator: Address,
        question: soroban_sdk::String,
        oracle: Address,
        close_time: u64,
        resolution_deadline: u64,
    ) -> Result<u32, Error> {
        creator.require_auth();
        Self::require_not_paused(&env)?;
        if close_time <= env.ledger().timestamp() || resolution_deadline <= close_time {
            return Err(Error::InvalidDeadline);
        }
        let id: u32 = env.storage().instance().get(&MKT_CNT).unwrap_or(0);
        let market = Market {
            creator: creator.clone(),
            question,
            yes_shares: 0,
            no_shares: 0,
            yes_pool: 0,
            no_pool: 0,
            lp_pool: 0,
            lp_fees: 0,
            status: MarketStatus::Open,
            outcome: None,
            dispute_bond: 0,
            disputer: None,
            oracle: Some(oracle.clone()),
            total_lp_shares: 0,
            close_time,
            resolution_deadline,
        };
        Self::save_market(&env, id, &market);
        env.storage().instance().set(&MKT_CNT, &(id + 1));
        Self::emit(
            &env,
            symbol_short!("created"),
            (id, creator, oracle),
        );
        Ok(id)
    }

    pub fn seed_market(env: Env, caller: Address, market_id: u32, amount: i128) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_trading_open(&env, &market)?;
        Self::require_positive(amount)?;
        // Seeding credits `amount` to both the YES and NO pools, so the
        // contract must take custody of `2 * amount` to keep the pools backed.
        Self::transfer_in(&env, &caller, amount * 2)?;
        market.yes_pool += amount;
        market.no_pool += amount;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("seeded"), (market_id, caller, amount));
        Ok(())
    }

    pub fn close_market(env: Env, caller: Address, market_id: u32) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_admin_or_creator(&env, &caller, &market.creator)?;
        // Only the admin may halt trading before the scheduled close time.
        if env.ledger().timestamp() < market.close_time {
            Self::require_admin(&env, &caller)?;
        }
        market.status = MarketStatus::Closed;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("closed"), (market_id, caller));
        Ok(())
    }

    pub fn oracle_report(
        env: Env,
        caller: Address,
        market_id: u32,
        outcome: bool,
    ) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Closed)?;
        // Verify caller is the oracle
        if market.oracle != Some(caller.clone()) {
            return Err(Error::Unauthorized);
        }
        if env.ledger().timestamp() < market.close_time {
            return Err(Error::MarketNotExpired);
        }
        market.outcome = Some(outcome);
        // Status stays Closed; finalize moves it to Resolved after dispute window
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("reported"), (market_id, caller, outcome));
        Ok(())
    }

    pub fn dispute(
        env: Env,
        disputer: Address,
        market_id: u32,
        bond: i128,
    ) -> Result<(), Error> {
        disputer.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Closed)?;
        if market.outcome.is_none() {
            return Err(Error::InvalidOutcome);
        }
        market.status = MarketStatus::Disputed;
        market.disputer = Some(disputer.clone());
        market.dispute_bond = bond;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("disputed"), (market_id, disputer, bond));
        Ok(())
    }

    /// Admin upholds dispute → emergency resolve
    pub fn admin_uphold_dispute(
        env: Env,
        caller: Address,
        market_id: u32,
        new_outcome: bool,
    ) -> Result<(), Error> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Disputed)?;
        market.outcome = Some(new_outcome);
        market.status = MarketStatus::EmergencyResolved;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("upheld"), (market_id, caller, new_outcome));
        Ok(())
    }

    /// Admin rejects dispute → slash bond to treasury
    pub fn admin_reject_dispute(
        env: Env,
        caller: Address,
        market_id: u32,
    ) -> Result<(), Error> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Disputed)?;
        // Slash bond: add to treasury balance (tracked in market for simplicity)
        let _treasury: Address = env.storage().instance().get(&TREASURY).ok_or(Error::NotInitialized)?;
        let key = Self::treasury_key(&env);
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let slashed = market.dispute_bond;
        env.storage().persistent().set(&key, &(current + slashed));
        Self::extend_persistent(&env, &key);
        market.dispute_bond = 0;
        market.disputer = None;
        // Revert to Closed so finalize can proceed
        market.status = MarketStatus::Closed;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("rejected"), (market_id, caller, slashed));
        Ok(())
    }

    /// Called after dispute window passes (or immediately if no dispute)
    pub fn finalize(env: Env, market_id: u32) -> Result<(), Error> {
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        if market.status != MarketStatus::Closed && market.status != MarketStatus::EmergencyResolved {
            return Err(Error::MarketNotClosed);
        }
        if market.outcome.is_none() {
            return Err(Error::InvalidOutcome);
        }
        market.status = MarketStatus::Resolved;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("resolved"), (market_id, market.outcome));
        Ok(())
    }

    pub fn cancel_market(env: Env, caller: Address, market_id: u32) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_admin_or_creator(&env, &caller, &market.creator)?;
        if market.status == MarketStatus::Cancelled {
            return Err(Error::MarketAlreadyCancelled);
        }
        market.status = MarketStatus::Cancelled;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("cancelled"), (market_id, caller));
        Ok(())
    }

    /// Permissionless escape hatch: if the oracle has not reported by
    /// `resolution_deadline`, anyone may cancel the market so participants can
    /// reclaim their funds through `redeem`.
    pub fn emergency_timeout_refund(env: Env, market_id: u32) -> Result<(), Error> {
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        if market.status != MarketStatus::Open && market.status != MarketStatus::Closed {
            return Err(Error::MarketNotOpen);
        }
        if market.outcome.is_some() {
            return Err(Error::InvalidOutcome);
        }
        if env.ledger().timestamp() <= market.resolution_deadline {
            return Err(Error::ResolutionDeadlineNotReached);
        }
        market.status = MarketStatus::Cancelled;
        Self::save_market(&env, market_id, &market);
        Self::emit(&env, symbol_short!("timeout"), market_id);
        Ok(())
    }

    // ── Trading ──────────────────────────────────────────────────────────────

    pub fn buy_yes(
        env: Env,
        buyer: Address,
        market_id: u32,
        amount: i128,
        min_shares_out: i128,
    ) -> Result<i128, Error> {
        buyer.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_trading_open(&env, &market)?;
        Self::require_positive(amount)?;
        let shares = Self::calc_shares(amount, market.yes_pool, market.no_pool);
        if shares < min_shares_out {
            return Err(Error::SlippageExceeded);
        }
        Self::transfer_in(&env, &buyer, amount)?;
        market.yes_pool += amount;
        market.yes_shares += shares;
        Self::save_market(&env, market_id, &market);
        let mut pos = Self::load_position(&env, market_id, &buyer);
        pos.yes_shares += shares;
        Self::save_position(&env, market_id, &buyer, &pos);
        Self::emit(
            &env,
            symbol_short!("traded"),
            (market_id, buyer, true, amount, shares),
        );
        Ok(shares)
    }

    pub fn buy_no(
        env: Env,
        buyer: Address,
        market_id: u32,
        amount: i128,
        min_shares_out: i128,
    ) -> Result<i128, Error> {
        buyer.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_trading_open(&env, &market)?;
        Self::require_positive(amount)?;
        let shares = Self::calc_shares(amount, market.no_pool, market.yes_pool);
        if shares < min_shares_out {
            return Err(Error::SlippageExceeded);
        }
        Self::transfer_in(&env, &buyer, amount)?;
        market.no_pool += amount;
        market.no_shares += shares;
        Self::save_market(&env, market_id, &market);
        let mut pos = Self::load_position(&env, market_id, &buyer);
        pos.no_shares += shares;
        Self::save_position(&env, market_id, &buyer, &pos);
        Self::emit(
            &env,
            symbol_short!("traded"),
            (market_id, buyer, false, amount, shares),
        );
        Ok(shares)
    }

    // ── Redeem ───────────────────────────────────────────────────────────────

    pub fn redeem(env: Env, redeemer: Address, market_id: u32) -> Result<i128, Error> {
        redeemer.require_auth();
        Self::require_not_paused(&env)?;
        let market = Self::load_market(&env, market_id)?;
        if market.status == MarketStatus::Cancelled {
            return Self::refund_cancelled(&env, &redeemer, market_id, &market);
        }
        if market.status != MarketStatus::Resolved && market.status != MarketStatus::EmergencyResolved {
            return Err(Error::MarketNotResolved);
        }
        let mut pos = Self::load_position(&env, market_id, &redeemer);
        let winning_shares = match market.outcome {
            Some(true) => pos.yes_shares,
            Some(false) => pos.no_shares,
            None => return Err(Error::InvalidOutcome),
        };
        if winning_shares == 0 {
            return Err(Error::NothingToRedeem);
        }
        let total_pool = market.yes_pool + market.no_pool;
        let total_winning = if market.outcome == Some(true) {
            market.yes_shares
        } else {
            market.no_shares
        };
        let payout = if total_winning > 0 {
            (winning_shares * total_pool) / total_winning
        } else {
            0
        };
        // Clear position
        if market.outcome == Some(true) {
            pos.yes_shares = 0;
        } else {
            pos.no_shares = 0;
        }
        Self::save_position(&env, market_id, &redeemer, &pos);
        Self::transfer_out(&env, &redeemer, payout)?;
        Self::emit(&env, symbol_short!("redeemed"), (market_id, redeemer, payout));
        Ok(payout)
    }

    /// Batch redeem across multiple markets
    /// Returns per-market success/failure information instead of silently skipping failed markets.
    pub fn batch_redeem(env: Env, redeemer: Address, market_ids: Vec<u32>) -> Result<BatchRedeemResult, Error> {
        redeemer.require_auth();
        let mut successes: Vec<RedeemOutcome> = Vec::new();
        let mut failures: Vec<RedeemFailure> = Vec::new();
        let mut total_payout: i128 = 0;

        for id in market_ids.iter() {
            match Self::redeem(env.clone(), redeemer.clone(), id) {
                Ok(payout) => {
                    total_payout += payout;
                    successes.push_back(RedeemOutcome {
                        market_id: id,
                        payout,
                    });
                }
                Err(error) => {
                    failures.push_back(RedeemFailure {
                        market_id: id,
                        error,
                    });
                }
            }
        }

        Ok(BatchRedeemResult {
            successes,
            failures,
            total_payout,
        })
    }

    // ── LP ───────────────────────────────────────────────────────────────────

    pub fn add_liquidity(
        env: Env,
        provider: Address,
        market_id: u32,
        amount: i128,
    ) -> Result<i128, Error> {
        provider.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_trading_open(&env, &market)?;
        Self::require_positive(amount)?;
        Self::transfer_in(&env, &provider, amount)?;

        // Mint LP shares proportional to the provider's share of the pool value.
        // • First provider (empty pool): shares == amount (1:1 bootstrap).
        // • Subsequent providers: shares = amount * total_lp_shares / lp_pool,
        //   so later depositors into a more-valuable pool receive fewer shares
        //   for the same nominal deposit, correctly diluting their claim.
        let lp_shares = if market.lp_pool == 0 || market.total_lp_shares == 0 {
            amount
        } else {
            (amount * market.total_lp_shares) / market.lp_pool
        };

        market.lp_pool += amount;
        market.total_lp_shares += lp_shares;
        market.yes_pool += amount / 2;
        market.no_pool += amount / 2;
        Self::save_market(&env, market_id, &market);

        let mut pos = Self::load_position(&env, market_id, &provider);
        pos.lp_shares += lp_shares;
        Self::save_position(&env, market_id, &provider, &pos);
        Self::emit_liquidity(
            &env,
            symbol_short!("added"),
            (market_id, provider, amount, lp_shares),
        );
        Ok(lp_shares)
    }

    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        market_id: u32,
        lp_shares: i128,
    ) -> Result<i128, Error> {
        provider.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        let mut pos = Self::load_position(&env, market_id, &provider);
        if pos.lp_shares < lp_shares {
            return Err(Error::InsufficientFunds);
        }

        // Payout = redeemed_shares * current_pool_value / total_shares_outstanding.
        // This means an LP that deposited when the pool was large and trading has
        // since shifted yes/no prices will receive a payout reflecting the pool's
        // current total value — not just their original deposit.
        let payout = if market.total_lp_shares > 0 {
            (lp_shares * market.lp_pool) / market.total_lp_shares
        } else {
            lp_shares
        };

        market.lp_pool -= payout;
        market.total_lp_shares -= lp_shares;
        pos.lp_shares -= lp_shares;
        Self::save_market(&env, market_id, &market);
        Self::save_position(&env, market_id, &provider, &pos);
        Self::transfer_out(&env, &provider, payout)?;
        Self::emit_liquidity(
            &env,
            symbol_short!("removed"),
            (market_id, provider, lp_shares, payout),
        );
        Ok(payout)
    }

    pub fn claim_lp_fees(
        env: Env,
        provider: Address,
        market_id: u32,
    ) -> Result<i128, Error> {
        provider.require_auth();
        Self::require_not_paused(&env)?;
        let mut market = Self::load_market(&env, market_id)?;
        let pos = Self::load_position(&env, market_id, &provider);
        if pos.lp_shares == 0 {
            return Err(Error::NothingToRedeem);
        }
        // Fee share = provider_lp_shares / total_lp_shares_outstanding * accumulated_fees.
        // Use total_lp_shares (share units) not lp_pool (asset units) as the denominator
        // so that the fee split is proportional to ownership, not to deposit size.
        let total_lp = market.total_lp_shares.max(1);
        let fee_share = (pos.lp_shares * market.lp_fees) / total_lp;
        market.lp_fees -= fee_share;
        Self::save_market(&env, market_id, &market);
        Self::transfer_out(&env, &provider, fee_share)?;
        Self::emit_liquidity(&env, symbol_short!("claimed"), (market_id, provider, fee_share));
        Ok(fee_share)
    }

    // ── Split / Merge ────────────────────────────────────────────────────────

    pub fn split(
        env: Env,
        caller: Address,
        market_id: u32,
        amount: i128,
    ) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        Self::require_positive(amount)?;
        Self::transfer_in(&env, &caller, amount)?;
        let mut pos = Self::load_position(&env, market_id, &caller);
        pos.yes_shares += amount;
        pos.no_shares += amount;
        pos.split_tokens += amount;
        Self::save_position(&env, market_id, &caller, &pos);
        Self::emit(&env, symbol_short!("split"), (market_id, caller, amount));
        Ok(())
    }

    pub fn merge(
        env: Env,
        caller: Address,
        market_id: u32,
        amount: i128,
    ) -> Result<(), Error> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let market = Self::load_market(&env, market_id)?;
        Self::require_status(&market, &MarketStatus::Open)?;
        let mut pos = Self::load_position(&env, market_id, &caller);
        if pos.yes_shares < amount || pos.no_shares < amount {
            return Err(Error::InsufficientFunds);
        }
        pos.yes_shares -= amount;
        pos.no_shares -= amount;
        pos.split_tokens -= amount.min(pos.split_tokens);
        Self::save_position(&env, market_id, &caller, &pos);
        Self::transfer_out(&env, &caller, amount)?;
        Self::emit(&env, symbol_short!("merged"), (market_id, caller, amount));
        Ok(())
    }

    // ── Views ────────────────────────────────────────────────────────────────

    pub fn get_market(env: Env, market_id: u32) -> Result<Market, Error> {
        Self::load_market(&env, market_id)
    }

    pub fn get_position(env: Env, market_id: u32, user: Address) -> Position {
        Self::load_position(&env, market_id, &user)
    }

    pub fn get_treasury_balance(env: Env) -> i128 {
        let key = Self::treasury_key(&env);
        let bal = env.storage().persistent().get(&key).unwrap_or(0);
        if bal != 0 {
            Self::extend_persistent(&env, &key);
        }
        bal
    }

    pub fn get_token(env: Env) -> Result<Address, Error> {
        Self::extend_instance(&env);
        env.storage().instance().get(&TOKEN).ok_or(Error::NotInitialized)
    }

    // ── Events ───────────────────────────────────────────────────────────────
    //
    // Every state-mutating function above publishes an on-chain event via one
    // of these helpers so off-chain indexers can subscribe to contract
    // activity instead of re-scanning storage. Topics follow a
    // `(category, action)` scheme; see `stellar-contract/README.md` for the
    // full topic/payload table.

    /// Publish an event under the `("market", action)` topic.
    fn emit(env: &Env, action: Symbol, data: impl IntoVal<Env, Val>) {
        env.events().publish((symbol_short!("market"), action), data);
    }

    /// Publish an event under the `("liquidity", action)` topic.
    fn emit_liquidity(env: &Env, action: Symbol, data: impl IntoVal<Env, Val>) {
        env.events().publish((symbol_short!("liquidity"), action), data);
    }

    /// Publish an event under the `("admin", action)` topic.
    fn emit_admin(env: &Env, action: Symbol, data: impl IntoVal<Env, Val>) {
        env.events().publish((symbol_short!("admin"), action), data);
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
        let admin: Address = env.storage().instance().get(&ADMIN).ok_or(Error::NotInitialized)?;
        if *caller != admin {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }

    fn require_admin_or_creator(env: &Env, caller: &Address, creator: &Address) -> Result<(), Error> {
        if Self::require_admin(env, caller).is_ok() || caller == creator {
            return Ok(());
        }
        Err(Error::Unauthorized)
    }

    fn require_not_paused(env: &Env) -> Result<(), Error> {
        Self::extend_instance(env);
        let paused: bool = env.storage().instance().get(&PAUSED).unwrap_or(false);
        if paused {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    fn require_status(market: &Market, expected: &MarketStatus) -> Result<(), Error> {
        if market.status != *expected {
            return Err(match expected {
                MarketStatus::Open => Error::MarketNotOpen,
                MarketStatus::Closed => Error::MarketNotClosed,
                MarketStatus::Resolved => Error::MarketNotResolved,
                _ => Error::MarketNotOpen,
            });
        }
        Ok(())
    }

    fn require_trading_open(env: &Env, market: &Market) -> Result<(), Error> {
        if env.ledger().timestamp() >= market.close_time {
            return Err(Error::TradingClosed);
        }
        Ok(())
    }

    fn require_positive(amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        Ok(())
    }

    fn token_client(env: &Env) -> Result<token::Client<'_>, Error> {
        let token: Address = env.storage().instance().get(&TOKEN).ok_or(Error::NotInitialized)?;
        Ok(token::Client::new(env, &token))
    }

    /// Move `amount` of the market token from `from` into contract custody.
    /// Panics (reverting the whole invocation) if `from` lacks the balance.
    fn transfer_in(env: &Env, from: &Address, amount: i128) -> Result<(), Error> {
        if amount > 0 {
            Self::token_client(env)?.transfer(from, &env.current_contract_address(), &amount);
        }
        Ok(())
    }

    /// Pay `amount` of the market token out of contract custody to `to`.
    fn transfer_out(env: &Env, to: &Address, amount: i128) -> Result<(), Error> {
        if amount > 0 {
            Self::token_client(env)?.transfer(&env.current_contract_address(), to, &amount);
        }
        Ok(())
    }

    fn extend_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_BUMP_THRESHOLD, INSTANCE_EXTEND_TO);
    }

    fn extend_persistent<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
        env.storage()
            .persistent()
            .extend_ttl(key, PERSISTENT_BUMP_THRESHOLD, PERSISTENT_EXTEND_TO);
    }

    fn calc_shares(amount: i128, own_pool: i128, other_pool: i128) -> i128 {
        // Simple CPMM: shares = amount * other_pool / (own_pool + amount)
        if own_pool == 0 && other_pool == 0 {
            return amount;
        }
        let denom = own_pool + amount;
        if denom == 0 {
            return 0;
        }
        (amount * (other_pool + own_pool)) / denom
    }

    fn market_key(env: &Env, id: u32) -> soroban_sdk::Val {
        let _ = env;
        soroban_sdk::Val::from(id)
    }

    fn load_market(env: &Env, id: u32) -> Result<Market, Error> {
        let key = (symbol_short!("MKT"), id);
        let market = env.storage().persistent().get(&key).ok_or(Error::MarketNotFound)?;
        Self::extend_persistent(env, &key);
        Ok(market)
    }

    fn save_market(env: &Env, id: u32, market: &Market) {
        let key = (symbol_short!("MKT"), id);
        env.storage().persistent().set(&key, market);
        Self::extend_persistent(env, &key);
    }

    fn load_position(env: &Env, market_id: u32, user: &Address) -> Position {
        let key = (symbol_short!("POS"), market_id, user.clone());
        match env.storage().persistent().get(&key) {
            Some(pos) => {
                Self::extend_persistent(env, &key);
                pos
            }
            None => Position {
                yes_shares: 0,
                no_shares: 0,
                lp_shares: 0,
                split_tokens: 0,
            },
        }
    }

    fn save_position(env: &Env, market_id: u32, user: &Address, pos: &Position) {
        let key = (symbol_short!("POS"), market_id, user.clone());
        env.storage().persistent().set(&key, pos);
        Self::extend_persistent(env, &key);
    }

    fn treasury_key(env: &Env) -> Symbol {
        let _ = env;
        symbol_short!("TRES_BAL")
    }

    fn refund_cancelled(
        env: &Env,
        redeemer: &Address,
        market_id: u32,
        market: &Market,
    ) -> Result<i128, Error> {
        let mut pos = Self::load_position(env, market_id, redeemer);
        // 1:1 refund; a split YES+NO pair was backed by a single unit of
        // collateral, so count it once.
        let refund = pos.yes_shares + pos.no_shares - pos.split_tokens;
        if refund <= 0 {
            return Err(Error::NothingToRedeem);
        }
        pos.yes_shares = 0;
        pos.no_shares = 0;
        pos.split_tokens = 0;
        Self::save_position(env, market_id, redeemer, &pos);
        Self::transfer_out(env, redeemer, refund)?;
        Self::emit(env, symbol_short!("refunded"), (market_id, redeemer.clone(), refund));
        let _ = market;
        Ok(refund)
    }
}
