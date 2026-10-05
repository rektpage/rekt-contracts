// Run against a build with `--features fast-clock` (one "day" = one second).
import * as anchor from '@coral-xyz/anchor'
import { Program, BN } from '@coral-xyz/anchor'
import { Keypair, PublicKey, SystemProgram, LAMPORTS_PER_SOL, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import {
  TOKEN_2022_PROGRAM_ID,
  ExtensionType,
  LENGTH_SIZE,
  TYPE_SIZE,
  createAssociatedTokenAccount,
  createInitializeMetadataPointerInstruction,
  createInitializeMintInstruction,
  getMintLen,
  mintTo,
  getAccount,
  getMint
} from '@solana/spl-token'
import { createInitializeInstruction, pack } from '@solana/spl-token-metadata'
import { expect } from 'chai'
import type { RektAccess } from '../target/types/rekt_access'
import type { RektBurn } from '../target/types/rekt_burn'

const DECIMALS = 6
const UNIT = 10n ** BigInt(DECIMALS)
const rekt = (n: number) => new BN((BigInt(n) * UNIT).toString())
const STAKE_7D = rekt(10_000_000)
const STAKE_30D = rekt(20_000_000)
const LIFETIME = rekt(20_000_000)

const sleep = (ms: number) => new Promise(r => setTimeout(r, ms))

async function expectError(promise: Promise<unknown>, code: string) {
  try {
    await promise
  } catch (error) {
    expect(String(error)).to.include(code)
    return
  }
  expect.fail(`expected ${code}`)
}

describe('rekt-access', () => {
  const provider = anchor.AnchorProvider.env()
  anchor.setProvider(provider)
  const program = anchor.workspace.RektAccess as Program<RektAccess>
  const burnProgram = anchor.workspace.RektBurn as Program<RektBurn>
  const [burnState] = PublicKey.findProgramAddressSync([Buffer.from('state')], burnProgram.programId)
  const [burnVault] = PublicKey.findProgramAddressSync([Buffer.from('vault')], burnProgram.programId)
  const connection = provider.connection
  const admin = (provider.wallet as anchor.Wallet).payer

  const dev = Keypair.generate() // treasury owner
  const alice = Keypair.generate()
  const bob = Keypair.generate()
  let mint: PublicKey
  let treasury: PublicKey
  let aliceTokens: PublicKey
  let bobTokens: PublicKey

  const [config] = PublicKey.findProgramAddressSync([Buffer.from('config')], program.programId)
  const [vault] = PublicKey.findProgramAddressSync([Buffer.from('vault')], program.programId)
  const stakeOf = (owner: PublicKey) => PublicKey.findProgramAddressSync([Buffer.from('stake'), owner.toBuffer()], program.programId)[0]
  const lifetimeOf = (owner: PublicKey) => PublicKey.findProgramAddressSync([Buffer.from('lifetime'), owner.toBuffer()], program.programId)[0]
  const balance = async (account: PublicKey) => (await getAccount(connection, account, 'processed', TOKEN_2022_PROGRAM_ID)).amount

  before(async () => {
    for (const kp of [dev, alice, bob]) {
      const sig = await connection.requestAirdrop(kp.publicKey, 2 * LAMPORTS_PER_SOL)
      await connection.confirmTransaction(sig, 'confirmed')
    }
    // Same shape as the mainnet REKT mint: Token-2022 with MetadataPointer + TokenMetadata extensions,
    // so the vault and user accounts are created exactly as they will be on mainnet
    const mintKp = Keypair.generate()
    mint = mintKp.publicKey
    const metadata = { mint, name: 'rekt.page', symbol: 'REKT', uri: 'https://rekt.page', additionalMetadata: [] as [string, string][] }
    const mintLen = getMintLen([ExtensionType.MetadataPointer])
    const fullLen = mintLen + TYPE_SIZE + LENGTH_SIZE + pack(metadata).length
    const tx = new Transaction().add(
      SystemProgram.createAccount({
        fromPubkey: admin.publicKey,
        newAccountPubkey: mint,
        space: mintLen,
        lamports: await connection.getMinimumBalanceForRentExemption(fullLen),
        programId: TOKEN_2022_PROGRAM_ID
      }),
      createInitializeMetadataPointerInstruction(mint, admin.publicKey, mint, TOKEN_2022_PROGRAM_ID),
      createInitializeMintInstruction(mint, DECIMALS, admin.publicKey, null, TOKEN_2022_PROGRAM_ID),
      createInitializeInstruction({
        programId: TOKEN_2022_PROGRAM_ID,
        metadata: mint,
        updateAuthority: admin.publicKey,
        mint,
        mintAuthority: admin.publicKey,
        name: metadata.name,
        symbol: metadata.symbol,
        uri: metadata.uri
      })
    )
    await sendAndConfirmTransaction(connection, tx, [admin, mintKp], { commitment: 'confirmed' })
    treasury = await createAssociatedTokenAccount(connection, admin, mint, dev.publicKey, undefined, TOKEN_2022_PROGRAM_ID)
    aliceTokens = await createAssociatedTokenAccount(connection, admin, mint, alice.publicKey, undefined, TOKEN_2022_PROGRAM_ID)
    bobTokens = await createAssociatedTokenAccount(connection, admin, mint, bob.publicKey, undefined, TOKEN_2022_PROGRAM_ID)
    await mintTo(connection, admin, mint, aliceTokens, admin, 100_000_000n * UNIT, [], undefined, TOKEN_2022_PROGRAM_ID)
    await mintTo(connection, admin, mint, bobTokens, admin, 100_000_000n * UNIT, [], undefined, TOKEN_2022_PROGRAM_ID)
  })

  const stakeIx = (user: Keypair, tokens: PublicKey, days: number, expected: BN) =>
    program.methods
      .stake(days, expected)
      .accountsPartial({ user: user.publicKey, config, stake: stakeOf(user.publicKey), mint, userTokens: tokens, vault, tokenProgram: TOKEN_2022_PROGRAM_ID, systemProgram: SystemProgram.programId })
      .signers([user])
      .rpc()

  const unstakeIx = (user: Keypair, tokens: PublicKey) =>
    program.methods
      .unstake()
      .accountsPartial({ user: user.publicKey, config, stake: stakeOf(user.publicKey), mint, userTokens: tokens, vault, tokenProgram: TOKEN_2022_PROGRAM_ID })
      .signers([user])
      .rpc()

  const lifetimeIx = (user: Keypair, tokens: PublicKey, maxPrice: BN) =>
    program.methods
      .buyLifetime(maxPrice)
      .accountsPartial({
        user: user.publicKey,
        config,
        lifetime: lifetimeOf(user.publicKey),
        mint,
        userTokens: tokens,
        treasury,
        burnProgram: burnProgram.programId,
        burnState,
        burnVault,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId
      })
      .signers([user])
      .rpc()

  const [programData] = PublicKey.findProgramAddressSync([program.programId.toBuffer()], new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111'))
  const initIx = (deployer: Keypair) =>
    program.methods
      .initialize(admin.publicKey, STAKE_7D, STAKE_30D, LIFETIME)
      .accountsPartial({
        deployer: deployer.publicKey,
        program: program.programId,
        programData,
        config,
        mint,
        vault,
        treasury,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId
      })
      .signers([deployer])
      .rpc()

  it('refuses initialize from anyone but the upgrade authority', async () => {
    await expectError(initIx(alice), 'NotUpgradeAuthority')
  })

  it('initializes with the 10-05 amounts', async () => {
    await initIx(admin)
    const c = await program.account.config.fetch(config)
    expect(c.admin.toBase58()).to.equal(admin.publicKey.toBase58())
    expect(c.stakeWeek.toString()).to.equal(STAKE_7D.toString())
    expect(c.stakeMonth.toString()).to.equal(STAKE_30D.toString())
    expect(c.lifetimePrice.toString()).to.equal(LIFETIME.toString())
    expect(c.treasury.toBase58()).to.equal(treasury.toBase58())
  })

  const [burnProgramData] = PublicKey.findProgramAddressSync([burnProgram.programId.toBuffer()], new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111'))
  const burnInit = (deployer: Keypair) =>
    burnProgram.methods
      .initialize(admin.publicKey, admin.publicKey)
      .accountsPartial({
        deployer: deployer.publicKey,
        program: burnProgram.programId,
        programData: burnProgramData,
        state: burnState,
        mint,
        vault: burnVault,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId
      })
      .signers([deployer])
      .rpc()

  it('burn vault: refuses initialize from anyone but its upgrade authority', async () => {
    await expectError(burnInit(alice), 'NotUpgradeAuthority')
  })

  it('burn vault: initializes', async () => {
    await burnInit(admin)
    const st = await burnProgram.account.state.fetch(burnState)
    expect(st.admin.toBase58()).to.equal(admin.publicKey.toBase58())
    expect(st.vault.toBase58()).to.equal(burnVault.toBase58())
  })

  it('refuses a stake whose expected amount differs from the price', async () => {
    await expectError(stakeIx(alice, aliceTokens, 7, rekt(1)), 'PriceChanged')
  })

  it('refuses an unknown plan', async () => {
    await expectError(stakeIx(alice, aliceTokens, 14, STAKE_7D), 'UnknownPlan')
  })

  it('locks 10M for 7 days', async () => {
    const before = await balance(aliceTokens)
    await stakeIx(alice, aliceTokens, 7, STAKE_7D)
    const s = await program.account.stakeAccount.fetch(stakeOf(alice.publicKey))
    expect(s.amount.toString()).to.equal(STAKE_7D.toString())
    expect(s.planDays).to.equal(7)
    expect(s.expiresAt.sub(s.startedAt).toNumber()).to.equal(7) // fast-clock: 7 "days" = 7 s
    expect((before - (await balance(aliceTokens))).toString()).to.equal(STAKE_7D.toString())
    expect((await balance(vault)).toString()).to.equal(STAKE_7D.toString())
  })

  it('allows only one open stake per wallet', async () => {
    await expectError(stakeIx(alice, aliceTokens, 30, STAKE_30D), 'already in use')
  })

  it('keeps the tokens locked until expiry', async () => {
    await expectError(unstakeIx(alice, aliceTokens), 'StillLocked')
  })

  it('does not let someone else unstake', async () => {
    // bob tries to close alice's stake: the PDA is derived from the signer, so it is bob's (missing) one
    await expectError(
      program.methods
        .unstake()
        .accountsPartial({ user: bob.publicKey, config, stake: stakeOf(alice.publicKey), mint, userTokens: bobTokens, vault, tokenProgram: TOKEN_2022_PROGRAM_ID })
        .signers([bob])
        .rpc(),
      'ConstraintSeeds'
    )
  })

  it('returns the tokens after expiry and closes the stake', async () => {
    await sleep(9_000)
    const before = await balance(aliceTokens)
    await unstakeIx(alice, aliceTokens)
    expect(((await balance(aliceTokens)) - before).toString()).to.equal(STAKE_7D.toString())
    expect(await connection.getAccountInfo(stakeOf(alice.publicKey), 'processed')).to.equal(null)
    expect((await balance(vault)).toString()).to.equal('0')
  })

  it('keeps an open stake on its original amount when the admin changes prices', async () => {
    await stakeIx(bob, bobTokens, 30, STAKE_30D)
    await program.methods.setParams(rekt(5), rekt(6), rekt(7)).accountsPartial({ admin: admin.publicKey, config }).rpc()
    const s = await program.account.stakeAccount.fetch(stakeOf(bob.publicKey))
    expect(s.amount.toString()).to.equal(STAKE_30D.toString())
    // and a new stake has to accept the new price
    await expectError(stakeIx(alice, aliceTokens, 7, STAKE_7D), 'PriceChanged')
    await program.methods.setParams(STAKE_7D, STAKE_30D, LIFETIME).accountsPartial({ admin: admin.publicKey, config }).rpc()
  })

  it('refuses parameter changes from anyone but the admin', async () => {
    await expectError(
      program.methods.setParams(rekt(1), rekt(1), rekt(1)).accountsPartial({ admin: bob.publicKey, config }).signers([bob]).rpc(),
      'NotAdmin'
    )
  })

  it('refuses a lifetime purchase above the buyer cap', async () => {
    await expectError(lifetimeIx(alice, aliceTokens, rekt(1)), 'PriceChanged')
  })

  it('sells a lifetime licence for 20M, split 1:1 between the burn vault and the treasury', async () => {
    const before = await balance(treasury)
    await lifetimeIx(alice, aliceTokens, LIFETIME)
    const half = BigInt(LIFETIME.toString()) / 2n
    expect(((await balance(treasury)) - before).toString()).to.equal(half.toString())
    expect((await balance(burnVault)).toString()).to.equal(half.toString())
    const st = await burnProgram.account.state.fetch(burnState)
    expect(st.deposited[0].toString()).to.equal(half.toString()) // tagged "lifetime"
    const l = await program.account.lifetime.fetch(lifetimeOf(alice.publicKey))
    expect(l.owner.toBase58()).to.equal(alice.publicKey.toBase58())
    expect(l.paid.toString()).to.equal(LIFETIME.toString())
  })

  it('does not sell a second lifetime licence to the same wallet', async () => {
    await expectError(lifetimeIx(alice, aliceTokens, LIFETIME), 'already in use')
  })

  it('burn vault: nobody but the staking program can tag a deposit as lifetime', async () => {
    await expectError(
      burnProgram.methods
        .deposit(rekt(1), 0)
        .accountsPartial({ payer: bob.publicKey, tagger: bob.publicKey, state: burnState, mint, from: bobTokens, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID })
        .signers([bob])
        .rpc(),
      'NotAllowedSource'
    )
  })

  it('burn vault: nobody can tag a deposit as creator fee (that slot is for proven buybacks)', async () => {
    await expectError(
      burnProgram.methods
        .deposit(rekt(1), 1)
        // even with the admin vouching
        .accountsPartial({ payer: bob.publicKey, tagger: admin.publicKey, state: burnState, mint, from: bobTokens, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID })
        .signers([bob])
        .rpc(),
      'UnknownSource'
    )
  })

  it('burn vault: only the admin can tag developer deposits', async () => {
    await expectError(
      burnProgram.methods
        .deposit(rekt(1), 2)
        .accountsPartial({ payer: bob.publicKey, tagger: bob.publicKey, state: burnState, mint, from: bobTokens, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID })
        .signers([bob])
        .rpc(),
      'NotAllowedSource'
    )
    // bob pays, the admin vouches for the tag
    await burnProgram.methods
      .deposit(rekt(3), 2)
      .accountsPartial({ payer: bob.publicKey, tagger: admin.publicKey, state: burnState, mint, from: bobTokens, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID })
      .signers([bob])
      .rpc()
    const st = await burnProgram.account.state.fetch(burnState)
    expect(st.deposited[2].toString()).to.equal(rekt(3).toString())
  })

  it('burn vault: only the admin can burn, and burning lowers the supply', async () => {
    await expectError(
      burnProgram.methods.burn(rekt(1)).accountsPartial({ admin: bob.publicKey, state: burnState, mint, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID }).signers([bob]).rpc(),
      'NotAdmin'
    )
    const supplyBefore = (await getMint(connection, mint, 'processed', TOKEN_2022_PROGRAM_ID)).supply
    const held = await balance(burnVault)
    await burnProgram.methods.burn(new BN(held.toString())).accountsPartial({ admin: admin.publicKey, state: burnState, mint, vault: burnVault, tokenProgram: TOKEN_2022_PROGRAM_ID }).rpc()
    expect((await balance(burnVault)).toString()).to.equal('0')
    expect((supplyBefore - (await getMint(connection, mint, 'processed', TOKEN_2022_PROGRAM_ID)).supply).toString()).to.equal(held.toString())
    const st = await burnProgram.account.state.fetch(burnState)
    expect(st.burnedTotal.toString()).to.equal(held.toString())
    expect(st.burns).to.equal(1)
  })

  it('burn vault: only the governor can replace the admin', async () => {
    await expectError(burnProgram.methods.setAdmin(bob.publicKey).accountsPartial({ governor: bob.publicKey, state: burnState }).signers([bob]).rpc(), 'NotGovernor')
    await burnProgram.methods.setAdmin(dev.publicKey).accountsPartial({ governor: admin.publicKey, state: burnState }).rpc()
    expect((await burnProgram.account.state.fetch(burnState)).admin.toBase58()).to.equal(dev.publicKey.toBase58())
  })

  it('hands over the admin role in two steps', async () => {
    await expectError(
      program.methods.acceptAdmin().accountsPartial({ newAdmin: dev.publicKey, config }).signers([dev]).rpc(),
      'NotPendingAdmin'
    )
    await program.methods.proposeAdmin(dev.publicKey).accountsPartial({ admin: admin.publicKey, config }).rpc()
    await program.methods.acceptAdmin().accountsPartial({ newAdmin: dev.publicKey, config }).signers([dev]).rpc()
    const c = await program.account.config.fetch(config)
    expect(c.admin.toBase58()).to.equal(dev.publicKey.toBase58())
    // the old admin is now just a user
    await expectError(program.methods.setParams(rekt(1), rekt(1), rekt(1)).accountsPartial({ admin: admin.publicKey, config }).rpc(), 'NotAdmin')
  })

  it('tracks the totals', async () => {
    const c = await program.account.config.fetch(config)
    expect(c.stakedTotal.toString()).to.equal(STAKE_30D.toString()) // bob's 30-day stake is still open
    expect(c.activeStakes).to.equal(1)
    expect(c.lifetimes).to.equal(1)
  })
})
