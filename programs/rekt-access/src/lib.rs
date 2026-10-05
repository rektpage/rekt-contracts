//! rekt.page access — stake REKT for a timed licence, or pay REKT once for a lifetime one.
//!
//! - **7-day / 30-day plans**: the plan's amount of REKT moves into the program vault and stays
//!   locked until the plan expires. The licence lasts exactly as long as the lock; after expiry the
//!   owner takes the tokens back with `unstake`.
//! - **Lifetime**: `lifetime_price` REKT is paid, split 1:1 between the burn vault (rekt-burn) and the treasury, and a
//!   `Lifetime` account records it for good.
//! - The admin can change the three amounts and the treasury. A stake keeps the amount and expiry it
//!   was opened with — changes only apply to new stakes. **No instruction moves tokens out of the
//!   vault except `unstake` back to the staker**, so a leaked admin key cannot take staked tokens.
//! - Buyers pass the amount they agreed to (`expected_amount` / `max_price`); if the admin changed it
//!   in between, the transaction fails instead of charging a different price.
//!
//! The server decides access off-chain from two PDAs per wallet: `["lifetime", wallet]` exists →
//! lifetime; else `["stake", wallet]` with `expires_at` in the future → licensed until then.
//!
//! REKT is a Token-2022 mint, so every token account goes through `token_interface`.
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("REKTKhydBNJMvFZX6iELQ6eiTggPa18WYzwAdjQGCNH");

/// Seconds in a day. Never change it in a deployed build. Test builds only:
///   `--features fast-clock`   1 s  (unit tests: a 7-day plan ends in 7 s)
///   `--features minute-clock` 60 s (local click test: 7 minutes / 30 minutes)
#[cfg(not(any(feature = "fast-clock", feature = "minute-clock")))]
pub const DAY: i64 = 86_400;
#[cfg(feature = "fast-clock")]
pub const DAY: i64 = 1;
#[cfg(all(feature = "minute-clock", not(feature = "fast-clock")))]
pub const DAY: i64 = 60;
pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const STAKE_SEED: &[u8] = b"stake";
pub const LIFETIME_SEED: &[u8] = b"lifetime";

#[program]
pub mod rekt_access {
    use super::*;

    /// One-time setup. Only the program's upgrade authority (the deployer) may call it — otherwise anyone
    /// watching the deploy could initialize first and take the admin role, the mint and the treasury.
    /// `admin` becomes the admin; it does not have to be the deployer.
    pub fn initialize(ctx: Context<Initialize>, admin: Pubkey, stake_week: u64, stake_month: u64, lifetime_price: u64) -> Result<()> {
        require!(stake_week > 0 && stake_month > 0 && lifetime_price > 0, AccessError::ZeroAmount);
        let config = &mut ctx.accounts.config;
        config.admin = admin;
        config.pending_admin = Pubkey::default();
        config.mint = ctx.accounts.mint.key();
        config.treasury = ctx.accounts.treasury.key();
        config.vault = ctx.accounts.vault.key();
        config.stake_week = stake_week;
        config.stake_month = stake_month;
        config.lifetime_price = lifetime_price;
        config.staked_total = 0;
        config.active_stakes = 0;
        config.lifetimes = 0;
        config.bump = ctx.bumps.config;
        config.vault_bump = ctx.bumps.vault;
        emit!(ParamsChanged { stake_week, stake_month, lifetime_price });
        Ok(())
    }

    /// Lock the plan's amount until the plan expires. `plan_days` is 7 or 30.
    /// `expected_amount` must equal the current price of that plan (protects against a change in flight).
    pub fn stake(ctx: Context<Stake>, plan_days: u16, expected_amount: u64) -> Result<()> {
        let config = &mut ctx.accounts.config;
        let amount = match plan_days {
            7 => config.stake_week,
            30 => config.stake_month,
            _ => return err!(AccessError::UnknownPlan),
        };
        require!(amount == expected_amount, AccessError::PriceChanged);

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.user_tokens.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        let now = Clock::get()?.unix_timestamp;
        let stake = &mut ctx.accounts.stake;
        stake.owner = ctx.accounts.user.key();
        stake.amount = amount;
        stake.plan_days = plan_days;
        stake.started_at = now;
        stake.expires_at = now
            .checked_add(i64::from(plan_days).checked_mul(DAY).ok_or(AccessError::Overflow)?)
            .ok_or(AccessError::Overflow)?;
        stake.bump = ctx.bumps.stake;

        config.staked_total = config.staked_total.checked_add(amount).ok_or(AccessError::Overflow)?;
        config.active_stakes = config.active_stakes.checked_add(1).ok_or(AccessError::Overflow)?;
        emit!(Staked { owner: stake.owner, amount, plan_days, expires_at: stake.expires_at });
        Ok(())
    }

    /// After expiry, return the locked tokens to the owner and close the stake account.
    pub fn unstake(ctx: Context<Unstake>) -> Result<()> {
        let stake = &ctx.accounts.stake;
        let now = Clock::get()?.unix_timestamp;
        require!(now >= stake.expires_at, AccessError::StillLocked);
        let amount = stake.amount;

        let config_bump = ctx.accounts.config.bump;
        let signer: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.user_tokens.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                signer,
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        let config = &mut ctx.accounts.config;
        config.staked_total = config.staked_total.checked_sub(amount).ok_or(AccessError::Overflow)?;
        config.active_stakes = config.active_stakes.checked_sub(1).ok_or(AccessError::Overflow)?;
        emit!(Unstaked { owner: ctx.accounts.user.key(), amount });
        Ok(())
    }

    /// Pay `lifetime_price` REKT for a permanent licence, split 1:1 — half into the burn vault (tagged
    /// "lifetime", which only this program can do), the rest (half plus any odd unit) to the treasury.
    /// `max_price` caps what the buyer agreed to pay.
    pub fn buy_lifetime(ctx: Context<BuyLifetime>, max_price: u64) -> Result<()> {
        let price = ctx.accounts.config.lifetime_price;
        require!(price <= max_price, AccessError::PriceChanged);
        let to_burn = price / 2;
        let to_treasury = price - to_burn;

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.user_tokens.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            to_treasury,
            ctx.accounts.mint.decimals,
        )?;

        if to_burn > 0 {
            // the config PDA vouches for the "lifetime" tag
            let config_bump = ctx.accounts.config.bump;
            let signer: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
            rekt_burn::cpi::deposit(
                CpiContext::new_with_signer(
                    ctx.accounts.burn_program.to_account_info(),
                    rekt_burn::cpi::accounts::Deposit {
                        payer: ctx.accounts.user.to_account_info(),
                        tagger: ctx.accounts.config.to_account_info(),
                        state: ctx.accounts.burn_state.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        from: ctx.accounts.user_tokens.to_account_info(),
                        vault: ctx.accounts.burn_vault.to_account_info(),
                        token_program: ctx.accounts.token_program.to_account_info(),
                    },
                    signer,
                ),
                to_burn,
                rekt_burn::SOURCE_LIFETIME,
            )?;
        }

        let lifetime = &mut ctx.accounts.lifetime;
        lifetime.owner = ctx.accounts.user.key();
        lifetime.paid = price;
        lifetime.at = Clock::get()?.unix_timestamp;
        lifetime.bump = ctx.bumps.lifetime;

        let config = &mut ctx.accounts.config;
        config.lifetimes = config.lifetimes.checked_add(1).ok_or(AccessError::Overflow)?;
        emit!(LifetimeBought { owner: lifetime.owner, paid: price });
        Ok(())
    }

    /// Admin: change the amounts. Open stakes keep theirs.
    pub fn set_params(ctx: Context<AdminOnly>, stake_week: u64, stake_month: u64, lifetime_price: u64) -> Result<()> {
        require!(stake_week > 0 && stake_month > 0 && lifetime_price > 0, AccessError::ZeroAmount);
        let config = &mut ctx.accounts.config;
        config.stake_week = stake_week;
        config.stake_month = stake_month;
        config.lifetime_price = lifetime_price;
        emit!(ParamsChanged { stake_week, stake_month, lifetime_price });
        Ok(())
    }

    /// Admin: send future lifetime payments to another REKT token account.
    pub fn set_treasury(ctx: Context<SetTreasury>) -> Result<()> {
        ctx.accounts.config.treasury = ctx.accounts.treasury.key();
        emit!(TreasuryChanged { treasury: ctx.accounts.treasury.key() });
        Ok(())
    }

    /// Admin: start handing the admin role to `new_admin` (two-step, so a typo cannot lose it).
    pub fn propose_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.config.pending_admin = new_admin;
        emit!(AdminProposed { pending_admin: new_admin });
        Ok(())
    }

    /// The proposed admin accepts the role.
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.new_admin.key();
        config.pending_admin = Pubkey::default();
        emit!(AdminChanged { admin: config.admin });
        Ok(())
    }
}

// ---------- accounts ----------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub mint: Pubkey,
    /// token account (REKT) that receives lifetime payments
    pub treasury: Pubkey,
    pub vault: Pubkey,
    pub stake_week: u64,
    pub stake_month: u64,
    pub lifetime_price: u64,
    /// REKT currently locked in the vault
    pub staked_total: u64,
    pub active_stakes: u32,
    pub lifetimes: u32,
    pub bump: u8,
    pub vault_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct StakeAccount {
    pub owner: Pubkey,
    pub amount: u64,
    pub plan_days: u16,
    pub started_at: i64,
    pub expires_at: i64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Lifetime {
    pub owner: Pubkey,
    pub paid: u64,
    pub at: i64,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub deployer: Signer<'info>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ AccessError::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::RektAccess>,
    #[account(constraint = program_data.upgrade_authority_address == Some(deployer.key()) @ AccessError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
    #[account(init, payer = deployer, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = deployer,
        seeds = [VAULT_SEED],
        bump,
        token::mint = mint,
        token::authority = config,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(token::mint = mint, token::token_program = token_program)]
    pub treasury: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = mint, has_one = vault)]
    pub config: Account<'info, Config>,
    /// one stake per wallet at a time; renew = unstake after expiry, then stake again
    #[account(init, payer = user, space = 8 + StakeAccount::INIT_SPACE, seeds = [STAKE_SEED, user.key().as_ref()], bump)]
    pub stake: Account<'info, StakeAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = token_program)]
    pub user_tokens: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = mint, has_one = vault)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        close = user,
        seeds = [STAKE_SEED, user.key().as_ref()],
        bump = stake.bump,
        constraint = stake.owner == user.key() @ AccessError::NotOwner,
    )]
    pub stake: Account<'info, StakeAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = token_program)]
    pub user_tokens: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct BuyLifetime<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = mint, has_one = treasury)]
    pub config: Account<'info, Config>,
    #[account(init, payer = user, space = 8 + Lifetime::INIT_SPACE, seeds = [LIFETIME_SEED, user.key().as_ref()], bump)]
    pub lifetime: Account<'info, Lifetime>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = token_program)]
    pub user_tokens: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub treasury: InterfaceAccount<'info, TokenAccount>,
    pub burn_program: Program<'info, rekt_burn::program::RektBurn>,
    /// checked by the burn program (seeds, mint, vault)
    #[account(mut, seeds = [rekt_burn::STATE_SEED], bump, seeds::program = rekt_burn::ID)]
    pub burn_state: Account<'info, rekt_burn::State>,
    #[account(mut, address = burn_state.vault)]
    pub burn_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ AccessError::NotAdmin)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct SetTreasury<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ AccessError::NotAdmin, has_one = mint)]
    pub config: Account<'info, Config>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(token::mint = mint, token::token_program = token_program)]
    pub treasury: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub new_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.pending_admin == new_admin.key() @ AccessError::NotPendingAdmin,
    )]
    pub config: Account<'info, Config>,
}

// ---------- events & errors ----------

#[event]
pub struct ParamsChanged {
    pub stake_week: u64,
    pub stake_month: u64,
    pub lifetime_price: u64,
}

#[event]
pub struct Staked {
    pub owner: Pubkey,
    pub amount: u64,
    pub plan_days: u16,
    pub expires_at: i64,
}

#[event]
pub struct Unstaked {
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct LifetimeBought {
    pub owner: Pubkey,
    pub paid: u64,
}

#[event]
pub struct TreasuryChanged {
    pub treasury: Pubkey,
}

#[event]
pub struct AdminProposed {
    pub pending_admin: Pubkey,
}

#[event]
pub struct AdminChanged {
    pub admin: Pubkey,
}

#[error_code]
pub enum AccessError {
    #[msg("amounts must be greater than zero")]
    ZeroAmount,
    #[msg("plan must be 7 or 30 days")]
    UnknownPlan,
    #[msg("the price changed; reload and sign again")]
    PriceChanged,
    #[msg("the stake is still locked")]
    StillLocked,
    #[msg("only the stake owner can do this")]
    NotOwner,
    #[msg("only the admin can do this")]
    NotAdmin,
    #[msg("only the proposed admin can accept")]
    NotPendingAdmin,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("only the program upgrade authority can initialize")]
    NotUpgradeAuthority,
}
