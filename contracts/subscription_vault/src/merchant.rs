//! Merchant payout and accumulated USDC tracking entrypoints.
//! (Full manual re-sync)

use crate::types::MerchantConfig;
use crate::safe_math::validate_non_negative;
use crate::types::{DataKey, Error, MerchantPausedEvent, MerchantUnpausedEvent};
use soroban_sdk::{token, Address, Env, Symbol};

fn merchant_balance_key(
    env: &Env,
    merchant: &Address,
    token: &Address,
) -> (Symbol, Address, Address) {
    (
        Symbol::new(env, "merchant_balance"),
        merchant.clone(),
        token.clone(),
    )
}

pub fn get_merchant_balance(env: &Env, merchant: &Address) -> i128 {
    if let Ok(token_addr) = crate::admin::get_token(env) {
        return get_merchant_balance_by_token(env, merchant, &token_addr);
    }
    0
}

pub fn get_merchant_balance_by_token(env: &Env, merchant: &Address, token: &Address) -> i128 {
    let key = merchant_balance_key(env, merchant, token);
    env.storage().instance().get(&key).unwrap_or(0i128)
}

fn set_merchant_balance(env: &Env, merchant: &Address, token: &Address, balance: &i128) {
    let key = merchant_balance_key(env, merchant, token);
    env.storage().instance().set(&key, balance);
}

pub fn credit_merchant_balance_for_token(
    env: &Env,
    merchant: &Address,
    token_addr: &Address,
    amount: i128,
) -> Result<(), Error> {
    validate_non_negative(amount)?;
    let current = get_merchant_balance_by_token(env, merchant, token_addr);
    let new_balance = current.checked_add(amount).ok_or(Error::Overflow)?;
    set_merchant_balance(env, merchant, token_addr, &new_balance);
    Ok(())
}

pub fn withdraw_merchant_funds(env: &Env, merchant: Address, amount: i128) -> Result<(), Error> {
    let token_addr = crate::admin::get_token(env)?;
    withdraw_merchant_funds_for_token(env, merchant, token_addr, amount)
}

pub fn withdraw_merchant_funds_for_token(
    env: &Env,
    merchant: Address,
    token_addr: Address,
    amount: i128,
) -> Result<(), Error> {
    merchant.require_auth();
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    let current = get_merchant_balance_by_token(env, &merchant, &token_addr);
    if current == 0 {
        return Err(Error::NotFound);
    }
    if amount > current {
        return Err(Error::InsufficientBalance);
    }

    let new_balance = current.checked_sub(amount).ok_or(Error::Overflow)?;
    set_merchant_balance(env, &merchant, &token_addr, &new_balance);
    env.events()
        .publish((Symbol::new(env, "withdrawn"), merchant.clone()), amount);

    let token_client = token::Client::new(env, &token_addr);
    token_client.transfer(&env.current_contract_address(), &merchant, &amount);

    Ok(())
}

fn merchant_config_key(env: &Env, merchant: &Address) -> (Symbol, Address) {
    (Symbol::new(env, "merch_conf"), merchant.clone())
}

pub fn set_merchant_config(
    env: &Env,
    merchant: Address,
    config: MerchantConfig,
) -> Result<(), Error> {
    merchant.require_auth();
    
    // Validation: URL shouldn't be excessively long (standard limit 256)
    if config.redirect_url.len() > 256 {
        return Err(Error::InvalidAmount); // Reusing error or add specific one
    }

    let key = merchant_config_key(env, &merchant);
    env.storage().instance().set(&key, &config);
    Ok(())
}

pub fn get_merchant_config(env: &Env, merchant: Address) -> Option<MerchantConfig> {
    let key = merchant_config_key(env, &merchant);
    env.storage().instance().get(&key)
}
