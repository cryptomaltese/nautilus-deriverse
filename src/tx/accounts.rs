//! PDA derivations + venue directory (port of kit `account-helpers.ts` +
//! `engine/utils.ts`).
//!
//! TRANSPOSITION TRAP (spec §1.4): PDA seed buffers are
//! [u32-LE VERSION, u32-LE TAG] — version FIRST — while the on-wire account
//! discriminator (and the getProgramAccounts memcmp) is
//! [u32-LE TAG, u32-LE VERSION]. Both orders appear below, each with its
//! kit line cited; the PDA goldens (fixture) pin the seed order, the live
//! discovery pins the memcmp order.

use std::collections::BTreeMap;
use std::str::FromStr;

use solana_pubkey::Pubkey;

use crate::common::consts::{
    ADDRESS_LOOKUP_TABLE_PROGRAM, ASSOCIATED_TOKEN_PROGRAM, DRVS_AUTHORITY_SEED, INSTR_SNAPSHOT_LEN,
};
use crate::decode::instr::decode_instr_account;
use crate::decode::token::{decode_token_state, token_decimals, token_is_2022};
use crate::rpc::SolanaRpc;
// Plan-drift note (recorded): at pinned tag v0.2.74 the enum lives under the
// `account_type` submodule, not directly in `types` (types.rs:77/:118).
use drv_models::state::types::account_type::AccountType;

/// kit `engine/index.ts:521` — seeds ['ndxnt'].
#[must_use]
pub fn derive_drvs_authority(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[DRVS_AUTHORITY_SEED], program_id).0
}

/// kit `account-helpers.ts::getAccountByTag` — seeds [version_le ++ tag_le,
/// drvs_authority].
#[must_use]
pub fn account_by_tag(
    program_id: &Pubkey,
    version: u32,
    tag: u32,
    drvs_authority: &Pubkey,
) -> Pubkey {
    let mut buf = [0u8; 8];
    buf[..4].copy_from_slice(&version.to_le_bytes());
    buf[4..].copy_from_slice(&tag.to_le_bytes());
    Pubkey::find_program_address(&[&buf, drvs_authority.as_ref()], program_id).0
}

/// kit `account-helpers.ts::getInstrAccountByTag` — seeds
/// [version_le ++ tag_le ++ asset_le ++ crncy_le, drvs_authority].
#[must_use]
pub fn instr_account_by_tag(
    program_id: &Pubkey,
    version: u32,
    tag: u32,
    asset_token_id: u32,
    crncy_token_id: u32,
    drvs_authority: &Pubkey,
) -> Pubkey {
    let mut buf = [0u8; 16];
    buf[..4].copy_from_slice(&version.to_le_bytes());
    buf[4..8].copy_from_slice(&tag.to_le_bytes());
    buf[8..12].copy_from_slice(&asset_token_id.to_le_bytes());
    buf[12..].copy_from_slice(&crncy_token_id.to_le_bytes());
    Pubkey::find_program_address(&[&buf, drvs_authority.as_ref()], program_id).0
}

/// kit `account-helpers.ts:174–180` (findClientPrimaryAccount /
/// findClientCommunityAccount) — seeds [version_le ++ tag_le, SIGNER]
/// (signer, not drvs_authority).
#[must_use]
pub fn client_account(program_id: &Pubkey, version: u32, tag: u32, signer: &Pubkey) -> Pubkey {
    let mut buf = [0u8; 8];
    buf[..4].copy_from_slice(&version.to_le_bytes());
    buf[4..].copy_from_slice(&tag.to_le_bytes());
    Pubkey::find_program_address(&[&buf, signer.as_ref()], program_id).0
}

/// kit `account-helpers.ts::getTokenAccount` — seed = the 32 mint bytes with
/// the version written LE at offset 28, plus drvs_authority.
#[must_use]
pub fn program_token_account(
    program_id: &Pubkey,
    version: u32,
    mint: &Pubkey,
    drvs_authority: &Pubkey,
) -> Pubkey {
    let mut seed = mint.to_bytes();
    seed[28..32].copy_from_slice(&version.to_le_bytes());
    Pubkey::find_program_address(&[&seed, drvs_authority.as_ref()], program_id).0
}

/// kit `engine/utils.ts::findAssociatedTokenAddress`.
#[must_use]
pub fn associated_token_address(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM).expect("ata program id");
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .0
}

/// kit `engine/utils.ts::getLookupTableAddress` — seeds [authority,
/// slot i64 LE] on the Address Lookup Table program.
#[must_use]
pub fn lookup_table_address(authority: &Pubkey, slot: u64) -> Pubkey {
    let alt_program = Pubkey::from_str(ADDRESS_LOOKUP_TABLE_PROGRAM).expect("alt program id");
    Pubkey::find_program_address(&[authority.as_ref(), &slot.to_le_bytes()], &alt_program).0
}

/// getProgramAccounts memcmp bytes at offset 0: TAG then VERSION (kit
/// `account-helpers.ts::findAccountsByTag` — the discriminator order, i.e.
/// the transpose of the PDA seed order).
#[must_use]
pub fn discriminator_memcmp_base58(tag: u32, version: u32) -> String {
    let mut buf = [0u8; 8];
    buf[..4].copy_from_slice(&tag.to_le_bytes());
    buf[4..].copy_from_slice(&version.to_le_bytes());
    bs58::encode(buf).into_string()
}

#[derive(Clone, Debug)]
pub struct VenueToken {
    pub id: u32,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub decimals: u32,
    pub token2022: bool,
}

#[derive(Clone, Debug)]
pub struct InstrMeta {
    pub instr_id: u32,
    pub account: Pubkey,
    pub asset_token_id: u32,
    pub crncy_token_id: u32,
    pub asset_decs: u32,
    pub crncy_decs: u32,
    pub maps_address: Pubkey,
    pub lut_address: Pubkey,
}

/// Signer-independent venue facts (data client + bins share this).
#[derive(Clone, Debug)]
pub struct VenueDirectory {
    pub program_id: Pubkey,
    pub version: u32,
    pub drvs_authority: Pubkey,
    pub root_account: Pubkey,
    pub community_account: Pubkey,
    pub tokens: BTreeMap<u32, VenueToken>,
    pub instruments: BTreeMap<u32, InstrMeta>,
}

/// Live discovery: token registry (tag 4) + instrument headers (tag 7,
/// dataSlice = the 2344-byte snapshot region).
pub async fn resolve_directory<R: SolanaRpc + ?Sized>(
    rpc: &R,
    program_id: &Pubkey,
    version: u32,
) -> anyhow::Result<VenueDirectory> {
    let drvs_authority = derive_drvs_authority(program_id);
    let root_account = account_by_tag(
        program_id,
        version,
        AccountType::Root as u32,
        &drvs_authority,
    );
    let community_account = account_by_tag(
        program_id,
        version,
        AccountType::Community as u32,
        &drvs_authority,
    );

    let mut tokens = BTreeMap::new();
    for (_pubkey, bytes) in rpc
        .get_program_accounts(
            &discriminator_memcmp_base58(AccountType::Token as u32, version),
            None,
        )
        .await?
    {
        let t = decode_token_state(&bytes, version)?;
        // Mint + vault are Pubkey fields on the drv-models struct; read them
        // wire-level (offsets 8 / 40, kit TokenStateModel) for the same
        // field-visibility robustness as decode/token.rs.
        let raw = bytemuck::bytes_of(&t);
        let mint = Pubkey::new_from_array(raw[8..40].try_into()?);
        let vault = Pubkey::new_from_array(raw[40..72].try_into()?);
        let id = u32::from_le_bytes(raw[72..76].try_into()?);
        tokens.insert(
            id,
            VenueToken {
                id,
                mint,
                vault,
                decimals: token_decimals(&t),
                token2022: token_is_2022(&t),
            },
        );
    }
    anyhow::ensure!(!tokens.is_empty(), "venue token registry came back empty");

    let mut instruments = BTreeMap::new();
    for (pubkey, bytes) in rpc
        .get_program_accounts(
            &discriminator_memcmp_base58(AccountType::Instr as u32, version),
            Some((0, INSTR_SNAPSHOT_LEN as u64)),
        )
        .await?
    {
        let snap = decode_instr_account(&bytes, 0, version)?;
        let h = &snap.header;
        // instr_id / token ids / decs / addresses: named fields on the Pod
        // header (drv-models instrument.rs).
        let instr_id = u32::from_le_bytes(bytemuck::bytes_of(&h.instr_id)[..4].try_into()?);
        let asset_decs = h.asset_token_decs_count;
        let crncy_decs = h.crncy_token_decs_count;
        let maps_address = Pubkey::new_from_array(h.maps_address.to_bytes());
        let lut_address = Pubkey::new_from_array(h.lut_address.to_bytes());
        instruments.insert(
            instr_id,
            InstrMeta {
                instr_id,
                account: Pubkey::from_str(&pubkey)?,
                asset_token_id: h.asset_token_id,
                crncy_token_id: h.crncy_token_id,
                asset_decs,
                crncy_decs,
                maps_address,
                lut_address,
            },
        );
    }
    anyhow::ensure!(
        !instruments.is_empty(),
        "venue instrument list came back empty"
    );

    Ok(VenueDirectory {
        program_id: *program_id,
        version,
        drvs_authority,
        root_account,
        community_account,
        tokens,
        instruments,
    })
}

/// Directory + the signer's client PDAs (the tx-build context).
#[derive(Clone, Debug)]
pub struct VenueContext {
    pub dir: VenueDirectory,
    pub signer: Pubkey,
    pub client_primary: Pubkey,
    pub client_community: Pubkey,
}

impl VenueContext {
    #[must_use]
    pub fn new(dir: VenueDirectory, signer: Pubkey) -> Self {
        let client_primary = client_account(
            &dir.program_id,
            dir.version,
            AccountType::ClientPrimary as u32,
            &signer,
        );
        let client_community = client_account(
            &dir.program_id,
            dir.version,
            AccountType::ClientCommunity as u32,
            &signer,
        );
        Self {
            dir,
            signer,
            client_primary,
            client_community,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::consts::{PROGRAM_ID, TOKEN_PROGRAM, USDC_MINT};

    fn goldens() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/goldens.json"
        )))
        .expect("goldens.json parses")
    }

    fn pk(v: &serde_json::Value) -> Pubkey {
        Pubkey::from_str(v.as_str().expect("pubkey str")).expect("pubkey parses")
    }

    #[test]
    fn pda_derivations_match_kit_goldens() {
        let g = goldens();
        let program_id = Pubkey::from_str(PROGRAM_ID).unwrap();
        let version = g["version"].as_u64().unwrap() as u32;
        let signer = pk(&g["signer_pubkey"]);
        let auth = derive_drvs_authority(&program_id);
        assert_eq!(auth, pk(&g["pda"]["drvs_authority"]));
        assert_eq!(
            account_by_tag(&program_id, version, AccountType::Root as u32, &auth),
            pk(&g["pda"]["root_account"])
        );
        assert_eq!(
            account_by_tag(&program_id, version, AccountType::Community as u32, &auth),
            pk(&g["pda"]["community_account"])
        );
        assert_eq!(
            client_account(
                &program_id,
                version,
                AccountType::ClientPrimary as u32,
                &signer
            ),
            pk(&g["pda"]["client_primary"])
        );
        assert_eq!(
            client_account(
                &program_id,
                version,
                AccountType::ClientCommunity as u32,
                &signer
            ),
            pk(&g["pda"]["client_community"])
        );
        let usdc = Pubkey::from_str(USDC_MINT).unwrap();
        assert_eq!(
            program_token_account(&program_id, version, &usdc, &auth),
            pk(&g["pda"]["program_token_account_usdc"])
        );
        assert_eq!(
            associated_token_address(&signer, &Pubkey::from_str(TOKEN_PROGRAM).unwrap(), &usdc),
            pk(&g["pda"]["ata_usdc"])
        );
        let lut_slot = g["pda"]["lut_slot"].as_u64().unwrap();
        assert_eq!(
            lookup_table_address(&signer, lut_slot),
            pk(&g["pda"]["lut_address_for_slot"])
        );
    }

    #[test]
    fn seed_transposition_is_load_bearing() {
        // Swapping seed order must change the PDA — guards against a
        // "helpful" refactor unifying the two byte orders.
        let program_id = Pubkey::from_str(PROGRAM_ID).unwrap();
        let auth = derive_drvs_authority(&program_id);
        let correct = account_by_tag(&program_id, 1, 2, &auth);
        let mut transposed = [0u8; 8];
        transposed[..4].copy_from_slice(&2u32.to_le_bytes()); // tag first = WRONG for PDAs
        transposed[4..].copy_from_slice(&1u32.to_le_bytes());
        let wrong = Pubkey::find_program_address(&[&transposed, auth.as_ref()], &program_id).0;
        assert_ne!(correct, wrong);
    }

    #[test]
    fn memcmp_is_tag_then_version() {
        let b58 = discriminator_memcmp_base58(7, 1);
        let bytes = bs58::decode(&b58).into_vec().unwrap();
        assert_eq!(&bytes[..4], &7u32.to_le_bytes());
        assert_eq!(&bytes[4..], &1u32.to_le_bytes());
    }
}
