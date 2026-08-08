#!/usr/bin/env node
// One-shot: decode the captured instrument bytes with @deriverse/kit's own
// models (InstrAccountHeaderModel + LineQuotesModel — the reference decode,
// kit engine/index.ts ~815-860) and dump raw + scaled TOB values.
// Usage: node dump_kit_decode.cjs <kit-clone-root> <instr0_mainnet.bin> > instr0_kit_decode.json
'use strict';
const fs = require('fs');
const path = require('path');

const kitRoot = process.argv[2];
const binPath = process.argv[3];
if (!kitRoot || !binPath) {
  console.error('usage: node dump_kit_decode.cjs <kit-clone-root> <instr0_mainnet.bin>');
  process.exit(1);
}
const req = (p) => require(path.join(kitRoot, p));
const { InstrAccountHeaderModel, LineQuotesModel } = req('dist/structure_models.js');
const { MARKET_DEPTH, DF } = req('dist/constants.js');

const raw = fs.readFileSync(binPath);
const asB64Response = [raw.toString('base64'), 'base64'];
const header = InstrAccountHeaderModel.fromBuffer(asB64Response);

function ladder(k) {
  const out = [];
  for (let i = 0; i < MARKET_DEPTH; ++i) {
    const offset = InstrAccountHeaderModel.LENGTH + i * 16 + 16 * MARKET_DEPTH * k;
    const line = LineQuotesModel.fromBuffer(asB64Response, offset);
    if (line.px === 0) break;
    out.push({ px_raw: line.px, qty_raw: line.qty });
  }
  return out;
}

const spotBids = ladder(0);
const spotAsks = ladder(1);
const assetDec = Math.pow(10, header.assetTokenDecsCount);
process.stdout.write(JSON.stringify({
  kit_version: req('package.json').version,
  header: {
    instr_id: header.instrId,
    asset_token_id: header.assetTokenId,
    crncy_token_id: header.crncyTokenId,
    asset_token_decs_count: header.assetTokenDecsCount,
    crncy_token_decs_count: header.crncyTokenDecsCount,
    best_bid_raw: header.bestBid,
    best_ask_raw: header.bestAsk,
  },
  spot_bids: spotBids,
  spot_asks: spotAsks,
  scaled_tob: {
    bid_px: spotBids[0].px_raw / DF,
    ask_px: spotAsks[0].px_raw / DF,
    bid_qty: spotBids[0].qty_raw / assetDec,
    ask_qty: spotAsks[0].qty_raw / assetDec,
  },
}, null, 2));
