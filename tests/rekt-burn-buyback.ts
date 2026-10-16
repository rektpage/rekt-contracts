// Burn vault buyback (2026-10-17). Runs after rekt-access.ts on the same validator: the burn vault is
// already initialized there (governor = provider wallet, mint authority = provider wallet).
// The swap in the middle is mocked by a "pool" keypair: the buyer pays it SOL, it pays the vault $REKT.
import * as anchor from '@coral-xyz/anchor'
import { Program, BN } from '@coral-xyz/anchor'
import {
  Keypair,
  PublicKey,
  SystemProgram,
  LAMPORTS_PER_SOL,
  Transaction,
  TransactionInstruction,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  sendAndConfirmTransaction
} from '@solana/web3.js'
import { TOKEN_2022_PROGRAM_ID, createAssociatedTokenAccount, createTransferCheckedInstruction, mintTo } from '@solana/spl-token'
import { expect } from 'chai'
import type { RektBurn } from '../target/types/rekt_burn'

const DECIMALS = 6

async function expectError(promise: Promise<unknown>, code: string) {
  try {
    await promise
  } catch (error: any) {
    const text = String(error?.message ?? error) + String(error?.logs ?? '') + String(error?.transactionLogs ?? '')
    if (!text.includes(code)) console.log('---', code, text)
    expect(text).to.include(code)
    return
  }
  expect.fail(`expected ${code}`)
}

describe('rekt-burn buyback', () => {
  const provider = anchor.AnchorProvider.env()
  anchor.setProvider(provider)
  const program = anchor.workspace.RektBurn as Program<RektBurn>
  const connection = new anchor.web3.Connection(provider.connection.rpcEndpoint, 'confirmed')
  const governor = (provider.wallet as anchor.Wallet).payer
  const pda = (seed: string) => PublicKey.findProgramAddressSync([Buffer.from(seed)], program.programId)[0]
  const state = pda('state')
  const vault = pda('vault')
  const sol = pda('sol')
  const buyback = pda('buyback')

  const buyer = Keypair.generate()
  const pool = Keypair.generate()
  const bob = Keypair.generate()
  let mint: PublicKey
  let poolTokens: PublicKey

  const setBuyback = (who: Keypair, key: PublicKey, maxSol: number, interval: number) =>
    program.methods
      .setBuyback(key, new BN(maxSol * LAMPORTS_PER_SOL), new BN(interval))
      .accountsPartial({ governor: who.publicKey, state, buyback, systemProgram: SystemProgram.programId })
      .signers([who])
      .rpc({ commitment: 'confirmed', preflightCommitment: 'confirmed' })

  const start = (signer: Keypair, lamports: number, minOut: number) =>
    program.methods
      .buybackStart(new BN(lamports), new BN(minOut))
      .accountsPartial({ buyer: signer.publicKey, state, buyback, sol, vault, instructions: SYSVAR_INSTRUCTIONS_PUBKEY, systemProgram: SystemProgram.programId })
      .instruction()
  const end = (signer: Keypair) => program.methods.buybackEnd().accountsPartial({ buyer: signer.publicKey, state, buyback, vault }).instruction()
  // mocked swap: buyer pays the pool, the pool pays the vault
  const paySol = (from: Keypair, lamports: number) => SystemProgram.transfer({ fromPubkey: from.publicKey, toPubkey: pool.publicKey, lamports })
  const payRekt = (amount: number) => createTransferCheckedInstruction(poolTokens, mint, vault, pool.publicKey, amount, DECIMALS, [], TOKEN_2022_PROGRAM_ID)
  const send = (ixs: TransactionInstruction[], signers: Keypair[]) => sendAndConfirmTransaction(connection, new Transaction().add(...ixs), signers, { commitment: 'confirmed', preflightCommitment: 'confirmed' })

  before(async () => {
    for (const kp of [buyer, pool, bob]) {
      const sig = await connection.requestAirdrop(kp.publicKey, 2 * LAMPORTS_PER_SOL)
      await connection.confirmTransaction(sig, 'confirmed')
    }
    mint = (await program.account.state.fetch(state)).mint
    poolTokens = await createAssociatedTokenAccount(connection, governor, mint, pool.publicKey, undefined, TOKEN_2022_PROGRAM_ID)
    await mintTo(connection, governor, mint, poolTokens, governor, 1_000_000_000, [], undefined, TOKEN_2022_PROGRAM_ID)
    // creator-fee SOL waiting in the SOL address
    await send([SystemProgram.transfer({ fromPubkey: governor.publicKey, toPubkey: sol, lamports: 2 * LAMPORTS_PER_SOL })], [governor])
  })

  it('only the governor sets the buyback key and limits', async () => {
    await expectError(setBuyback(bob, bob.publicKey, 1, 60), 'NotGovernor')
    await expectError(setBuyback(governor, buyer.publicKey, 1, 0), 'BadSetting')
    await setBuyback(governor, buyer.publicKey, 1, 60)
    const b = await program.account.buyback.fetch(buyback)
    expect(b.buyer.toBase58()).to.equal(buyer.publicKey.toBase58())
    expect(b.maxSol.toString()).to.equal(String(LAMPORTS_PER_SOL))
  })

  it('refuses a start with no buyback_end after it', async () => {
    await expectError(send([await start(buyer, 0.1 * LAMPORTS_PER_SOL, 1)], [buyer]), 'NoBuybackEnd')
    await expectError(send([await start(buyer, 0.1 * LAMPORTS_PER_SOL, 1), await start(buyer, 0.1 * LAMPORTS_PER_SOL, 1), await end(buyer)], [buyer]), 'NoBuybackEnd')
  })

  it('refuses anyone but the buyback key', async () => {
    await expectError(send([await start(bob, 0.1 * LAMPORTS_PER_SOL, 1), await end(bob)], [bob]), 'NotBuyer')
  })

  it('refuses more than the per-buyback cap', async () => {
    await expectError(send([await start(buyer, 1.5 * LAMPORTS_PER_SOL, 1), await end(buyer)], [buyer]), 'OverCap')
  })

  it('fails the whole transaction when the vault gets less than min_out', async () => {
    const solBefore = await connection.getBalance(sol)
    await expectError(
      send([await start(buyer, 0.5 * LAMPORTS_PER_SOL, 2000), paySol(buyer, 0.5 * LAMPORTS_PER_SOL), payRekt(1000), await end(buyer)], [buyer, pool]),
      'ShortBuyback'
    )
    expect(await connection.getBalance(sol)).to.equal(solBefore)
  })

  it('fails when the buyer keeps the SOL', async () => {
    await expectError(send([await start(buyer, 0.5 * LAMPORTS_PER_SOL, 1000), payRekt(1000), await end(buyer)], [buyer, pool]), 'SolKept')
  })

  it('keeps the SOL address rent-exempt', async () => {
    await setBuyback(governor, buyer.publicKey, 100, 60)
    const all = await connection.getBalance(sol)
    await expectError(send([await start(buyer, all, 1), paySol(buyer, all), payRekt(1), await end(buyer)], [buyer, pool]), 'NotEnoughSol')
  })

  it('buys back: SOL leaves the SOL address, $REKT lands in the vault and is counted in slot 1', async () => {
    const before = await program.account.state.fetch(state)
    const solBefore = await connection.getBalance(sol)
    await send([await start(buyer, 0.5 * LAMPORTS_PER_SOL, 1000), paySol(buyer, 0.5 * LAMPORTS_PER_SOL), payRekt(1234), await end(buyer)], [buyer, pool])
    const after = await program.account.state.fetch(state)
    expect(after.deposited[1].sub(before.deposited[1]).toNumber()).to.equal(1234)
    expect(after.solSpent.sub(before.solSpent).toNumber()).to.equal(0.5 * LAMPORTS_PER_SOL)
    expect(after.buybacks).to.equal(before.buybacks + 1)
    expect(solBefore - (await connection.getBalance(sol))).to.equal(0.5 * LAMPORTS_PER_SOL)
    expect((await program.account.buyback.fetch(buyback)).inProgress).to.equal(false)
  })

  it('waits the minimum interval between buybacks', async () => {
    await setBuyback(governor, buyer.publicKey, 1, 3600)
    await expectError(
      send([await start(buyer, 0.1 * LAMPORTS_PER_SOL, 1), paySol(buyer, 0.1 * LAMPORTS_PER_SOL), payRekt(1), await end(buyer)], [buyer, pool]),
      'TooSoon'
    )
  })

  it('switches off with the default key', async () => {
    await setBuyback(governor, PublicKey.default, 1, 60)
    await expectError(send([await start(buyer, 0.1 * LAMPORTS_PER_SOL, 1), await end(buyer)], [buyer]), 'NotBuyer')
  })
})
