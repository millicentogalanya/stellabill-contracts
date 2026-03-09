//! Subscription lifecycle: create, deposit, withdraw, cancel.
//!
//! See `docs/subscription_lifecycle.md` for the full lifecycle and state machine.
//!
//! **PRs that only change subscription lifecycle or billing should edit this file only.**
//!
//! # Reentrancy Protection
//!
//! This module contains two critical external calls to the token contract:
//! - `do_deposit_funds`: transfers tokens FROM subscriber TO contract
//! - `do_withdraw_subscriber_funds`: transfers tokens FROM contract TO subscriber
//!
//! Both functions follow the **Checks-Effects-Interactions (CEI)** pattern:
//! 1. **Checks**: Validate inputs and authorization
//! 2. **Effects**: Update internal contract state (prepaid_balance) in storage
//! 3. **Interactions**: Call token.transfer() AFTER state is persisted
//!
//! This ordering ensures that even if the token contract calls back into our contract,
//! the contract state will already be consistent and the attacker cannot exploit the
//! temporal inconsistency.
//!
//! See `docs/reentrancy.md` for full details on reentrancy threats and mitigations.

use crate::queries::get_subscription;
use crate::safe_math::{safe_add_balance, validate_non_negative};
use crate::state_machine::validate_status_transition;
use crate::statements::append_statement;
use crate::types::{
    BillingChargeKind, DataKey, Error, PlanTemplate, PlanTemplateUpdatedEvent, Subscription,
    SubscriptionMigratedEvent, SubscriptionStatus,
};
use soroban_sdk::{symbol_short, Address, Env, Symbol, Vec};

#[allow(dead_code)]
pub fn next_id(env: &Env) -> u32 {
    let key = Symbol::new(env, "next_id");
    let storage = env.storage().instance();
    let id: u32 = storage.get(&key).unwrap_or(0);
    storage.set(&key, &(id + 1));
    id
}

pub fn next_plan_id(env: &Env) -> u32 {
    let key = Symbol::new(env, "next_plan_id");
    let id: u32 = env.storage().instance().get(&key).unwrap_or(0);
    env.storage().instance().set(&key, &(id + 1));
    id
}

pub fn get_plan_template(env: &Env, plan_template_id: u32) -> Result<PlanTemplate, Error> {
    let key = (Symbol::new(env, "plan"), plan_template_id);
    env.storage().instance().get(&key).ok_or(Error::NotFound)
}

fn sub_plan_key(env: &Env, subscription_id: u32) -> (Symbol, u32) {
    (Symbol::new(env, "sub_plan"), subscription_id)
}

pub fn do_create_subscription(
    env: &Env,
    subscriber: Address,
    merchant: Address,
    amount: i128,
    interval_seconds: u64,
    usage_enabled: bool,
    lifetime_cap: Option<i128>,
) -> Result<u32, Error> {
    let token = crate::admin::get_token(env)?;
    do_create_subscription_with_token(
        env,
        subscriber,
        merchant,
        token,
        amount,
        interval_seconds,
        usage_enabled,
        lifetime_cap,
    )
}

pub fn do_create_subscription_with_token(
    env: &Env,
    subscriber: Address,
    merchant: Address,
    token: Address,
    amount: i128,
    interval_seconds: u64,
    usage_enabled: bool,
    lifetime_cap: Option<i128>,
) -> Result<u32, Error> {
    subscriber.require_auth();
    validate_non_negative(amount)?;
    if !crate::admin::is_token_accepted(env, &token) {
        return Err(Error::InvalidInput);
    }

    // Validate lifetime_cap if provided
    if let Some(cap) = lifetime_cap {
        if cap <= 0 {
            return Err(Error::InvalidAmount);
        }
    }

    let sub = Subscription {
        subscriber: subscriber.clone(),
        merchant: merchant.clone(),
        token: token.clone(),
        amount,
        interval_seconds,
        last_payment_timestamp: env.ledger().timestamp(),
        status: SubscriptionStatus::Active,
        prepaid_balance: 0i128,
        usage_enabled,
        lifetime_cap,
        lifetime_charged: 0i128,
    };

    // Allocate ID with overflow / limit guard.
    let key = Symbol::new(env, "next_id");
    let id: u32 = env.storage().instance().get(&key).unwrap_or(0);
    if id == crate::MAX_SUBSCRIPTION_ID {
        return Err(Error::SubscriptionLimitReached);
    }
    env.storage().instance().set(&key, &(id + 1));

    env.storage().instance().set(&id, &sub);

    // Maintain merchant → subscription-ID index
    let merchant_key = DataKey::MerchantSubs(sub.merchant.clone());
    let mut ids: Vec<u32> = env
        .storage()
        .instance()
        .get(&merchant_key)
        .unwrap_or(Vec::new(env));
    ids.push_back(id);
    env.storage().instance().set(&merchant_key, &ids);

    // Maintain token -> subscription-ID index
    let token_key = (Symbol::new(env, "token_subs"), token);
    let mut token_ids: Vec<u32> = env
        .storage()
        .instance()
        .get(&token_key)
        .unwrap_or(Vec::new(env));
    token_ids.push_back(id);
    env.storage().instance().set(&token_key, &token_ids);

    env.events().publish(
        (symbol_short!("created"), id),
        (
            subscriber.clone(),
            merchant.clone(),
            amount,
            interval_seconds,
            lifetime_cap,
        ),
    );

    Ok(id)
}

pub fn do_deposit_funds(
    env: &Env,
    subscription_id: u32,
    subscriber: Address,
    amount: i128,
) -> Result<(), Error> {
    subscriber.require_auth();

    // ──────────────────────────────────────────────────────────────────────────
    // CHECKS: Validate all preconditions before any state mutations
    // ──────────────────────────────────────────────────────────────────────────
    let min_topup: i128 = crate::admin::get_min_topup(env)?;
    if amount < min_topup {
        return Err(Error::BelowMinimumTopup);
    }
    validate_non_negative(amount)?;

    let mut sub = get_subscription(env, subscription_id)?;
    let token_addr = sub.token.clone();

    // ──────────────────────────────────────────────────────────────────────────
    // EFFECTS: Update internal state before external interactions (CEI pattern)
    // ──────────────────────────────────────────────────────────────────────────
    sub.prepaid_balance = safe_add_balance(sub.prepaid_balance, amount)?;
    env.storage().instance().set(&subscription_id, &sub);

    // ──────────────────────────────────────────────────────────────────────────
    // INTERACTIONS: Only after internal state is consistent, call token contract
    // ──────────────────────────────────────────────────────────────────────────
    let token_client = soroban_sdk::token::Client::new(env, &token_addr);
    token_client.transfer(&subscriber, &env.current_contract_address(), &amount);

    // Emit event after successful transfer
    env.events().publish(
        (Symbol::new(env, "deposited"), subscription_id),
        (subscriber, amount, sub.prepaid_balance),
    );
    Ok(())
}

pub fn do_cancel_subscription(
    env: &Env,
    subscription_id: u32,
    authorizer: Address,
) -> Result<(), Error> {
    authorizer.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;

    if authorizer != sub.subscriber && authorizer != sub.merchant {
        return Err(Error::Forbidden);
    }

    validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
    sub.status = SubscriptionStatus::Cancelled;

    env.storage().instance().set(&subscription_id, &sub);
    Ok(())
}

pub fn do_pause_subscription(
    env: &Env,
    subscription_id: u32,
    authorizer: Address,
) -> Result<(), Error> {
    authorizer.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;
    validate_status_transition(&sub.status, &SubscriptionStatus::Paused)?;
    sub.status = SubscriptionStatus::Paused;

    env.storage().instance().set(&subscription_id, &sub);
    Ok(())
}

pub fn do_resume_subscription(
    env: &Env,
    subscription_id: u32,
    authorizer: Address,
) -> Result<(), Error> {
    authorizer.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;
    validate_status_transition(&sub.status, &SubscriptionStatus::Active)?;
    sub.status = SubscriptionStatus::Active;

    env.storage().instance().set(&subscription_id, &sub);
    Ok(())
}

/// Merchant-initiated one-off charge: debits `amount` from the subscription's prepaid balance.
///
/// Requires merchant auth; the subscription's merchant must match the caller.
/// Subscription must be Active or Paused. Amount must be positive and not exceed prepaid_balance.
///
/// One-off charges also count toward the lifetime cap when one is configured.
pub fn do_charge_one_off(
    env: &Env,
    subscription_id: u32,
    merchant: Address,
    amount: i128,
) -> Result<(), Error> {
    merchant.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;
    if sub.merchant != merchant {
        return Err(Error::Unauthorized);
    }
    if sub.status != SubscriptionStatus::Active && sub.status != SubscriptionStatus::Paused {
        return Err(Error::NotActive);
    }
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }
    if sub.prepaid_balance < amount {
        return Err(Error::InsufficientPrepaidBalance);
    }

    // Enforce lifetime cap for one-off charges
    if let Some(cap) = sub.lifetime_cap {
        let new_charged = sub
            .lifetime_charged
            .checked_add(amount)
            .ok_or(Error::Overflow)?;
        if new_charged > cap {
            return Err(Error::LifetimeCapReached);
        }
        sub.lifetime_charged = new_charged;
    }

    sub.prepaid_balance = sub
        .prepaid_balance
        .checked_sub(amount)
        .ok_or(Error::Overflow)?;

    env.storage().instance().set(&subscription_id, &sub);
    append_statement(
        env,
        subscription_id,
        amount,
        sub.merchant.clone(),
        BillingChargeKind::OneOff,
        env.ledger().timestamp(),
        env.ledger().timestamp(),
    );
    Ok(())
}

pub fn do_withdraw_subscriber_funds(
    env: &Env,
    subscription_id: u32,
    subscriber: Address,
) -> Result<(), Error> {
    subscriber.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;

    if subscriber != sub.subscriber {
        return Err(Error::Forbidden);
    }

    if sub.status != SubscriptionStatus::Cancelled {
        return Err(Error::InvalidStatusTransition);
    }

    let amount_to_refund = sub.prepaid_balance;
    if amount_to_refund > 0 {
        sub.prepaid_balance = 0;
        env.storage().instance().set(&subscription_id, &sub);

        let token_addr = sub.token.clone();
        let token_client = soroban_sdk::token::Client::new(env, &token_addr);

        token_client.transfer(
            &env.current_contract_address(),
            &subscriber,
            &amount_to_refund,
        );
    }

    Ok(())
}

pub fn do_create_plan_template(
    env: &Env,
    merchant: Address,
    amount: i128,
    interval_seconds: u64,
    usage_enabled: bool,
    lifetime_cap: Option<i128>,
) -> Result<u32, Error> {
    merchant.require_auth();

    // Validate lifetime_cap if provided
    if let Some(cap) = lifetime_cap {
        if cap <= 0 {
            return Err(Error::InvalidAmount);
        }
    }

    let token = crate::admin::get_token(env)?;
    let plan_id = next_plan_id(env);
    let plan = PlanTemplate {
        merchant,
        token,
        amount,
        interval_seconds,
        usage_enabled,
        lifetime_cap,
        template_key: plan_id,
        version: 1,
    };

    let key = (Symbol::new(env, "plan"), plan_id);
    env.storage().instance().set(&key, &plan);

    Ok(plan_id)
}

pub fn do_create_plan_template_with_token(
    env: &Env,
    merchant: Address,
    token: Address,
    amount: i128,
    interval_seconds: u64,
    usage_enabled: bool,
    lifetime_cap: Option<i128>,
) -> Result<u32, Error> {
    merchant.require_auth();
    if !crate::admin::is_token_accepted(env, &token) {
        return Err(Error::InvalidInput);
    }
    if let Some(cap) = lifetime_cap {
        if cap <= 0 {
            return Err(Error::InvalidAmount);
        }
    }

    let plan_id = next_plan_id(env);
    let plan = PlanTemplate {
        merchant,
        token,
        amount,
        interval_seconds,
        usage_enabled,
        lifetime_cap,
        template_key: plan_id,
        version: 1,
    };

    let key = (Symbol::new(env, "plan"), plan_id);
    env.storage().instance().set(&key, &plan);
    Ok(plan_id)
}

pub fn do_create_subscription_from_plan(
    env: &Env,
    subscriber: Address,
    plan_template_id: u32,
) -> Result<u32, Error> {
    subscriber.require_auth();

    let plan = get_plan_template(env, plan_template_id)?;

    let key = Symbol::new(env, "next_id");
    let id: u32 = env.storage().instance().get(&key).unwrap_or(0);
    env.storage().instance().set(&key, &(id + 1));

    let sub = Subscription {
        subscriber: subscriber.clone(),
        merchant: plan.merchant.clone(),
        token: plan.token.clone(),
        amount: plan.amount,
        interval_seconds: plan.interval_seconds,
        last_payment_timestamp: env.ledger().timestamp(),
        status: SubscriptionStatus::Active,
        prepaid_balance: 0i128,
        usage_enabled: plan.usage_enabled,
        lifetime_cap: plan.lifetime_cap,
        lifetime_charged: 0i128,
    };

    env.storage().instance().set(&id, &sub);

    // Persist linkage between subscription and the plan template it was created from.
    let sub_plan_storage_key = sub_plan_key(env, id);
    env.storage()
        .instance()
        .set(&sub_plan_storage_key, &plan_template_id);

    // Maintain merchant → subscription-ID index
    let merchant_key = DataKey::MerchantSubs(plan.merchant.clone());
    let mut ids: Vec<u32> = env
        .storage()
        .instance()
        .get(&merchant_key)
        .unwrap_or(Vec::new(env));
    ids.push_back(id);
    env.storage().instance().set(&merchant_key, &ids);

    // Maintain token -> subscription-ID index
    let token_key = (Symbol::new(env, "token_subs"), plan.token);
    let mut token_ids: Vec<u32> = env
        .storage()
        .instance()
        .get(&token_key)
        .unwrap_or(Vec::new(env));
    token_ids.push_back(id);
    env.storage().instance().set(&token_key, &token_ids);

    Ok(id)
}

pub fn do_update_plan_template(
    env: &Env,
    merchant: Address,
    plan_template_id: u32,
    amount: i128,
    interval_seconds: u64,
    usage_enabled: bool,
    lifetime_cap: Option<i128>,
) -> Result<u32, Error> {
    merchant.require_auth();

    // Validate lifetime_cap if provided
    if let Some(cap) = lifetime_cap {
        if cap <= 0 {
            return Err(Error::InvalidAmount);
        }
    }

    let existing = get_plan_template(env, plan_template_id)?;
    if existing.merchant != merchant {
        return Err(Error::Forbidden);
    }

    // Do not allow changing token through versioning – that would be a different plan family.
    let token = existing.token.clone();

    let new_plan_id = next_plan_id(env);
    let new_version = existing.version + 1;
    let updated = PlanTemplate {
        merchant: merchant.clone(),
        token,
        amount,
        interval_seconds,
        usage_enabled,
        lifetime_cap,
        template_key: existing.template_key,
        version: new_version,
    };

    let key = (Symbol::new(env, "plan"), new_plan_id);
    env.storage().instance().set(&key, &updated);

    env.events().publish(
        (
            Symbol::new(env, "plan_template_updated"),
            existing.template_key,
        ),
        PlanTemplateUpdatedEvent {
            template_key: existing.template_key,
            old_plan_id: plan_template_id,
            new_plan_id,
            version: new_version,
            merchant,
            timestamp: env.ledger().timestamp(),
        },
    );

    Ok(new_plan_id)
}

pub fn do_migrate_subscription_to_plan(
    env: &Env,
    subscriber: Address,
    subscription_id: u32,
    new_plan_template_id: u32,
) -> Result<(), Error> {
    subscriber.require_auth();

    let mut sub = get_subscription(env, subscription_id)?;
    if sub.subscriber != subscriber {
        return Err(Error::Forbidden);
    }

    // Resolve the current plan the subscription is pinned to (if any).
    let sub_plan_storage_key = sub_plan_key(env, subscription_id);
    let current_plan_id: u32 = match env.storage().instance().get(&sub_plan_storage_key) {
        Some(id) => id,
        None => {
            // Subscription was not created from a plan template – explicit migration required.
            return Err(Error::InvalidInput);
        }
    };

    let current_plan = get_plan_template(env, current_plan_id)?;
    let new_plan = get_plan_template(env, new_plan_template_id)?;

    // Enforce migration within the same logical template family.
    if current_plan.template_key != new_plan.template_key {
        return Err(Error::InvalidInput);
    }

    // Only allow upgrades to newer versions.
    if new_plan.version <= current_plan.version {
        return Err(Error::InvalidInput);
    }

    // For safety, do not allow token switches via migration.
    if new_plan.token != sub.token {
        return Err(Error::InvalidInput);
    }

    // Enforce compatibility of lifetime caps: cannot migrate into a cap that is already exceeded.
    if let Some(cap) = new_plan.lifetime_cap {
        if sub.lifetime_charged > cap {
            return Err(Error::LifetimeCapReached);
        }
        sub.lifetime_cap = Some(cap);
    } else {
        // Removing a cap via migration is allowed; keeps existing lifetime_charged.
        sub.lifetime_cap = None;
    }

    // Apply updated commercial terms from the new plan version.
    sub.amount = new_plan.amount;
    sub.interval_seconds = new_plan.interval_seconds;
    sub.usage_enabled = new_plan.usage_enabled;

    env.storage().instance().set(&subscription_id, &sub);
    env.storage()
        .instance()
        .set(&sub_plan_storage_key, &new_plan_template_id);

    env.events().publish(
        (Symbol::new(env, "subscription_migrated"), subscription_id),
        SubscriptionMigratedEvent {
            subscription_id,
            template_key: new_plan.template_key,
            from_plan_id: current_plan_id,
            to_plan_id: new_plan_template_id,
            merchant: new_plan.merchant,
            subscriber,
            timestamp: env.ledger().timestamp(),
        },
    );

    Ok(())
}
