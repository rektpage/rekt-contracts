//! rekt.page burn vault — collects $REKT (and SOL for buybacks) and burns $REKT on the admin's order.
//!
//! Where the tokens come from (each has its own running total, so the landing page can show them apart):
//!   - **lifetime** — half of every lifetime purchase, deposited by the staking program itself. Only the
//!     staking program's config PDA can sign for this tag, so the number cannot be inflated by hand.
//!   - **creator fee** — pump.fun's fee sharing sends the vault's share of creator fees, in SOL, straight to the
//!     SOL address `["sol"]`. That SOL can only be spent on buying $REKT back (`buyback_start` /
//!     `buyback_end`). Bought tokens are counted in slot 1. Nobody can tag a deposit as "creator fee" by hand.
//!   - **developer** — the developer buying $REKT back with other revenue (`deposit`, admin only).
//!   Tokens sent straight to the vault account without `deposit` are untagged: balance + burned − tagged.
//!
//! - **burn** (admin) burns tokens held by the vault. Supply really goes down; `burned_total` records it.
//! - There is **no withdraw** of $REKT or SOL. Tokens leave only by burning; SOL only by buying $REKT.
//! - Roles: `admin` (one key, runs burns and developer deposits), `governor` (replaces the admin, hands
//!   governance on, sets the buyback key and limits) and the buyback key (`buyer`, may be the admin).
//!
//! Buyback (added 2026-10-17, program upgrade). One transaction, three steps:
//!   1. `buyback_start(sol, min_out)` lends `sol` lamports from `["sol"]` to the buyer key. It only runs
//!      when a `buyback_end` of this program follows later in the same transaction.
//!   2. the buyer swaps that SOL for $REKT anywhere (Jupiter, PumpSwap, the bonding curve) with the vault
//!      as the destination. The program does not care which venue.
//!   3. `buyback_end` checks the vault got at least `min_out` more $REKT and the buyer kept no SOL, then
//!      records it (slot 1, `sol_spent`, `buybacks`). Anything short of that fails the whole transaction.
//!   The buyer key picks `min_out`, so it is trusted for the price. A per-run cap and a minimum interval,
//!   both set by the governor, bound what a stolen buyer key could waste.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};
use anchor_lang::solana_program::sysvar::instructions::{load_current_index_checked, load_instruction_at_checked};
use anchor_lang::system_program;
use anchor_lang::Discriminator;
use anchor_spl::token_interface::{self, Burn, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("BURNSZMs9SKHsR5mqVDipDDShLXrPeGDXQUBZPJSQ1dg");

pub const STATE_SEED: &[u8] = b"state";
pub const VAULT_SEED: &[u8] = b"vault";
/// System-owned SOL address of the vault. pump.fun fee sharing pays the creator-fee share here.
pub const SOL_SEED: &[u8] = b"sol";
/// Buyback settings and the in-flight record between `buyback_start` and `buyback_end`.
pub const BUYBACK_SEED: &[u8] = b"buyback";
/// shortest gap the governor may set between buybacks (seconds)
pub const MIN_INTERVAL: i64 = 60;

/// The staking program (programs/rekt-access in this repository). Its config PDA is the only signer
/// accepted for `LIFETIME` deposits.
pub const ACCESS_PROGRAM: Pubkey = pubkey!("REKTKhydBNJMvFZX6iELQ6eiTggPa18WYzwAdjQGCNH");

/// slot 0: lifetime purchases (staking program only)
pub const SOURCE_LIFETIME: u8 = 0;
/// slot 1: $REKT bought with creator-fee SOL — written only by `buyback` (phase 2), never by `deposit`
pub const SOURCE_BUYBACK: u8 = 1;
/// slot 2: developer buybacks with other revenue (admin)
pub const SOURCE_DEVELOPER: u8 = 2;
pub const SOURCES: usize = 3;

#[program]
pub mod rekt_burn {
    use super::*;

    /// One-time setup. Only the program's upgrade authority may call it (otherwise anyone watching the
    /// deploy could initialize first and take the roles).
    pub fn initialize(ctx: Context<Initialize>, admin: Pubkey, governor: Pubkey) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.admin = admin;
        state.governor = governor;
        state.mint = ctx.accounts.mint.key();
        state.vault = ctx.accounts.vault.key();
        state.burned_total = 0;
        state.deposited = [0; SOURCES];
        state.burns = 0;
        state.sol_spent = 0;
        state.buybacks = 0;
        state.bump = ctx.bumps.state;
        state.vault_bump = ctx.bumps.vault;
        state.sol_bump = Pubkey::find_program_address(&[SOL_SEED], ctx.program_id).1;
        state.reserved = [0; 64];
        emit!(RolesChanged { admin, governor });
        Ok(())
    }

    /// Put $REKT into the vault and record where it came from.
    pub fn deposit(ctx: Context<Deposit>, amount: u64, source: u8) -> Result<()> {
        require!(amount > 0, BurnError::ZeroAmount);
        let signer = ctx.accounts.tagger.key();
        match source {
            SOURCE_LIFETIME => {
                let (access_config, _) = Pubkey::find_program_address(&[b"config"], &ACCESS_PROGRAM);
                require_keys_eq!(signer, access_config, BurnError::NotAllowedSource);
            }
            SOURCE_DEVELOPER => {
                require_keys_eq!(signer, ctx.accounts.state.admin, BurnError::NotAllowedSource);
            }
            // slot 1 belongs to buyback: creator-fee $REKT is proven by the swap, not declared
            _ => return err!(BurnError::UnknownSource),
        }

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.from.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        let state = &mut ctx.accounts.state;
        let slot = &mut state.deposited[source as usize];
        *slot = slot.checked_add(amount).ok_or(BurnError::Overflow)?;
        emit!(Deposited { source, amount, from: ctx.accounts.payer.key() });
        Ok(())
    }

    /// Burn `amount` of the $REKT the vault holds.
    pub fn burn(ctx: Context<BurnTokens>, amount: u64) -> Result<()> {
        require!(amount > 0, BurnError::ZeroAmount);
        let bump = ctx.accounts.state.bump;
        let signer: &[&[&[u8]]] = &[&[STATE_SEED, &[bump]]];
        token_interface::burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.state.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;
        let state = &mut ctx.accounts.state;
        state.burned_total = state.burned_total.checked_add(amount).ok_or(BurnError::Overflow)?;
        state.burns = state.burns.checked_add(1).ok_or(BurnError::Overflow)?;
        emit!(Burned { amount, burned_total: state.burned_total });
        Ok(())
    }

    /// Governor: replace the admin (the key that deposits and burns).
    pub fn set_admin(ctx: Context<GovernorOnly>, admin: Pubkey) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.admin = admin;
        emit!(RolesChanged { admin, governor: state.governor });
        Ok(())
    }

    /// Governor: hand governance to another account.
    pub fn set_governor(ctx: Context<GovernorOnly>, governor: Pubkey) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.governor = governor;
        emit!(RolesChanged { admin: state.admin, governor });
        Ok(())
    }

    /// Governor: set the buyback key, the most SOL one buyback may spend, and the minimum seconds between
    /// buybacks. Creates the settings account on first use. `buyer = default` switches buybacks off.
    pub fn set_buyback(ctx: Context<SetBuyback>, buyer: Pubkey, max_sol: u64, min_interval: i64) -> Result<()> {
        // at least a minute: with no gap, several start/end pairs in one transaction would each pass the cap
        require!(min_interval >= MIN_INTERVAL, BurnError::BadSetting);
        let b = &mut ctx.accounts.buyback;
        b.buyer = buyer;
        b.max_sol = max_sol;
        b.min_interval = min_interval;
        b.bump = ctx.bumps.buyback;
        emit!(BuybackSet { buyer, max_sol, min_interval });
        Ok(())
    }

    /// Step 1: lend `sol` lamports from the SOL address to the buyer for a swap in this same transaction.
    pub fn buyback_start(ctx: Context<BuybackStart>, sol: u64, min_out: u64) -> Result<()> {
        let b = &ctx.accounts.buyback;
        require!(b.buyer != Pubkey::default(), BurnError::BuybackOff);
        require!(!b.in_progress, BurnError::BuybackInProgress);
        require!(sol > 0 && min_out > 0, BurnError::ZeroAmount);
        require!(sol <= b.max_sol, BurnError::OverCap);
        let now = Clock::get()?.unix_timestamp;
        require!(now >= b.last_at.saturating_add(b.min_interval), BurnError::TooSoon);
        // the SOL address is a plain system account: keep it rent-exempt so fee shares keep landing
        let keep = Rent::get()?.minimum_balance(0);
        require!(ctx.accounts.sol.lamports().saturating_sub(sol) >= keep, BurnError::NotEnoughSol);

        // Called directly by the transaction (no wrapper program), and the next instruction of this
        // program in the transaction must be buyback_end. A failed end reverts the loan with everything else.
        require!(get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT, BurnError::NoBuybackEnd);
        let ixs = ctx.accounts.instructions.to_account_info();
        let mut i = load_current_index_checked(&ixs)? as usize + 1;
        loop {
            let ix = load_instruction_at_checked(i, &ixs).map_err(|_| error!(BurnError::NoBuybackEnd))?;
            if ix.program_id == crate::ID {
                require!(ix.data.starts_with(crate::instruction::BuybackEnd::DISCRIMINATOR), BurnError::NoBuybackEnd);
                break;
            }
            i += 1;
        }

        let b = &mut ctx.accounts.buyback;
        b.in_progress = true;
        b.sol = sol;
        b.min_out = min_out;
        b.vault_before = ctx.accounts.vault.amount;
        b.buyer_before = ctx.accounts.buyer.lamports();

        let bump = ctx.accounts.state.sol_bump;
        let signer: &[&[&[u8]]] = &[&[SOL_SEED, &[bump]]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer { from: ctx.accounts.sol.to_account_info(), to: ctx.accounts.buyer.to_account_info() },
                signer,
            ),
            sol,
        )
    }

    /// Step 3: the vault must hold at least `min_out` more $REKT and the buyer no more SOL than before.
    pub fn buyback_end(ctx: Context<BuybackEnd>) -> Result<()> {
        // top level only, so a program the swap passes through cannot close the buyback early
        require!(get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT, BurnError::NoBuybackStart);
        let b = &ctx.accounts.buyback;
        require!(b.in_progress, BurnError::NoBuybackStart);
        let received = ctx.accounts.vault.amount.checked_sub(b.vault_before).ok_or(BurnError::ShortBuyback)?;
        require!(received >= b.min_out, BurnError::ShortBuyback);
        require!(ctx.accounts.buyer.lamports() <= b.buyer_before, BurnError::SolKept);

        let sol = b.sol;
        let state = &mut ctx.accounts.state;
        let slot = &mut state.deposited[SOURCE_BUYBACK as usize];
        *slot = slot.checked_add(received).ok_or(BurnError::Overflow)?;
        state.sol_spent = state.sol_spent.checked_add(sol).ok_or(BurnError::Overflow)?;
        state.buybacks = state.buybacks.checked_add(1).ok_or(BurnError::Overflow)?;

        let b = &mut ctx.accounts.buyback;
        b.in_progress = false;
        b.last_at = Clock::get()?.unix_timestamp;
        b.sol = 0;
        b.min_out = 0;
        emit!(BoughtBack { sol, amount: received, buyer: ctx.accounts.buyer.key() });
        Ok(())
    }
}

// ---------- accounts ----------

#[account]
#[derive(InitSpace)]
pub struct State {
    pub admin: Pubkey,
    pub governor: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    /// all $REKT burned through this vault
    pub burned_total: u64,
    /// $REKT in by source: [lifetime, bought with creator-fee SOL, developer]
    pub deposited: [u64; SOURCES],
    /// number of burn orders
    pub burns: u32,
    /// SOL (lamports) spent on buybacks. SOL received = SOL address balance + this
    pub sol_spent: u64,
    pub buybacks: u32,
    pub bump: u8,
    pub vault_bump: u8,
    pub sol_bump: u8,
    /// room for phase 2 without moving the fields above
    pub reserved: [u8; 64],
}

#[account]
#[derive(InitSpace)]
pub struct Buyback {
    /// key allowed to run buybacks; default = off
    pub buyer: Pubkey,
    /// most lamports one buyback may spend
    pub max_sol: u64,
    /// minimum seconds between buybacks
    pub min_interval: i64,
    /// when the last buyback finished
    pub last_at: i64,
    /// between buyback_start and buyback_end of one transaction
    pub in_progress: bool,
    pub sol: u64,
    pub min_out: u64,
    pub vault_before: u64,
    pub buyer_before: u64,
    pub bump: u8,
    pub reserved: [u8; 32],
}

#[derive(Accounts)]
pub struct SetBuyback<'info> {
    #[account(mut)]
    pub governor: Signer<'info>,
    #[account(seeds = [STATE_SEED], bump = state.bump, has_one = governor @ BurnError::NotGovernor)]
    pub state: Account<'info, State>,
    #[account(init_if_needed, payer = governor, space = 8 + Buyback::INIT_SPACE, seeds = [BUYBACK_SEED], bump)]
    pub buyback: Account<'info, Buyback>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BuybackStart<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(seeds = [STATE_SEED], bump = state.bump, has_one = vault)]
    pub state: Account<'info, State>,
    #[account(mut, seeds = [BUYBACK_SEED], bump = buyback.bump, has_one = buyer @ BurnError::NotBuyer)]
    pub buyback: Account<'info, Buyback>,
    #[account(mut, seeds = [SOL_SEED], bump = state.sol_bump)]
    pub sol: SystemAccount<'info>,
    pub vault: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: the instructions sysvar, checked by address
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BuybackEnd<'info> {
    pub buyer: Signer<'info>,
    #[account(mut, seeds = [STATE_SEED], bump = state.bump, has_one = vault)]
    pub state: Account<'info, State>,
    #[account(mut, seeds = [BUYBACK_SEED], bump = buyback.bump, has_one = buyer @ BurnError::NotBuyer)]
    pub buyback: Account<'info, Buyback>,
    pub vault: InterfaceAccount<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub deployer: Signer<'info>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ BurnError::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::RektBurn>,
    #[account(constraint = program_data.upgrade_authority_address == Some(deployer.key()) @ BurnError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
    #[account(init, payer = deployer, space = 8 + State::INIT_SPACE, seeds = [STATE_SEED], bump)]
    pub state: Account<'info, State>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = deployer,
        seeds = [VAULT_SEED],
        bump,
        token::mint = mint,
        token::authority = state,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    /// owner of `from`; pays the tokens
    pub payer: Signer<'info>,
    /// who vouches for the source tag: the staking program's config PDA (lifetime) or the admin
    pub tagger: Signer<'info>,
    #[account(mut, seeds = [STATE_SEED], bump = state.bump, has_one = mint, has_one = vault)]
    pub state: Account<'info, State>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = payer, token::token_program = token_program)]
    pub from: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct BurnTokens<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [STATE_SEED], bump = state.bump, has_one = admin @ BurnError::NotAdmin, has_one = mint, has_one = vault)]
    pub state: Account<'info, State>,
    #[account(mut)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct GovernorOnly<'info> {
    pub governor: Signer<'info>,
    #[account(mut, seeds = [STATE_SEED], bump = state.bump, has_one = governor @ BurnError::NotGovernor)]
    pub state: Account<'info, State>,
}

// ---------- events & errors ----------

#[event]
pub struct Deposited {
    pub source: u8,
    pub amount: u64,
    pub from: Pubkey,
}

#[event]
pub struct Burned {
    pub amount: u64,
    pub burned_total: u64,
}

#[event]
pub struct RolesChanged {
    pub admin: Pubkey,
    pub governor: Pubkey,
}

#[event]
pub struct BuybackSet {
    pub buyer: Pubkey,
    pub max_sol: u64,
    pub min_interval: i64,
}

#[event]
pub struct BoughtBack {
    /// lamports spent
    pub sol: u64,
    /// $REKT that reached the vault
    pub amount: u64,
    pub buyer: Pubkey,
}

#[error_code]
pub enum BurnError {
    #[msg("amount must be greater than zero")]
    ZeroAmount,
    #[msg("unknown deposit source")]
    UnknownSource,
    #[msg("this signer may not use that source tag")]
    NotAllowedSource,
    #[msg("only the admin can do this")]
    NotAdmin,
    #[msg("only the governor can do this")]
    NotGovernor,
    #[msg("only the program upgrade authority can initialize")]
    NotUpgradeAuthority,
    #[msg("arithmetic overflow")]
    Overflow,
    // appended so the error codes above keep their numbers
    #[msg("bad buyback setting")]
    BadSetting,
    #[msg("buybacks are switched off")]
    BuybackOff,
    #[msg("only the buyback key can do this")]
    NotBuyer,
    #[msg("a buyback is already in progress")]
    BuybackInProgress,
    #[msg("over the per-buyback SOL cap")]
    OverCap,
    #[msg("too soon after the last buyback")]
    TooSoon,
    #[msg("not enough SOL in the SOL address")]
    NotEnoughSol,
    #[msg("buyback_start must be followed by buyback_end in the same transaction")]
    NoBuybackEnd,
    #[msg("no buyback in progress")]
    NoBuybackStart,
    #[msg("the vault received less $REKT than min_out")]
    ShortBuyback,
    #[msg("the buyer kept SOL from the buyback")]
    SolKept,
}
