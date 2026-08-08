#!/usr/bin/env node
// One-shot golden generator for the Deriverse adapter (M1a acceptance 2).
// Run from anywhere:
//   node gen_goldens.cjs <kit-clone-root> > goldens.json
// Preconditions: (cd <kit-clone-root> && npm ci && npm run build)
// Fully offline: PDAs are pure crypto; the deposit builder's rpcGetSlot is
// stubbed; signing uses a fixed-seed DUMMY key + fixed blockhash.
'use strict';
const path = require('path');

async function main() {
  const kitRoot = process.argv[2];
  if (!kitRoot) {
    console.error('usage: node gen_goldens.cjs <kit-clone-root>');
    process.exit(1);
  }
  const req = (p) => require(path.join(kitRoot, p));
  const kit = req('node_modules/@solana/kit');
  const spot = req('dist/engine/spot-instructions.js');
  const perp = req('dist/engine/perp-instructions.js');
  const instrs = req('dist/engine/instructions.js');
  const helpers = req('dist/engine/account-helpers.js');
  const utils = req('dist/engine/utils.js');
  const rootIndex = req('dist/index.js');
  const kitPkg = req('package.json');
  const { AccountType } = rootIndex;

  const b58 = (bytes) => kit.getBase58Decoder().decode(bytes);
  const filled = (n) => new Uint8Array(32).fill(n);
  const addrFromByte = (n) => kit.address(b58(filled(n)));
  const u32le = (v) => { const b = Buffer.alloc(4); b.writeUInt32LE(v); return [...b]; };
  const u64le = (v) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(v)); return [...b]; };
  const hex = (u8) => Buffer.from(u8).toString('hex');

  const PROGRAM_ID = kit.address('DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD');
  const VERSION = 1;
  const WSOL_MINT = kit.address('So11111111111111111111111111111111111111112');
  const USDC_MINT = kit.address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
  const TOKEN_PROGRAM = kit.address('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');

  // Dummy signer: fixed seed 0x07 × 32 (matches Rust SigningKey::from_bytes([7u8;32])).
  const signer = await kit.createKeyPairSignerFromPrivateKeyBytes(new Uint8Array(32).fill(7));
  // Fixed blockhash: base58 of 0x09 × 32.
  const blockhash = b58(filled(9));
  const lastValidBlockHeight = 360001000n;

  // ── Pure PDA derivations (kit account-helpers/utils — the porting SoT) ──
  const drvsAuthority = (await kit.getProgramDerivedAddress({
    programAddress: PROGRAM_ID, seeds: ['ndxnt'],
  }))[0];
  const helperCtx = { programId: PROGRAM_ID, version: VERSION, drvsAuthority };
  const rootAccount = await helpers.getAccountByTag(helperCtx, AccountType.ROOT);
  const communityAccount = await helpers.getAccountByTag(helperCtx, AccountType.COMMUNITY);
  const clientPrimaryAccount = await helpers.findClientPrimaryAccount(helperCtx, signer.address);
  const clientCommunityAccount = await helpers.findClientCommunityAccount(helperCtx, signer.address);
  const programTokenAccountUsdc = await helpers.getTokenAccount(helperCtx, USDC_MINT);
  const ataUsdc = await utils.findAssociatedTokenAddress(signer.address, TOKEN_PROGRAM, USDC_MINT);
  const LUT_SLOT = 359999999;
  const lutForSlot = await utils.getLookupTableAddress(signer.address, LUT_SLOT);

  // ── Synthetic-but-fixed venue directory (identical values load into the
  //    Rust VenueContext in the golden tests) ──
  const instrHeader = {
    instrId: 0,
    assetTokenId: 1,
    crncyTokenId: 2,
    mapsAddress: addrFromByte(21),
    lutAddress: addrFromByte(22),
  };
  // mask low byte = decimals (kit tokenDec); USDC vault/WSOL vault fixed.
  const tokens = new Map([
    [1, { id: 1, address: WSOL_MINT, programAddress: addrFromByte(31), mask: 9 }],
    [2, { id: 2, address: USDC_MINT, programAddress: addrFromByte(32), mask: 6 }],
  ]);

  const ctx = {
    signer: signer.address,
    rootAccount,
    programId: PROGRAM_ID,
    version: VERSION,
    commitment: 'confirmed',
    drvsAuthority,
    privateMode: false,
    uiNumbers: true,
    tokens,
    clientPrimaryAccount,
    clientCommunityAccount,
    communityAccount,
    refClientPrimaryAccount: null,
    refClientCommunityAccount: null,
  };
  const instr = { header: instrHeader };

  const dumpIx = (ix) => ({
    program: ix.programAddress,
    data_hex: hex(ix.data),
    accounts: (ix.accounts ?? []).map((a) => ({
      address: a.address,
      signer: a.role >= 2, // kit AccountRole: READONLY_SIGNER=2, WRITABLE_SIGNER=3
      writable: (a.role & 1) === 1,
    })),
  });

  // ── Instruction goldens (human-unit args ⇒ kit's scaling layer is IN the
  //    bytes — the NT Quantity→i64 coverage the spec demands) ──
  const newSpotArgs = { side: 0, instrId: 0, price: 152.25, qty: 0.033, ioc: 0, orderType: 0, edgePrice: 0 };
  const newSpotIx = await spot.buildNewSpotOrderInstruction(ctx, newSpotArgs, instr);
  const newSpotIocArgs = { side: 1, instrId: 0, price: 151.5, qty: 0.033, ioc: 1, orderType: 0, edgePrice: 0 };
  const newSpotIocIx = await spot.buildNewSpotOrderInstruction(ctx, newSpotIocArgs, instr);
  // Mixed control (review F6): side=0 WITH ioc=1 — decorrelates the side and
  // ioc bytes, which the base (0,0) and ioc (1,1) goldens leave coupled.
  const newSpotMixedArgs = { side: 0, instrId: 0, price: 152.25, qty: 0.033, ioc: 1, orderType: 0, edgePrice: 0 };
  const newSpotMixedIx = await spot.buildNewSpotOrderInstruction(ctx, newSpotMixedArgs, instr);
  const cancelArgs = { side: 0, instrId: 0, orderId: 987654321012345 };
  const cancelIx = await spot.buildSpotOrderCancelInstruction(ctx, cancelArgs, instr);
  const rpcGetSlot = async () => BigInt(LUT_SLOT + 1); // builder does slot-1
  const depExistsArgs = { tokenId: 2, amount: 10, customId: 0 };
  const depExistsIx = await instrs.buildDepositInstruction(ctx, depExistsArgs, true, rpcGetSlot);
  const depFirstIx = await instrs.buildDepositInstruction(ctx, depExistsArgs, false, rpcGetSlot);
  // Perp DATA-BYTES goldens (M1a table: data-bytes only; client use M1b).
  const newPerpIx = await perp.buildNewPerpOrderInstruction(
    ctx, { side: 0, instrId: 0, price: 152.25, qty: 0.033, leverage: 5, ioc: 0, orderType: 0, edgePrice: 0 }, instr);
  const perpCancelIx = await perp.buildPerpOrderCancelInstruction(
    ctx, { side: 0, instrId: 0, orderId: 987654321012345 }, instr);
  const perpMassIx = await perp.buildPerpMassCancelInstruction(ctx, { instrId: 0 }, instr);

  // ── Signed-tx goldens (spec §2.3) for the three M1a-used instructions ──
  const CB_PROGRAM = kit.address('ComputeBudget111111111111111111111111111111');
  const CU_LIMIT = 400000;
  const CU_PRICE = 1000;
  const cuLimitIx = { programAddress: CB_PROGRAM, accounts: [], data: new Uint8Array([2, ...u32le(CU_LIMIT)]) };
  const cuPriceIx = { programAddress: CB_PROGRAM, accounts: [], data: new Uint8Array([3, ...u64le(CU_PRICE)]) };

  // Synthetic LUT holding the non-signer venue accounts the spot ixs touch.
  const spotContextAddrs = [
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.INSTR }),
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_BIDS_TREE }),
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_ASKS_TREE }),
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_BID_ORDERS }),
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_ASK_ORDERS }),
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_LINES }),
    instrHeader.mapsAddress,
    await helpers.getInstrAccountByTag(helperCtx, { assetTokenId: 1, crncyTokenId: 2, tag: AccountType.SPOT_CLIENT_INFOS }),
  ];
  const lutAddress = addrFromByte(40);
  const lutMap = { [lutAddress]: spotContextAddrs };

  async function signedGolden(venueIx, useLut) {
    let message = kit.pipe(
      kit.createTransactionMessage({ version: 0 }),
      (m) => kit.setTransactionMessageLifetimeUsingBlockhash({ blockhash, lastValidBlockHeight }, m),
      (m) => kit.setTransactionMessageFeePayerSigner(signer, m),
    );
    for (const ix of [cuLimitIx, cuPriceIx, venueIx]) {
      message = kit.appendTransactionMessageInstruction(ix, message);
    }
    if (useLut) {
      message = kit.compressTransactionMessageUsingAddressLookupTables(message, lutMap);
    }
    const compiled = kit.compileTransaction(message);
    const signed = await kit.signTransactionMessageWithSigners(message);
    return {
      message_b64: Buffer.from(compiled.messageBytes).toString('base64'),
      tx_b64: kit.getBase64EncodedWireTransaction(signed),
      signature_b58: kit.getSignatureFromTransaction(signed),
      used_lut: useLut,
    };
  }

  const out = {
    kit_version: kitPkg.version,
    drv_models_tag: 'v0.2.74',
    program_id: PROGRAM_ID,
    version: VERSION,
    signer_seed_byte: 7,
    signer_pubkey: signer.address,
    blockhash,
    last_valid_block_height: Number(lastValidBlockHeight),
    compute_unit_limit: CU_LIMIT,
    compute_unit_price_microlamports: CU_PRICE,
    pda: {
      drvs_authority: drvsAuthority,
      root_account: rootAccount,
      community_account: communityAccount,
      client_primary: clientPrimaryAccount,
      client_community: clientCommunityAccount,
      program_token_account_usdc: programTokenAccountUsdc,
      ata_usdc: ataUsdc,
      lut_slot: LUT_SLOT,
      lut_address_for_slot: lutForSlot,
    },
    directory: {
      instr: instrHeader,
      tokens: [...tokens.values()].map((t) => ({
        id: t.id, mint: t.address, vault: t.programAddress, mask: t.mask,
      })),
    },
    lut: { address: lutAddress, addresses: spotContextAddrs },
    instructions: {
      new_spot_order: { args: newSpotArgs, ...dumpIx(newSpotIx) },
      new_spot_order_ioc: { args: newSpotIocArgs, ...dumpIx(newSpotIocIx) },
      new_spot_order_mixed: { args: newSpotMixedArgs, ...dumpIx(newSpotMixedIx) },
      spot_order_cancel: { args: cancelArgs, ...dumpIx(cancelIx) },
      deposit_exists: { args: depExistsArgs, ...dumpIx(depExistsIx) },
      deposit_first: { args: depExistsArgs, ...dumpIx(depFirstIx) },
      new_perp_order: { data_hex: hex(newPerpIx.data) },
      perp_order_cancel: { data_hex: hex(perpCancelIx.data) },
      perp_mass_cancel: { data_hex: hex(perpMassIx.data) },
    },
    signed_txs: {
      new_spot_order: await signedGolden(newSpotIx, true),
      spot_order_cancel: await signedGolden(cancelIx, true),
      deposit_first: await signedGolden(depFirstIx, false),
    },
  };
  process.stdout.write(JSON.stringify(out, null, 2));
}

main().catch((e) => { console.error(e); process.exit(1); });
