use anchor_lang::{prelude::*, solana_program::instruction::Instruction};

pub const SWAP_V3_DYN_DISCRIMINATOR: [u8; 8] = [229, 46, 213, 132, 105, 40, 40, 228];

pub fn swap_v3_dyn(
    venue_program_id: Pubkey,
    amount_in: u64,
    account_metas: &[AccountMeta],
) -> Result<Vec<Instruction>> {
    let mut data = Vec::with_capacity(41);
    data.extend_from_slice(&SWAP_V3_DYN_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes());
    data.push(1);

    Ok(vec![Instruction {
        program_id: venue_program_id,
        accounts: account_metas.to_vec(),
        data,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_v3_dyn_data_layout_is_stable() {
        let ix = swap_v3_dyn(Pubkey::new_unique(), 123, &[]).unwrap().remove(0);

        let mut expected = Vec::new();
        expected.extend_from_slice(&SWAP_V3_DYN_DISCRIMINATOR);
        expected.extend_from_slice(&123u64.to_le_bytes());
        expected.extend_from_slice(&0u64.to_le_bytes());
        expected.extend_from_slice(&0u128.to_le_bytes());
        expected.push(1);

        assert_eq!(ix.data, expected);
    }

    #[test]
    fn swap_v3_dyn_preserves_program_and_accounts() {
        let venue_program_id = Pubkey::new_unique();
        let writable = Pubkey::new_unique();
        let readonly = Pubkey::new_unique();
        let accounts = vec![
            AccountMeta::new(writable, true),
            AccountMeta::new_readonly(readonly, false),
        ];

        let ix = swap_v3_dyn(venue_program_id, 456, &accounts)
            .unwrap()
            .remove(0);

        assert_eq!(ix.program_id, venue_program_id);
        assert_eq!(ix.accounts, accounts);
    }
}
