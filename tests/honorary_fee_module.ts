import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";
import * as spl from "@solana/spl-token";
import assert from "assert";

describe("honorary_fee", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());
  const provider = anchor.getProvider();
  const program = anchor.workspace.HonoraryFee as Program;

  // helper to create mint and ata
  async function createMintAndAta() {
    const mint = await spl.createMint(
      provider.connection,
      provider.wallet.payer,
      provider.wallet.publicKey,
      null,
      6
    );
    const ata = await spl.getOrCreateAssociatedTokenAccount(
      provider.connection,
      provider.wallet.payer,
      mint,
      provider.wallet.publicKey
    );
    return { mint, ata };
  }

  it("initialize and crank across pages", async () => {
    const vault = Keypair.generate();
    // create a dummy vault account on chain
    const tx = await provider.connection.requestAirdrop(vault.publicKey, 1e9);
    await provider.connection.confirmTransaction(tx);

    // create quote mint and treasury
    const quoteMint = await spl.createMint(
      provider.connection,
      provider.wallet.payer,
      provider.wallet.publicKey,
      null,
      6
    );

    // derive PDAs consistent with program
    const [policyPda] = await PublicKey.findProgramAddress(
      [Buffer.from("policy"), vault.publicKey.toBuffer()],
      program.programId
    );
    const [progressPda] = await PublicKey.findProgramAddress(
      [Buffer.from("progress"), vault.publicKey.toBuffer()],
      program.programId
    );
    const [investorFeeOwnerPda, ownerBump] = await PublicKey.findProgramAddress(
      [Buffer.from("vault"), vault.publicKey.toBuffer(), Buffer.from("investor_fee_pos_owner")],
      program.programId
    );

    // create treasury ATA (owned by program PDA)
    const treasuryAta = await spl.createAccount(
      provider.connection,
      provider.wallet.payer,
      quoteMint,
      investorFeeOwnerPda // owner
    );

    // For convenience create creator ATA (recipient) for provider.wallet
    const creatorAta = await spl.getOrCreateAssociatedTokenAccount(
      provider.connection,
      provider.wallet.payer,
      quoteMint,
      provider.wallet.publicKey
    );

    // call initialize
    await program.methods
      .initialize(1000, null, new BN(1_000_000), new BN(1_000_000_000)) // investor_fee_share_bps=1000(10%), no cap, min_payout 1_000_000, y0
      .accounts({
        payer: provider.wallet.publicKey,
        vault: vault.publicKey,
        investorFeePosOwner: investorFeeOwnerPda,
        quoteMint: quoteMint,
        treasury: treasuryAta,
        creatorQuoteAta: creatorAta.address,
        streamflowProgram: program.programId,
        policy: policyPda,
        progress: progressPda,
        systemProgram: SystemProgram.programId,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    // Simulate claim by minting tokens to treasury (simulate cp-amm claim)
    // mint 10_000_000 tokens (10 units with 6 decimals)
    await spl.mintTo(
      provider.connection,
      provider.wallet.payer,
      quoteMint,
      treasuryAta,
      provider.wallet.publicKey,
      10_000_000
    );

    // Page 0: two investors
    // create two investor atAs
    const investor1 = Keypair.generate();
    const investor2 = Keypair.generate();

    // create their token ATAs
    const inv1Ata = await spl.getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, quoteMint, investor1.publicKey);
    const inv2Ata = await spl.getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, quoteMint, investor2.publicKey);

    // locked amounts (simulate Streamflow read): page 0 has inv1 locked 6, inv2 locked 4 (in raw lamports units)
    const lockedAmountsPage0 = [6000000, 4000000]; // using 6 decimals scale

    // call crank_distribute for page 0
    // We don't provide cp-amm claim_ix_data here because we already minted to treasury.
    await program.methods
      .crankDistribute(new BN(Math.floor(Date.now() / 1000)), null, null, lockedAmountsPage0.map(x => new BN(x)), false)
      .accounts({
        payer: provider.wallet.publicKey,
        vault: vault.publicKey,
        investorFeePosOwner: investorFeeOwnerPda,
        policy: policyPda,
        progress: progressPda,
        quoteMint: quoteMint,
        treasury: treasuryAta,
        baseTreasury: null,
        creatorQuoteAta: creatorAta.address,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        cpAmmProgram: program.programId,
      })
      // supply investor ATAs as remaining accounts (after cp-amm accounts)
      .remainingAccounts([
        { pubkey: inv1Ata.address, isSigner: false, isWritable: true },
        { pubkey: inv2Ata.address, isSigner: false, isWritable: true },
      ])
      .rpc();

    // check balances: both investor ATAs should have received proportional payouts
    const inv1Bal = (await provider.connection.getTokenAccountBalance(inv1Ata.address)).value.amount;
    const inv2Bal = (await provider.connection.getTokenAccountBalance(inv2Ata.address)).value.amount;
    console.log("inv balances:", inv1Bal, inv2Bal);

    // Now Page 1: final page with no investors (simulate next page or remainder)
    // call final page to route remainder to creator
    await program.methods
      .crankDistribute(new BN(Math.floor(Date.now() / 1000)), null, new BN(1), [], true) // page index 1, final
      .accounts({
        payer: provider.wallet.publicKey,
        vault: vault.publicKey,
        investorFeePosOwner: investorFeeOwnerPda,
        policy: policyPda,
        progress: progressPda,
        quoteMint: quoteMint,
        treasury: treasuryAta,
        baseTreasury: null,
        creatorQuoteAta: creatorAta.address,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        cpAmmProgram: program.programId,
      })
      .remainingAccounts([])
      .rpc();

    // check creator got remainder
    const creatorBal = (await provider.connection.getTokenAccountBalance(creatorAta.address)).value.amount;
    console.log("creator balance:", creatorBal);

    assert.ok(Number(inv1Bal) > 0 || Number(inv2Bal) > 0);
  });
});
