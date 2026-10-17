# rekt.page contracts

Source of the two Solana programs behind [rekt.page](https://rekt.page) and the $REKT token
(`BDALHN9WzRDbZzBgSJhuofa2NhaU5bJodQGNefRapump`, Token-2022).

| Program | Address | What it does |
|---|---|---|
| rekt-access | `REKTKhydBNJMvFZX6iELQ6eiTggPa18WYzwAdjQGCNH` | Stake $REKT for a 7- or 30-day licence, or pay once for a lifetime licence (half to the burn vault, half to the treasury). Staked tokens come back when the owner calls `unstake` after expiry |
| rekt-burn | `BURNSZMs9SKHsR5mqVDipDDShLXrPeGDXQUBZPJSQ1dg` | Burn vault. Receives $REKT and creator-fee SOL, buys $REKT back with that SOL, burns $REKT |

The doc comment at the top of each `programs/*/src/lib.rs` describes the rules in detail.

## Keys and limits

Both programs are upgradeable. Everything below describes the code as deployed, and a verified build
covers only the version on chain right now.

- **rekt-access:** the admin can change the 7-day, 30-day and lifetime prices (open stakes keep theirs),
  change the treasury that receives half of each lifetime payment, and hand the admin role on in two
  steps. The admin cannot move staked tokens: the only way out of the stake vault is `unstake`, back to
  the staker.
- **rekt-burn:** there is **no withdraw** for $REKT or SOL. $REKT leaves only by burning. SOL leaves only
  through a buyback that, in the same transaction, must deliver at least `min_out` $REKT to the vault and
  leave the buyback key with no more SOL than it started with. Each buyback is limited by a per-run SOL
  cap and a minimum interval set on chain.
- **Not audited.** A verified build means the code here is what runs on chain, not that it is free of bugs.

## Reproducible builds

Each program was built from a different commit of this repository, because rekt-access links the
rekt-burn crate and rekt-burn changed after rekt-access was deployed.

| Program | Commit | Tag | Deployed / last upgraded |
|---|---|---|---|
| rekt-access | `73779655faf641d454d34ea3bbb1e8e8bc24d79d` | `rekt-access-2026-10-05` | 2026-10-05, slot 453562852 |
| rekt-burn | `86553e3d63828573dcfa17e7fc63f8ce5151e1f6` | `rekt-burn-2026-10-17` | 2026-10-17, slot 457787055 |

Verification status is shown on [verify.osec.io](https://verify.osec.io) and on Solscan.

To check it yourself (needs Docker and
[solana-verify](https://github.com/Ellipsis-Labs/solana-verifiable-build)):

```bash
solana-verify verify-from-repo -u https://api.mainnet-beta.solana.com \
  --program-id REKTKhydBNJMvFZX6iELQ6eiTggPa18WYzwAdjQGCNH \
  --commit-hash 73779655faf641d454d34ea3bbb1e8e8bc24d79d --library-name rekt_access \
  -b solanafoundation/solana-verifiable-build:2.3.0 \
  https://github.com/rektpage/rekt-contracts

solana-verify verify-from-repo -u https://api.mainnet-beta.solana.com \
  --program-id BURNSZMs9SKHsR5mqVDipDDShLXrPeGDXQUBZPJSQ1dg \
  --commit-hash 86553e3d63828573dcfa17e7fc63f8ce5151e1f6 --library-name rekt_burn \
  -b solanafoundation/solana-verifiable-build:2.3.0 \
  https://github.com/rektpage/rekt-contracts
```

When the tool asks whether to upload verification data on chain, answer **n**. Only the upload made by
the upgrade authority counts.

If you build by hand, build one program at a time:
`cargo build-sbf --manifest-path programs/<name>/Cargo.toml -- --locked`. A plain build of the whole
workspace turns on rekt-burn's `cpi` feature (rekt-access depends on it) and produces a rekt-burn binary
with no entrypoint.

Toolchain: Anchor 0.32.1, Solana 2.3.0.

## Tests

```bash
docker run --rm -v "$PWD":/work -w /work solanafoundation/anchor:v0.32.1 bash -lc "
  solana-keygen new --no-bip39-passphrase -s -o ~/.config/solana/id.json &&
  npm install && anchor build -- --features fast-clock && anchor test --skip-build"
```

`fast-clock` makes one day one second so the tests can run stakes to expiry. Deployed builds never use it.

## Security

Please report vulnerabilities privately through this repository's
[Security tab](https://github.com/rektpage/rekt-contracts/security/advisories/new), not in a public issue.

## License

All rights reserved. The code is public for reading and verification only; see [LICENSE](LICENSE).
