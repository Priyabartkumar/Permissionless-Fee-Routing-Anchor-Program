<B>Permissionless Fee Routing Anchor Program</B>

<li>This Anchor-compatible Solana program provides a permissionless module for fee routing and investor distribution.</li> 
<li>The program exposes clear instruction interfaces and account requirements, supporting end-to-end testing against cp-amm and Streamflow protocols via a local validator.</li>
<br>
<B>Setup
Prerequisites:
<br>{Solana CLI (latest version)
Anchor CLI
Rust toolchain (stable)
Node.js (for testing if required)}</B>

<br>
<br>
<b>METHOD TO INSTALL</b>
<br>Installation:{
bash,
git clone https://github.com/Priyabartkumar/Permissionless-Fee-Routing-Anchor-Program.git,
cd Permissionless-Fee-Routing-Anchor-Program,
anchor build}

Wiring and PDAs: Main Program Accounts
Vault: Used as a seed for deterministic PDAs.

InvestorFeePosOwner PDA: [VAULT_SEED, vault_pubkey, INVESTOR_FEE_POS_OWNER_SEED] - controls position authority and fee distribution.

Policy PDA: Stores configuration for daily caps, min payouts, and investor share.

Progress PDA: Tracks daily cumulative payouts, page cursor, and carryover.

Associated Token Accounts
Treasury: SPL Token account used for incoming fees.

Creator Quote ATA: Destination for remaining fees after distribution.
<br>
<br>
<b>#INSTRUCTION_INTERFACE</b>{
1. initialize
Purpose: Sets up policy, progress, and PDAs.

Accounts:
{<span>Signer (payer)</span>, Vault (input), InvestorFeePosOwner PDA (init),Quote mint, Treasury(SPL Token), Creator Quote ATA, Policy PDA (init), Progress PDA (init)
}



System/Token Program

Rent sysvar
}
2. crank_distribute
Purpose: Permissionless, once-per-day crank to distribute fees to investors and handle integrations with cp-amm and Streamflow.

Accounts:{

Payer

Vault

InvestorFeePosOwner

Policy PDA

Progress PDA

Quote mint

Treasury

Creator Quote ATA

Base Treasury (optional, for enforcing zero base fees)

Token Program

cp-amm Program
}
Testing: End-to-End Flows
Run all tests in a local validator:

bash:
anchor test
Tests simulate flow with cp-amm and Streamflow accounts, validating correct fee splitting, cranking, and end-of-day rollover.

Failure modes are covered, such as mismatched mints, zero base fees, arithmetic overflow, out-of-order pages, and idempotency checks.

<B><U>Policies</U></B><li>
<B>1)Daily Cap</B>: Maximum total payout per day to investors.     <B>2)Min Payout</B>: Minimum lamports for any payout (dust is carried over).
<B>3)Investor Share BPS</B>: Share for investors in basis points (1/100th of percent).
<B>4)Carry Lamports</B>: Amount carried over to next distribution day.

Failure Modes
Treasury Mint Mismatch: Prevents payouts if treasury account mint doesn't match policy.

1) Quote Mint Mismatch: Prevents distribution to wrong quote mint.       2) Base Fees Detected: Fails crank if base token fees present.
3) Overflow: Checked arithmetic on payouts.                              4)Missing cp-amm Accounts: Requires integration accounts for CPI.
5) Not In Active Day: Disallows crank when outside distribution window.

Useful Resources
Solana Anchor Docs
Solana Program Derived Addresses
SPL Token Program





