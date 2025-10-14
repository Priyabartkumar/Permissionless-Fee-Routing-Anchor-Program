#![allow(unexpected_cfgs)]
#![allow(deprecated)]
use anchor_lang::prelude::*;
use anchor_spl::token::{self,Mint,TokenAccount, Token, Transfer};
use anchor_lang::solana_program;

declare_id!("HqFWrGqRnTetKVnaZhgXUX8CzHjG6j2bu31SMvDa3L42");

pub const VAULT_SEED: &[u8] = b"vault";
pub const INVESTOR_FEE_POS_OWNER_SEED: &[u8] = b"investor_fee_pos_owner";
pub const POLICY_PDA_SEED: &[u8] = b"policy";
pub const PROGRESS_PDA_SEED: &[u8] = b"progress";
pub const TREASURY_SEED: &[u8] = b"quote_treasury";

#[program]
pub mod honorary_fee {
    use super::*;

    /// Initialize the PAD-owned honorary position and policy.
    /// This will create Policy and Progress PDAs and set initial configuration.
    /// NOTE: actual creation of the AMM "position" account (pool side) is expected to be done externally;
    /// this instruction only records/validates configuration and creates the program-side owner PDAs/treasury ATA.
    pub fn initialize(
        ctx: Context<Initialize>,
        // Config
        investor_fee_share_bps: u16,
        daily_cap_lamports_opt: Option<u64>,
        min_payout_lamports: u64,
        y0_total_alloc: u64, // Y0 (total investor allocation minted at TGE)
    ) -> Result<()> {
        let policy = &mut ctx.accounts.policy;
        policy.bump = ctx.bumps.policy;
        policy.investor_fee_share_bps = investor_fee_share_bps;
        policy.daily_cap_lamports = daily_cap_lamports_opt;
        policy.min_payout_lamports = min_payout_lamports;
        policy.y0_total_alloc = y0_total_alloc;
        policy.quote_mint = ctx.accounts.quote_mint.key();
        policy.creator_quote_ata = ctx.accounts.creator_quote_ata.key();
        policy.streamflow_program = ctx.accounts.streamflow_program.key();

        // Progress initial state
        let progress = &mut ctx.accounts.progress;
        progress.bump = ctx.bumps.progress;
        progress.last_distribution_ts = 0i64;
        progress.current_day_cumulative = 0;
        progress.carry_lamports = 0;
        progress.page_cursor = 0;
        progress.in_day = false;

        // Treasury is created as an associated token account by the client and passed in; we check ownership/mint.
        require_keys_eq!(ctx.accounts.treasury.mint, ctx.accounts.quote_mint.key(), HonoraryError::TreasuryMintMismatch);

        // validate quote/token ordering may be done here by reading the pool account's token order
        // For safety: store the quote mint and later enforce that claimed assets are in that mint only.

        emit!(HonoraryPositionInitialized {
            policy: policy.key(),
            progress: progress.key(),
            treasury: ctx.accounts.treasury.key(),
            quote_mint: ctx.accounts.quote_mint.key(),
            creator_quote_ata: ctx.accounts.creator_quote_ata.key(),
        });

        Ok(())
    }

    /// Permissionless, once-per-24h crank that:
    /// - optionally invokes a cp-amm claim (if claim_ix_data provided and remaining_accounts correspond to cp-amm)
    /// - reads program-owned treasury quote balance
    /// - reads Streamflow still-locked amounts (passed in as per-investor accounts)
    /// - computes investor share and distributes pro-rata across provided page (paged)
    /// - supports idempotent resumable pagination using Progress PDA
    #[allow(clippy::too_many_arguments)]
    pub fn crank_distribute(
        ctx: Context<CrankDistribute>,
        // runtime options
        now_ts: i64,
        // optionally provide raw instruction bytes to invoke cp-amm claim (optional)
        claim_ix_data: Option<Vec<u8>>,
        // per-page: index of page (cursor). If None, will use progress.page_cursor
        page_index_opt: Option<u64>,
        // vector of still-locked amounts for each investor in this page (aligned with investor_streams and investor_at as passed)
        locked_amounts: Vec<u64>,
        // page_end boolean: caller indicates this is final page for the day
        page_is_final: bool,
    ) -> Result<()> {
        let policy = &ctx.accounts.policy;
        let progress = &mut ctx.accounts.progress;
        let treasury = &ctx.accounts.treasury;
        let quote_mint = &ctx.accounts.quote_mint;

        // 1) If claim_ix_data provided, attempt to invoke the cp-amm claim. The program will NOT itself
        // construct the cp-amm instruction — the caller passes the encoded instruction and cp-amm accounts
        if let Some(ix_data) = claim_ix_data {
            // We require at least one remaining_account: the cp-amm program and its accounts.
            // Anchor provides them as remaining_accounts
            use solana_program::{instruction::{Instruction,AccountMeta}, program::invoke_signed,pubkey::Pubkey as SolPubkey};
           
            require!(!ctx.remaining_accounts.is_empty(), HonoraryError::MissingCpAmmAccounts);
            // Build Instruction
            let cp_amm_program_id = SolPubkey::new_from_array(ctx.remaining_accounts[0].key().to_bytes());

           
            // Create vector of AccountMeta
            let metas: Vec<AccountMeta> = ctx.remaining_accounts.iter().map(|acc| AccountMeta{
                    pubkey: SolPubkey::new_from_array(acc.key.to_bytes()),
                    is_signer: acc.is_signer,
                    is_writable: acc.is_writable,
                
            }).collect();

            let ix = Instruction {
                program_id: cp_amm_program_id,
                accounts: metas,
                data: ix_data,
            };
          

            // signer seeds for PDAs if cp-amm expects program-owned authority. Use the investor_fee_pos_owner PDA seeds.
            let signer_seeds: &[&[&[u8]]] = &[
                &[
                    VAULT_SEED,
                    ctx.accounts.vault.key.as_ref(),
                    INVESTOR_FEE_POS_OWNER_SEED,
                    &[ctx.accounts.investor_fee_pos_owner.bump],
                ]
            ];

           

           invoke_signed(&ix, &ctx.remaining_accounts, signer_seeds)?;
        }

        // 2) Enforce that the treasury is the right mint and owned by program (checked at init but re-check)
        require_keys_eq!(policy.quote_mint, quote_mint.key(), HonoraryError::QuoteMintMismatch);
        require!(treasury.mint == quote_mint.key(), HonoraryError::TreasuryMintMismatch);

        // 3) Read treasury balance (claimed_quote)
        let claimed_quote = treasury.amount;
        if claimed_quote == 0 {
            // nothing to do — no claimed quote tokens
            emit!(QuoteFeesClaimed {
                claimed_amount: 0,
                treasury: treasury.key(),
            });
            return Ok(());
        }

        // 4) Ensure there's no base-token fees present. The instruction expects a base_treasury token account supplied if needed.
        if ctx.accounts.base_treasury.is_some() {
            let base_treasury = ctx.accounts.base_treasury.as_ref().unwrap();
            if base_treasury.amount > 0 {
                return err!(HonoraryError::BaseFeesDetected);
            }
        }

        // 5) 24h gate & day logic
        let day_window = 86_400i64;
        let is_new_day = if progress.last_distribution_ts == 0 {
            true
        } else {
            now_ts >= progress.last_distribution_ts.checked_add(day_window).unwrap_or(i64::MAX)
        };

        if is_new_day {
            // start a new day: reset per-day accumulators
            progress.last_distribution_ts = now_ts;
            progress.current_day_cumulative = 0;
            progress.carry_lamports = progress.carry_lamports; // carry persists across days
            progress.page_cursor = 0;
            progress.in_day = true;
        } else {
            // Not a new day: ensure the caller is continuing the same active day (allowed)
            require!(progress.in_day, HonoraryError::NotInActiveDay);
        }

        // 6) Compute f_locked(t) from locked_amounts and provided Y0.
        let locked_total: u128 = locked_amounts.iter().map(|&v| v as u128).sum();
        // If locked_total == 0 then all unlocked -> send everything to creator on day close (but per-page we'll do nothing)
        let y0 = policy.y0_total_alloc as u128;
        let f_locked_bps: u64 = if y0 == 0 {
            0
        } else {
            // compute floor(f_locked * 10000) = floor(locked_total / Y0 * 10000)
            ((locked_total * 10000u128) / y0) as u64
        };

        let eligible_investor_share_bps = std::cmp::min(policy.investor_fee_share_bps as u64, f_locked_bps);

        // compute investor_fee_quote = floor(claimed_quote * eligible_bps / 10000)
        let mut investor_fee_quote: u128 = (claimed_quote as u128) * (eligible_investor_share_bps as u128) / 10000u128;

        // incorporate carry from earlier (carry_lamports)
        let mut available_for_distribute: u128 = (progress.carry_lamports as u128) + investor_fee_quote;

        // apply daily cap if any
        if let Some(cap) = policy.daily_cap_lamports {
            let cap_u128 = cap as u128;
            if progress.current_day_cumulative as u128 >= cap_u128 {
                // cap already reached earlier in the day: nothing to distribute to investors now
                investor_fee_quote = 0;
                available_for_distribute = 0;
            } else {
                let remaining_cap = cap_u128 - (progress.current_day_cumulative as u128);
                if available_for_distribute > remaining_cap {
                    // clamp investor portion to remaining cap, move excess back to carry
                    investor_fee_quote = remaining_cap;
                    // the remainder goes to carry that will be transferred to creator or next day
                    // (we keep the remainder in treasury until day close)
                    available_for_distribute = remaining_cap;
                }
            }
        }

        // 7) If locked_total == 0 -> no investor payouts on pages; page should be noop, and final page will route everything to creator.
        if locked_total == 0 || eligible_investor_share_bps == 0 || available_for_distribute == 0 {
            // Nothing to distribute to investors this page. Advance pagination cursor if page provided.
            if let Some(page_index) = page_index_opt {
                // idempotency: only advance cursor if this is a new page
                if page_index == progress.page_cursor {
                    progress.page_cursor = progress.page_cursor.checked_add(1).ok_or(HonoraryError::Overflow)?;
                }
            }
            emit!(InvestorPayoutPage {
                page_index: progress.page_cursor,
                distributed: 0,
            });

            // If this is final page, route remainder (claimed_quote + carry) to creator
            if page_is_final {
                // compute full remainder: treasury.amount (claimed_quote) + carry
                // Transfer entire treasury amount to creator_quote_ata
                let treasury_amount_now = ctx.accounts.treasury.amount;
                if treasury_amount_now > 0 {
                    let seeds = &[VAULT_SEED, ctx.accounts.vault.key.as_ref(), INVESTOR_FEE_POS_OWNER_SEED, &[ctx.accounts.investor_fee_pos_owner.bump]];
                    let signer = &[&seeds[..]];
                    // Transfer via token::transfer with signer
                    let cpi_accounts = Transfer {
                        from: ctx.accounts.treasury.to_account_info(),
                        to: ctx.accounts.creator_quote_ata.to_account_info(),
                        authority: ctx.accounts.investor_fee_pos_owner.to_account_info(),
                    };
                    let cpi_program = ctx.accounts.token_program.to_account_info();
                    let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
                    token::transfer(cpi_ctx, treasury_amount_now)?;
                    progress.current_day_cumulative = progress.current_day_cumulative.checked_add(treasury_amount_now).ok_or(HonoraryError::Overflow)?;
                }
                // zero carry
                progress.carry_lamports = 0;
                progress.in_day = false;

                emit!(CreatorPayoutDayClose {
                    day_ts: progress.last_distribution_ts,
                    remainder_to_creator: treasury_amount_now,
                });
            }

            return Ok(());
        }

        // 8) Distribute pro-rata for this page.
        // We'll compute per-investor weight: locked_i / locked_total and pay floor(available_for_distribute * weight)
        // Use integer math with floors as required.
        let locked_total_u128: u128 = locked_total;
        let mut page_total_distributed: u128 = 0;
        let mut payouts: Vec<u64> = Vec::with_capacity(locked_amounts.len());
        for &locked_i in locked_amounts.iter() {
            let pay_i: u128 = (available_for_distribute * (locked_i as u128)) / locked_total_u128;
            payouts.push(pay_i as u64);
            page_total_distributed = page_total_distributed.checked_add(pay_i).ok_or(HonoraryError::Overflow)?;
        }

        // apply min_payout_lamports: if pay_i < min_payout then treat as 0 and it becomes dust
        let mut final_payout_sum: u128 = 0;
        let mut final_payouts: Vec<u64> = Vec::with_capacity(payouts.len());
        for pay in payouts.iter() {
            if *pay as u64 >= policy.min_payout_lamports {
                final_payout_sum = final_payout_sum.checked_add(*pay as u128).ok_or(HonoraryError::Overflow)?;
                final_payouts.push(*pay);
            } else {
                // treat as dust: don't pay
                final_payouts.push(0u64);
            }
        }

        // 9) Transfer per-investor payouts from treasury to each investor ATA (the list of investor_atas must align)
        // Idempotency: To avoid double-pay on retries we check progress.page_cursor.
        let page_index = page_index_opt.unwrap_or(progress.page_cursor);

        // If the page_index doesn't match progress.page_cursor, do not re-pay (idempotency) unless we're resuming current page.
        if page_index != progress.page_cursor {
            // This might be a retry of an earlier page that already completed; just return OK (no-op).
            return Ok(());
        }

        // do transfers
        let seeds = &[VAULT_SEED, ctx.accounts.vault.key.as_ref(), INVESTOR_FEE_POS_OWNER_SEED, &[ctx.accounts.investor_fee_pos_owner.bump]];
        let signer = &[&seeds[..]];
       
        for (i, investor_ata_info) in remaining_accounts.iter().enumerate() {
            // The tests/integration MUST pass investor token ATAs as remaining_accounts in same order as locked_amounts.
            if i >= final_payouts.len() {
                break;
            }
            let payout_amt = final_payouts[i] as u64;
            if payout_amt == 0 {
                continue;
            }
            // Build Transfer CPI using explicit accounts
            // remaining_accounts[i] should be investor_ata
            // But Anchor doesn't allow us to convert arbitrary AccountInfo to TokenAccount directly; we'll perform a CPI with AccountInfos
            let from = ctx.accounts.treasury.to_account_info();
            let to = investor_ata_info.clone();
            let authority = ctx.accounts.investor_fee_pos_owner.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), Transfer {
                from,
                to,
                authority,
            }, signer);
            token::transfer(cpi_ctx, payout_amt)?;
        }

        // 10) Update progress: cumulative & cursor & carry
        progress.current_day_cumulative = progress.current_day_cumulative.checked_add(final_payout_sum as u64).ok_or(HonoraryError::Overflow)?;
        // compute dust: available_for_distribute - final_payout_sum -> store in carry
        let dust = (available_for_distribute).checked_sub(final_payout_sum).unwrap_or(0u128);
        progress.carry_lamports = dust as u64;
        // advance cursor
        progress.page_cursor = progress.page_cursor.checked_add(1).ok_or(HonoraryError::Overflow)?;

        emit!(InvestorPayoutPage {
            page_index,
            distributed: final_payout_sum as u64
        });

        // 11) If final page, route remainder to creator and close day
        if page_is_final {
            // Transfer remaining treasury amount to creator (including carry)
            let remainder = ctx.accounts.treasury.amount;
            if remainder > 0 {
                let cpi_accounts = Transfer {
                    from: ctx.accounts.treasury.to_account_info(),
                    to: ctx.accounts.creator_quote_ata.to_account_info(),
                    authority: ctx.accounts.investor_fee_pos_owner.to_account_info(),
                };
                let cpi_program = ctx.accounts.token_program.to_account_info();
                let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
                token::transfer(cpi_ctx, remainder)?;
                progress.current_day_cumulative = progress.current_day_cumulative.checked_add(remainder).ok_or(HonoraryError::Overflow)?;
            }
            progress.carry_lamports = 0;
            progress.in_day = false;

            emit!(CreatorPayoutDayClose {
                day_ts: progress.last_distribution_ts,
                remainder_to_creator: remainder,
            });
        }

        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(investor_fee_share_bps: u16, daily_cap_lamports_opt: Option<u64>, min_payout_lamports: u64, y0_total_alloc: u64)]
pub struct Initialize<'info> {
    /// signer: payer
    #[account(mut)]
    pub payer: Signer<'info>,

    /// The vault account public key used as an input (arbitrary seed anchor uses)
    /// We use this in seeds to make the owner PDA deterministic per-vault.
    pub vault: AccountInfo<'info>,

    /// Investor Fee Position Owner PDA (program-owned)
    /// seeds: [VAULT_SEED, vault, INVESTOR_FEE_POS_OWNER_SEED]
    #[account(
        init,
        seeds = [VAULT_SEED, vault.key().as_ref(), INVESTOR_FEE_POS_OWNER_SEED],
        bump,
        payer = payer,
        space = 8 + 1 // minimal placeholder
    )]
    pub investor_fee_pos_owner: Account<'info, InvestorFeePosOwner>,

    /// Quote mint
    pub quote_mint: Account<'info, Mint>,

    /// program's quote treasury ATA (must already exist and be owned by this program's PDA or be an ATA owned by PDA)
    /// The treasury is expected to be an SPL token account with mint = quote_mint
    #[account(mut)]
    pub treasury: Account<'info, TokenAccount>,

    /// Creator's quote ATA (destination for final remainder)
    /// Must be supplied by integrator.
    #[account(mut)]
    pub creator_quote_ata: Account<'info, TokenAccount>,

    /// Streamflow program id for usage in integration (not invoked by this program directly)
    pub streamflow_program: UncheckedAccount<'info>,

    /// Policy PDA that stores fee policy
    #[account(init, seeds = [POLICY_PDA_SEED, vault.key().as_ref()], bump, payer = payer, space = 8 + Policy::SIZE)]
    pub policy: Account<'info, Policy>,

    /// Progress PDA that stores daily progress
    #[account(init, seeds = [PROGRESS_PDA_SEED, vault.key().as_ref()], bump, payer = payer, space = 8 + Progress::SIZE)]
    pub progress: Account<'info, Progress>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct CrankDistribute<'info> {
    /// Caller can be any signer (permissionless)
    pub payer: Signer<'info>,

    /// Vault (same used for PDA seeds)
    pub vault: AccountInfo<'info>,

    /// Investor Fee Position Owner PDA
    /// seeds: [VAULT_SEED, vault, INVESTOR_FEE_POS_OWNER_SEED]
    #[account(mut, seeds=[VAULT_SEED, vault.key().as_ref(), INVESTOR_FEE_POS_OWNER_SEED], bump)]
    pub investor_fee_pos_owner: Account<'info, InvestorFeePosOwner>,

    /// Policy PDA
    #[account(mut, seeds=[POLICY_PDA_SEED, vault.key().as_ref()], bump)]
    pub policy: Account<'info, Policy>,

    /// Progress PDA
    #[account(mut, seeds=[PROGRESS_PDA_SEED, vault.key().as_ref()], bump)]
    pub progress: Account<'info, Progress>,

    /// quote mint
    pub quote_mint: Account<'info, Mint>,

    /// program quote treasury ATA (must be same as policy.treasury)
    #[account(mut)]
    pub treasury: Account<'info, TokenAccount>,

    /// optional base treasury to enforce zero base fees
    /// If provided and base >0 crank will fail
    pub base_treasury: Option<Account<'info, TokenAccount>>,

    /// Creator quote ATA for remainder
    #[account(mut)]
    pub creator_quote_ata: Account<'info, TokenAccount>,

    /// token program
    pub token_program: Program<'info, Token>,

    /// cp-amm program ID if needed for CPI (as unchecked account)
    /// remaining_accounts may include cp-amm accounts & investor token ATAs (in same order)
    pub cp_amm_program: UncheckedAccount<'info>,
}

#[account]
pub struct Policy {
    pub bump: u8,
    pub investor_fee_share_bps: u16,
    pub daily_cap_lamports: Option<u64>,
    pub min_payout_lamports: u64,
    pub y0_total_alloc: u64,
    pub quote_mint: Pubkey,
    pub creator_quote_ata: Pubkey,
    pub streamflow_program: Pubkey,
}
impl Policy {
    // approximate size calculation
    pub const SIZE: usize = 1 + 2 + 9 + 8 + 8 + 32 + 32 + 32;
}

#[account]
pub struct Progress {
    pub bump: u8,
    pub last_distribution_ts: i64,
    pub current_day_cumulative: u64,
    pub carry_lamports: u64,
    pub page_cursor: u64,
    pub in_day: bool,
}
impl Progress {
    pub const SIZE: usize = 1 + 8 + 8 + 8 + 8 + 1;
}

#[account]
pub struct InvestorFeePosOwner {
    pub bump: u8,
    // other fields as needed
}

#[error_code]
pub enum HonoraryError {
    #[msg("Treasury mint mismatch")]
    TreasuryMintMismatch,
    #[msg("Quote mint mismatch")]
    QuoteMintMismatch,
    #[msg("Base token fees detected; aborting to enforce quote-only.")]
    BaseFeesDetected,
    #[msg("Overflow in arithmetic")]
    Overflow,
    #[msg("Missing cp-amm accounts when claim_ix_data provided")]
    MissingCpAmmAccounts,
    #[msg("Not in active day; start a new day first")]
    NotInActiveDay,
}

#[event]
pub struct HonoraryPositionInitialized {
    pub policy: Pubkey,
    pub progress: Pubkey,
    pub treasury: Pubkey,
    pub quote_mint: Pubkey,
    pub creator_quote_ata: Pubkey,
}

#[event]
pub struct QuoteFeesClaimed {
    pub claimed_amount: u64,
    pub treasury: Pubkey,
}

#[event]
pub struct InvestorPayoutPage {
    pub page_index: u64,
    pub distributed: u64,
}

#[event]
pub struct CreatorPayoutDayClose {
    pub day_ts: i64,
    pub remainder_to_creator: u64,
}

