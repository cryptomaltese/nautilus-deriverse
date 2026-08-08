//! Instruction builders (port of kit `spot-instructions.ts` /
//! `instructions.ts` account-meta tables, referral/private-mode branches
//! OFF) + hand-rolled v0 message compile/sign (reference kit-example
//! `lib/transactions.ts`; ordering semantics ported from @solana/kit's
//! compiler and BYTE-GATED by tests/golden_signed_tx.rs — if the goldens
//! disagree, the goldens win and this compiler changes).

use std::str::FromStr;

use anyhow::Context;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use solana_pubkey::Pubkey;

use drv_models::instruction_data::{
    DepositData, NewPerpOrderData, NewSpotOrderData, PerpMassCancelData, PerpOrderCancelData,
    SpotOrderCancelData,
};
use drv_models::new_types::instrument::InstrId;
// Plan-drift note (recorded): at pinned tag v0.2.74 AccountType lives under
// the `account_type` submodule (types.rs:77/:118); CappedI64 is top-level.
use drv_models::state::types::account_type::AccountType;
use drv_models::state::types::CappedI64;

use crate::common::consts::{
    ADDRESS_LOOKUP_TABLE_PROGRAM, COMPUTE_BUDGET_PROGRAM, DF, SYSTEM_PROGRAM, TOKEN_2022_PROGRAM,
    TOKEN_PROGRAM,
};
use crate::tx::accounts::{
    associated_token_address, account_by_tag, instr_account_by_tag, lookup_table_address,
    program_token_account, VenueContext,
};

// ── Wire scaling (kit: Math.round(price*DF), Math.round(qty*10^decs)) ──

#[must_use]
pub fn price_to_raw(px: f64) -> i64 {
    (px * DF as f64).round() as i64
}

#[must_use]
pub fn qty_to_raw(qty: f64, decs: u32) -> i64 {
    (qty * 10f64.powi(decs as i32)).round() as i64
}

// ── Instruction model ──

#[derive(Clone, Debug, PartialEq)]
pub struct AccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl AccountMeta {
    fn ro(pubkey: Pubkey) -> Self {
        Self { pubkey, is_signer: false, is_writable: false }
    }
    fn w(pubkey: Pubkey) -> Self {
        Self { pubkey, is_signer: false, is_writable: true }
    }
    fn ro_signer(pubkey: Pubkey) -> Self {
        Self { pubkey, is_signer: true, is_writable: false }
    }
}

#[derive(Clone, Debug)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ResolvedLut {
    pub address: Pubkey,
    pub addresses: Vec<Pubkey>,
}

#[derive(Clone, Debug)]
pub struct NewSpotOrderArgs {
    pub instr_id: u32,
    pub side: u8,
    pub ioc: u8,
    pub order_type: u8,
    pub price: f64,
    pub qty: f64,
    pub edge_price: f64,
}

#[derive(Clone, Debug)]
pub struct SpotOrderCancelArgs {
    pub instr_id: u32,
    pub side: u8,
    pub order_id: i64,
}

#[derive(Clone, Debug)]
pub struct DepositArgs {
    pub token_id: u32,
    pub amount: f64,
    pub deposit_all: bool,
    /// Client primary account already exists? First deposit (false) also
    /// creates client primary/community + the client LUT (kit
    /// `instructions.ts::buildDepositInstruction` exists=false branch).
    pub exists: bool,
    /// Only read when `exists == false` (the builder passes slot-1 already
    /// applied — callers hand `get_slot()? - 1`).
    pub lut_slot: u32,
    pub custom_id: i64,
}

pub struct TxBuilder<'a> {
    pub ctx: &'a VenueContext,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

impl TxBuilder<'_> {
    fn instr(&self, instr_id: u32) -> anyhow::Result<&crate::tx::accounts::InstrMeta> {
        self.ctx
            .dir
            .instruments
            .get(&instr_id)
            .with_context(|| format!("unknown instr_id {instr_id}"))
    }

    fn spot_ctx_metas(&self, instr_id: u32) -> anyhow::Result<Vec<AccountMeta>> {
        // kit context-builders.ts::getSpotContext (8 writable accounts).
        let m = self.instr(instr_id)?;
        let d = &self.ctx.dir;
        let by_tag = |tag: AccountType| {
            instr_account_by_tag(
                &d.program_id, d.version, tag as u32, m.asset_token_id, m.crncy_token_id,
                &d.drvs_authority,
            )
        };
        Ok(vec![
            AccountMeta::w(by_tag(AccountType::Instr)),
            AccountMeta::w(by_tag(AccountType::SpotBidsTree)),
            AccountMeta::w(by_tag(AccountType::SpotAsksTree)),
            AccountMeta::w(by_tag(AccountType::SpotBidOrders)),
            AccountMeta::w(by_tag(AccountType::SpotAskOrders)),
            AccountMeta::w(by_tag(AccountType::SpotLines)),
            AccountMeta::w(m.maps_address),
            AccountMeta::w(by_tag(AccountType::SpotClientInfos)),
        ])
    }

    fn spot_one_sided_ctx_metas(&self, instr_id: u32, side: u8) -> anyhow::Result<Vec<AccountMeta>> {
        // kit context-builders.ts::getSpotOneSidedContext (6 writable accounts).
        let m = self.instr(instr_id)?;
        let d = &self.ctx.dir;
        let by_tag = |tag: AccountType| {
            instr_account_by_tag(
                &d.program_id, d.version, tag as u32, m.asset_token_id, m.crncy_token_id,
                &d.drvs_authority,
            )
        };
        let (tree, orders) = if side == 0 {
            (AccountType::SpotBidsTree, AccountType::SpotBidOrders)
        } else {
            (AccountType::SpotAsksTree, AccountType::SpotAskOrders)
        };
        Ok(vec![
            AccountMeta::w(by_tag(AccountType::Instr)),
            AccountMeta::w(by_tag(tree)),
            AccountMeta::w(by_tag(orders)),
            AccountMeta::w(by_tag(AccountType::SpotLines)),
            AccountMeta::w(m.maps_address),
            AccountMeta::w(by_tag(AccountType::SpotClientInfos)),
        ])
    }

    /// kit spot-instructions.ts::buildNewSpotOrderInstruction (no referral).
    pub fn new_spot_order_ix(&self, a: &NewSpotOrderArgs) -> anyhow::Result<Instruction> {
        let m = self.instr(a.instr_id)?;
        let d = &self.ctx.dir;
        let data = NewSpotOrderData {
            tag: 12,
            ioc: a.ioc,
            order_type: a.order_type,
            side: a.side,
            instr_id: InstrId(a.instr_id),
            price: price_to_raw(a.price),
            amount: CappedI64 { value: qty_to_raw(a.qty, m.asset_decs) },
            edge_price: price_to_raw(a.edge_price),
        };
        let mut accounts = vec![
            AccountMeta::ro_signer(self.ctx.signer),
            AccountMeta::ro(d.root_account),
            AccountMeta::w(self.ctx.client_primary),
            AccountMeta::w(self.ctx.client_community),
        ];
        accounts.extend(self.spot_ctx_metas(a.instr_id)?);
        // Community: writable only for the DRVS instrument (asset id 0).
        let community = if m.asset_token_id == 0 {
            AccountMeta::w(d.community_account)
        } else {
            AccountMeta::ro(d.community_account)
        };
        accounts.push(community);
        accounts.push(AccountMeta::ro(Pubkey::from_str(SYSTEM_PROGRAM)?));
        Ok(Instruction {
            program_id: d.program_id,
            accounts,
            data: bytemuck::bytes_of(&data).to_vec(),
        })
    }

    /// kit spot-instructions.ts::buildSpotOrderCancelInstruction.
    pub fn spot_order_cancel_ix(&self, a: &SpotOrderCancelArgs) -> anyhow::Result<Instruction> {
        let m = self.instr(a.instr_id)?;
        let d = &self.ctx.dir;
        let data = SpotOrderCancelData {
            tag: 13,
            side: a.side,
            padding_u16: 0,
            instr_id: InstrId(a.instr_id),
            order_id: a.order_id,
        };
        let mut accounts = vec![
            AccountMeta::ro_signer(self.ctx.signer),
            AccountMeta::ro(d.root_account),
            AccountMeta::w(self.ctx.client_primary),
        ];
        accounts.extend(self.spot_one_sided_ctx_metas(a.instr_id, a.side)?);
        accounts.push(AccountMeta::ro(Pubkey::from_str(SYSTEM_PROGRAM)?));
        if m.asset_token_id == 0 {
            accounts.push(AccountMeta::w(d.community_account));
            accounts.push(AccountMeta::w(self.ctx.client_community));
        }
        Ok(Instruction {
            program_id: d.program_id,
            accounts,
            data: bytemuck::bytes_of(&data).to_vec(),
        })
    }

    /// kit instructions.ts::buildDepositInstruction (privateMode off,
    /// referral off, token_id != 0 — the M1a USDC path).
    pub fn deposit_ix(&self, a: &DepositArgs) -> anyhow::Result<Instruction> {
        let d = &self.ctx.dir;
        let token = d
            .tokens
            .get(&a.token_id)
            .with_context(|| format!("token {} not in registry", a.token_id))?;
        anyhow::ensure!(a.token_id != 0, "DRVS (token 0) deposit path is out of M1a scope");
        let token_program =
            Pubkey::from_str(if token.token2022 { TOKEN_2022_PROGRAM } else { TOKEN_PROGRAM })?;
        let client_ata = associated_token_address(&self.ctx.signer, &token_program, &token.mint);
        let mut accounts = vec![
            AccountMeta::ro_signer(self.ctx.signer),
            AccountMeta::w(client_ata),
            AccountMeta::w(token.vault),
            AccountMeta::ro(token.mint),
            if a.exists { AccountMeta::ro(d.root_account) } else { AccountMeta::w(d.root_account) },
            AccountMeta::ro(program_token_account(&d.program_id, d.version, &token.mint, &d.drvs_authority)),
            AccountMeta::w(self.ctx.client_primary),
            AccountMeta::ro(Pubkey::from_str(SYSTEM_PROGRAM)?),
            AccountMeta::ro(token_program),
        ];
        let (lut_slot, ref_id) = if a.exists {
            (0u32, 0u32)
        } else {
            accounts.push(AccountMeta::w(self.ctx.client_community));
            accounts.push(AccountMeta::w(lookup_table_address(&self.ctx.signer, u64::from(a.lut_slot))));
            accounts.push(AccountMeta::w(Pubkey::from_str(ADDRESS_LOOKUP_TABLE_PROGRAM)?));
            (a.lut_slot, 0u32)
        };
        let data = DepositData {
            tag: 7,
            competition_id: 0,
            deposit_all: u8::from(a.deposit_all),
            padding_u8: 0,
            token_id: a.token_id,
            amount: CappedI64 { value: qty_to_raw(a.amount, token.decimals) },
            lut_slot,
            ref_id,
            custom_id: a.custom_id,
        };
        Ok(Instruction {
            program_id: d.program_id,
            accounts,
            data: bytemuck::bytes_of(&data).to_vec(),
        })
    }

    /// Perp DATA-BYTES encoders (M1a acceptance 2; metas + client use = M1b).
    pub fn new_perp_order_data(
        &self,
        instr_id: u32,
        side: u8,
        ioc: u8,
        leverage: u8,
        order_type: u8,
        price: f64,
        qty: f64,
        edge_price: f64,
    ) -> anyhow::Result<Vec<u8>> {
        let m = self.instr(instr_id)?;
        let data = NewPerpOrderData {
            tag: 19,
            ioc,
            leverage,
            order_type,
            side,
            padding_u8: 0,
            padding_u16: 0,
            padding_u32: 0,
            instr_id: InstrId(instr_id),
            price: price_to_raw(price),
            amount: CappedI64 { value: qty_to_raw(qty, m.asset_decs) },
            edge_price: price_to_raw(edge_price),
        };
        Ok(bytemuck::bytes_of(&data).to_vec())
    }

    #[must_use]
    pub fn perp_order_cancel_data(&self, instr_id: u32, side: u8, order_id: i64) -> Vec<u8> {
        let data = PerpOrderCancelData {
            tag: 30,
            side,
            padding_u16: 0,
            instr_id: InstrId(instr_id),
            order_id,
        };
        bytemuck::bytes_of(&data).to_vec()
    }

    #[must_use]
    pub fn perp_mass_cancel_data(&self, instr_id: u32) -> Vec<u8> {
        let data = PerpMassCancelData {
            tag: 36,
            padding_u8: 0,
            padding_u16: 0,
            instr_id: InstrId(instr_id),
        };
        bytemuck::bytes_of(&data).to_vec()
    }

    fn compute_budget_ixs(&self) -> anyhow::Result<[Instruction; 2]> {
        let program_id = Pubkey::from_str(COMPUTE_BUDGET_PROGRAM)?;
        let mut limit = vec![2u8]; // SetComputeUnitLimit
        limit.extend_from_slice(&self.compute_unit_limit.to_le_bytes());
        let mut price = vec![3u8]; // SetComputeUnitPrice
        price.extend_from_slice(&self.compute_unit_price_microlamports.to_le_bytes());
        Ok([
            Instruction { program_id, accounts: vec![], data: limit },
            Instruction { program_id, accounts: vec![], data: price },
        ])
    }

    /// §1.6 steps 1–5: prepend ComputeBudget, compile v0 (+LUT compression),
    /// ed25519-sign. Pure up to signing — the §2.3 testable seam.
    pub fn build_signed_tx(
        &self,
        venue_ixs: &[Instruction],
        signing_key: &SigningKey,
        blockhash: &str,
        luts: &[ResolvedLut],
    ) -> anyhow::Result<VersionedTransaction> {
        let [cu_limit, cu_price] = self.compute_budget_ixs()?;
        let mut ixs = vec![cu_limit, cu_price];
        ixs.extend(venue_ixs.iter().cloned());
        let fee_payer = Pubkey::new_from_array(signing_key.verifying_key().to_bytes());
        anyhow::ensure!(
            fee_payer == self.ctx.signer,
            "signing key pubkey != context signer (fee payer = hot wallet, §1.6)"
        );
        let blockhash32: [u8; 32] = bs58::decode(blockhash)
            .into_vec()
            .context("blockhash base58")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("blockhash not 32 bytes"))?;
        let message = compile_v0_message(fee_payer, &ixs, blockhash32, luts)?;
        let signature = signing_key.sign(&message).to_bytes();
        Ok(VersionedTransaction { signatures: vec![signature], message })
    }
}

#[derive(Clone, Debug)]
pub struct VersionedTransaction {
    pub signatures: Vec<[u8; 64]>,
    pub message: Vec<u8>,
}

impl VersionedTransaction {
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.signatures.len() * 64 + self.message.len());
        shortvec(self.signatures.len(), &mut out);
        for sig in &self.signatures {
            out.extend_from_slice(sig);
        }
        out.extend_from_slice(&self.message);
        out
    }

    #[must_use]
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.serialize())
    }

    /// The transaction id = first signature, base58.
    #[must_use]
    pub fn signature_base58(&self) -> String {
        bs58::encode(self.signatures[0]).into_string()
    }
}

/// Solana compact-u16 length prefix.
fn shortvec(n: usize, out: &mut Vec<u8>) {
    let mut v = n as u32;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// @solana/kit's address comparator (`addresses::getAddressComparator` =
/// `Intl.Collator("en", { caseFirst: "lower", sensitivity: "variant" })`)
/// ported for base58 strings: digits sort before letters (numeric order),
/// letters compare case-insensitively at the primary level, and case breaks
/// ties with LOWERCASE first. Shorter prefix sorts first.
fn kit_address_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn primary(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            _ => 10 + (c.to_ascii_lowercase() - b'a'),
        }
    }
    a.bytes()
        .map(primary)
        .cmp(b.bytes().map(primary))
        .then_with(|| {
            a.bytes()
                .map(|c| u8::from(c.is_ascii_uppercase()))
                .cmp(b.bytes().map(|c| u8::from(c.is_ascii_uppercase())))
        })
}

fn cmp_keys(a: &Pubkey, b: &Pubkey) -> std::cmp::Ordering {
    kit_address_cmp(&a.to_string(), &b.to_string())
}

/// v0 message compiler, ported to match @solana/kit's compiled output
/// (byte-gated by tests/golden_signed_tx.rs; ordering semantics from
/// `@solana/transaction-messages` `getOrderedAccountsFromAddressMap`):
/// - fee payer first, then static keys before LUT-resolved keys;
/// - within statics: writable signers, readonly signers, writable
///   non-signers, readonly non-signers — each class sorted by the kit
///   address comparator (base58 collation, NOT first-appearance);
/// - invoked program ids are readonly non-signers and NEVER moved to a LUT;
/// - LUT compression moves matching non-signer keys into per-table
///   writable/readonly index lists; tables sorted by the comparator, index
///   lists sorted by the member ADDRESS (not table position);
/// - runtime index space: [static keys] ++ [all LUT writables, table order]
///   ++ [all LUT readonlys, table order].
fn compile_v0_message(
    fee_payer: Pubkey,
    ixs: &[Instruction],
    blockhash: [u8; 32],
    luts: &[ResolvedLut],
) -> anyhow::Result<Vec<u8>> {
    #[derive(Default, Clone, Copy)]
    struct Role {
        signer: bool,
        writable: bool,
        invoked: bool,
    }
    let mut roles: std::collections::HashMap<Pubkey, Role> = std::collections::HashMap::new();
    let mut upsert = |key: Pubkey, signer: bool, writable: bool, invoked: bool| {
        let entry = roles.entry(key).or_default();
        entry.signer |= signer;
        entry.writable |= writable;
        entry.invoked |= invoked;
    };
    upsert(fee_payer, true, true, false);
    for ix in ixs {
        upsert(ix.program_id, false, false, true);
        for meta in &ix.accounts {
            upsert(meta.pubkey, meta.is_signer, meta.is_writable, false);
        }
    }

    let class_of = |r: &Role| -> u8 {
        match (r.signer, r.writable) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        }
    };
    let mut classes: [Vec<Pubkey>; 4] = Default::default();
    for (key, role) in &roles {
        if *key == fee_payer {
            continue;
        }
        classes[class_of(role) as usize].push(*key);
    }
    for class in &mut classes {
        class.sort_by(cmp_keys);
    }

    // Tables in comparator order (kit sorts lookups by table address).
    let mut lut_order: Vec<usize> = (0..luts.len()).collect();
    lut_order.sort_by(|x, y| cmp_keys(&luts[*x].address, &luts[*y].address));
    // LUT extraction from the two non-signer classes.
    let mut lookups: Vec<(Pubkey, Vec<Pubkey>, Vec<Pubkey>)> = lut_order
        .iter()
        .map(|&i| (luts[i].address, Vec::new(), Vec::new()))
        .collect();
    let in_lut = |key: &Pubkey| -> Option<usize> {
        // Comparator-first table wins when a key is in several (kit
        // shouldReplaceEntry rule); lut_order is already comparator-sorted.
        lut_order
            .iter()
            .position(|&i| luts[i].addresses.contains(key))
    };
    let mut static_keys = vec![fee_payer];
    static_keys.extend(classes[0].iter().copied());
    static_keys.extend(classes[1].iter().copied());
    let mut static_wn = Vec::new();
    for key in &classes[2] {
        match in_lut(key) {
            Some(t) if !roles[key].invoked => lookups[t].1.push(*key),
            _ => static_wn.push(*key),
        }
    }
    let mut static_rn = Vec::new();
    for key in &classes[3] {
        match in_lut(key) {
            Some(t) if !roles[key].invoked => lookups[t].2.push(*key),
            _ => static_rn.push(*key),
        }
    }
    static_keys.extend(static_wn.iter().copied());
    static_keys.extend(static_rn.iter().copied());

    let num_signers = 1 + classes[0].len() + classes[1].len();
    let num_readonly_signed = classes[1].len();
    let num_readonly_unsigned = static_rn.len();

    // Runtime index map.
    let mut index_of: std::collections::HashMap<Pubkey, u8> = std::collections::HashMap::new();
    for (i, key) in static_keys.iter().enumerate() {
        index_of.insert(*key, u8::try_from(i).context("too many static keys")?);
    }
    let mut next = static_keys.len();
    for (_, writables, _) in &lookups {
        for key in writables {
            index_of.insert(*key, u8::try_from(next).context("too many keys")?);
            next += 1;
        }
    }
    for (_, _, readonlys) in &lookups {
        for key in readonlys {
            index_of.insert(*key, u8::try_from(next).context("too many keys")?);
            next += 1;
        }
    }

    // Serialize.
    let mut out = vec![0x80u8]; // v0 prefix
    out.push(u8::try_from(num_signers).context("signers")?);
    out.push(u8::try_from(num_readonly_signed).context("ro signed")?);
    out.push(u8::try_from(num_readonly_unsigned).context("ro unsigned")?);
    shortvec(static_keys.len(), &mut out);
    for key in &static_keys {
        out.extend_from_slice(key.as_ref());
    }
    out.extend_from_slice(&blockhash);
    shortvec(ixs.len(), &mut out);
    for ix in ixs {
        out.push(index_of[&ix.program_id]);
        shortvec(ix.accounts.len(), &mut out);
        for meta in &ix.accounts {
            out.push(index_of[&meta.pubkey]);
        }
        shortvec(ix.data.len(), &mut out);
        out.extend_from_slice(&ix.data);
    }
    let used_lookups: Vec<_> = lookups
        .iter()
        .filter(|(_, w, r)| !w.is_empty() || !r.is_empty())
        .collect();
    shortvec(used_lookups.len(), &mut out);
    for (table, writables, readonlys) in used_lookups {
        out.extend_from_slice(table.as_ref());
        shortvec(writables.len(), &mut out);
        for key in writables {
            let pos = luts
                .iter()
                .find(|l| l.address == *table)
                .and_then(|l| l.addresses.iter().position(|a| a == key))
                .context("lut position")?;
            out.push(u8::try_from(pos).context("lut index")?);
        }
        shortvec(readonlys.len(), &mut out);
        for key in readonlys {
            let pos = luts
                .iter()
                .find(|l| l.address == *table)
                .and_then(|l| l.addresses.iter().position(|a| a == key))
                .context("lut position")?;
            out.push(u8::try_from(pos).context("lut index")?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortvec_matches_solana_compact_u16() {
        let mut out = Vec::new();
        shortvec(0, &mut out);
        assert_eq!(out, [0x00]);
        out.clear();
        shortvec(127, &mut out);
        assert_eq!(out, [0x7f]);
        out.clear();
        shortvec(128, &mut out);
        assert_eq!(out, [0x80, 0x01]);
        out.clear();
        shortvec(300, &mut out);
        assert_eq!(out, [0xac, 0x02]);
    }

    #[test]
    fn scaling_rounds_like_kit() {
        assert_eq!(price_to_raw(152.25), 152_250_000_000);
        assert_eq!(qty_to_raw(0.033, 9), 33_000_000);
        assert_eq!(qty_to_raw(10.0, 6), 10_000_000);
    }
}
