//! Token program handling.
//!
//! SPL Token mints never charge a transfer fee (`utils/token.rs` `get_transfer_fee`
//! returns 0 for them). For Token-2022 mints the adapter does NOT model transfer fees: a
//! mint carrying the `TransferFeeConfig` extension (or any extension outside a small
//! allowlist that cannot change a transfer's raw amount) makes the pool inactive and
//! every quote on it an error, instead of guessing.

use solana_pubkey::{Pubkey, pubkey};

pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// `spl_token_2022::state::Mint::LEN`.
const MINT_BASE_LEN: usize = 82;
/// `spl_token_2022::state::Account::LEN`: extension data starts after the account-type
/// byte that follows this (padded) length.
const ACCOUNT_LEN: usize = 165;
/// `AccountType::Mint`.
const ACCOUNT_TYPE_MINT: u8 = 1;

/// Token-2022 `ExtensionType` values that cannot change the raw amount a `transfer_checked`
/// moves, nor block it: the subset of the program's own `is_supported_mint` allowlist
/// minus `TransferFeeConfig` (1). `InterestBearingConfig` (10) and `ScaledUiAmount` (25)
/// only affect UI amounts; `MetadataPointer` (18) / `TokenMetadata` (19) are metadata.
const RAW_AMOUNT_NEUTRAL_EXTENSIONS: [u16; 4] = [10, 18, 19, 25];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintSupport {
    Supported,
    Unsupported(String),
}

/// Classify a mint account by owner and (for Token-2022) its extensions.
pub fn mint_support(owner: &Pubkey, data: &[u8]) -> MintSupport {
    if *owner == TOKEN_PROGRAM_ID {
        return MintSupport::Supported;
    }
    if *owner != TOKEN_2022_PROGRAM_ID {
        return MintSupport::Unsupported(format!("mint owned by unknown program {owner}"));
    }
    if data.len() == MINT_BASE_LEN {
        return MintSupport::Supported; // no extensions
    }
    if data.len() <= ACCOUNT_LEN || data[ACCOUNT_LEN] != ACCOUNT_TYPE_MINT {
        return MintSupport::Unsupported("malformed Token-2022 mint".into());
    }
    let mut off = ACCOUNT_LEN + 1;
    while off + 4 <= data.len() {
        let ty = u16::from_le_bytes([data[off], data[off + 1]]);
        let len = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
        if ty == 0 {
            break; // Uninitialized: end of TLV data
        }
        if !RAW_AMOUNT_NEUTRAL_EXTENSIONS.contains(&ty) {
            let what = if ty == 1 {
                "TransferFeeConfig (transfer fees are not modelled)".to_string()
            } else {
                format!("extension type {ty}")
            };
            return MintSupport::Unsupported(format!("unsupported Token-2022 mint: {what}"));
        }
        off += 4 + len;
    }
    MintSupport::Supported
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t22_mint(exts: &[(u16, usize)]) -> Vec<u8> {
        let mut d = vec![0u8; ACCOUNT_LEN];
        d.push(ACCOUNT_TYPE_MINT);
        for &(ty, len) in exts {
            d.extend_from_slice(&ty.to_le_bytes());
            d.extend_from_slice(&(len as u16).to_le_bytes());
            d.extend(std::iter::repeat_n(0u8, len));
        }
        d
    }

    #[test]
    fn spl_token_is_supported() {
        assert_eq!(
            mint_support(&TOKEN_PROGRAM_ID, &[0u8; 82]),
            MintSupport::Supported
        );
    }

    #[test]
    fn plain_and_metadata_token_2022_supported() {
        assert_eq!(
            mint_support(&TOKEN_2022_PROGRAM_ID, &[0u8; 82]),
            MintSupport::Supported
        );
        let d = t22_mint(&[(18, 64), (19, 120)]);
        assert_eq!(
            mint_support(&TOKEN_2022_PROGRAM_ID, &d),
            MintSupport::Supported
        );
    }

    #[test]
    fn transfer_fee_and_hooks_rejected() {
        let d = t22_mint(&[(18, 64), (1, 108)]);
        assert!(matches!(
            mint_support(&TOKEN_2022_PROGRAM_ID, &d),
            MintSupport::Unsupported(s) if s.contains("TransferFeeConfig")
        ));
        let d = t22_mint(&[(14, 64)]); // TransferHook
        assert!(matches!(
            mint_support(&TOKEN_2022_PROGRAM_ID, &d),
            MintSupport::Unsupported(_)
        ));
        let d = t22_mint(&[(26, 33)]); // Pausable
        assert!(matches!(
            mint_support(&TOKEN_2022_PROGRAM_ID, &d),
            MintSupport::Unsupported(_)
        ));
    }

    #[test]
    fn unknown_owner_rejected() {
        assert!(matches!(
            mint_support(&Pubkey::new_from_array([9; 32]), &[0u8; 82]),
            MintSupport::Unsupported(_)
        ));
    }
}
