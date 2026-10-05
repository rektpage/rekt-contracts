//! rekt.page burn vault — collects $REKT (and SOL for buybacks) and burns $REKT on the admin's order.
//!
//! Where the tokens come from (each has its own running total, so the landing page can show them apart):
//!   - **lifetime** — half of every lifetime purchase, deposited by the staking program itself. Only the
//!     staking program's config PDA can sign for this tag, so the number cannot be inflated by hand.
//!   - **creator fee** — pump.fun's fee sharing sends the vault's share of creator fees, in SOL, straight to the
//!     SOL address `["sol"]`. That SOL can only be spent on buying $REKT back (`buyback`, phase 2). Bought
//!     tokens are counted in slot 1. Nobody can tag a deposit as "creator fee" by hand.
//!   - **developer** — the developer buying $REKT back with other revenue (`deposit`, admin only).
//!   Tokens sent straight to the vault account without `deposit` are untagged: balance + burned − tagged.
//!
//! - **burn** (admin) burns tokens held by the vault. Supply really goes down; `burned_total` records it.
//! - There is **no withdraw** of $REKT or SOL. Tokens leave only by burning; SOL only by buying $REKT.
//! - Roles: `admin` (one key, runs burns, developer deposits and buybacks) and `governor` (meant to be a
//!   Squads 2-of-3 multisig) which alone can replace the admin or hand governance on.
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Burn, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("BURNSZMs9SKHsR5mqVDipDDShLXrPeGDXQUBZPJSQ1dg");

pub const STATE_SEED: &[u8] = b"state";
pub const VAULT_SEED: &[u8] = b"vault";
/// System-owned SOL address of the vault. pump.fun fee sharing pays the creator-fee share here.
pub const SOL_SEED: &[u8] = b"sol";

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

    /// Governor: hand governance to another account (e.g. the Squads multisig vault).
    pub fn set_governor(ctx: Context<GovernorOnly>, governor: Pubkey) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.governor = governor;
        emit!(RolesChanged { admin: state.admin, governor });
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
}
