use assert_matches::assert_matches;
use solana_runtime::bank::Bank;
use solana_runtime::bank_client::BankClient;
use solana_runtime::genesis_utils::{create_genesis_block, GenesisBlockInfo};
use solana_sdk::account_utils::State;
use solana_sdk::client::SyncClient;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::sysvar;
use solana_sdk::sysvar::rewards::Rewards;
use solana_stake_api::id;
use solana_stake_api::stake_instruction;
use solana_stake_api::stake_instruction::process_instruction;
use solana_stake_api::stake_state::StakeState;
use solana_vote_api::vote_instruction;
use solana_vote_api::vote_state::{Vote, VoteState};
use std::sync::Arc;

fn fill_epoch_with_votes(
    bank: &Arc<Bank>,
    vote_keypair: &Keypair,
    mint_keypair: &Keypair,
) -> Arc<Bank> {
    let mint_pubkey = mint_keypair.pubkey();
    let vote_pubkey = vote_keypair.pubkey();
    let old_epoch = bank.epoch();
    let mut bank = bank.clone();
    while bank.epoch() != old_epoch + 1 {
        bank = Arc::new(Bank::new_from_parent(
            &bank,
            &Pubkey::default(),
            1 + bank.slot(),
        ));

        let bank_client = BankClient::new_shared(&bank);
        let parent = bank.parent().unwrap();

        let message = Message::new_with_payer(
            vec![vote_instruction::vote(
                &vote_pubkey,
                &vote_pubkey,
                vec![Vote::new(parent.slot() as u64, parent.hash())],
            )],
            Some(&mint_pubkey),
        );
        assert!(bank_client
            .send_message(&[&mint_keypair, &vote_keypair], message)
            .is_ok());
    }
    bank
}

#[test]
fn test_stake_account_delegate() {
    let staker_keypair = Keypair::new();
    let staker_pubkey = staker_keypair.pubkey();
    let vote_keypair = Keypair::new();
    let vote_pubkey = vote_keypair.pubkey();
    let node_pubkey = Pubkey::new_rand();

    let GenesisBlockInfo {
        mut genesis_block,
        mint_keypair,
        ..
    } = create_genesis_block(100_000_000_000);
    genesis_block
        .native_instruction_processors
        .push(solana_stake_program::solana_stake_program!());
    let bank = Bank::new(&genesis_block);
    let mint_pubkey = mint_keypair.pubkey();
    let mut bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    // Create Vote Account
    let message = Message::new(vote_instruction::create_account(
        &mint_pubkey,
        &vote_pubkey,
        &node_pubkey,
        std::u8::MAX / 2,
        10,
    ));
    bank_client
        .send_message(&[&mint_keypair], message)
        .expect("failed to create vote account");

    // Create stake account and delegate to vote account
    let message = Message::new(stake_instruction::create_stake_account_and_delegate_stake(
        &mint_pubkey,
        &staker_pubkey,
        &vote_pubkey,
        20000,
    ));
    bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .expect("failed to create and delegate stake account");

    // Test that correct lamports are staked
    let account = bank.get_account(&staker_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeState::Stake(stake) = stake_state {
        assert_eq!(stake.stake, 20000);
    } else {
        assert!(false, "wrong account type found")
    }

    // Test that we cannot withdraw staked lamports
    let message = Message::new_with_payer(
        vec![stake_instruction::withdraw(
            &staker_pubkey,
            &Pubkey::new_rand(),
            20000,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .is_err());

    // Test that lamports are still staked
    let account = bank.get_account(&staker_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeState::Stake(stake) = stake_state {
        assert_eq!(stake.stake, 20000);
    } else {
        assert!(false, "wrong account type found")
    }

    // Reward redemption
    // Submit enough votes to generate rewards
    let old_epoch = bank.epoch();
    bank = fill_epoch_with_votes(&bank, &vote_keypair, &mint_keypair);

    // Test that votes and credits are there
    let account = bank.get_account(&vote_pubkey).expect("account not found");
    let vote_state: VoteState = account.state().expect("couldn't unpack account data");

    // 1 less vote, as the first vote should have cleared the lockout
    assert_eq!(vote_state.votes.len(), 31);
    assert_eq!(vote_state.credits(), 1);
    assert_ne!(old_epoch, bank.epoch());

    // Cycle thru banks until we reach next epoch
    bank = fill_epoch_with_votes(&bank, &vote_keypair, &mint_keypair);

    // Test that rewards are there
    let rewards_account = bank
        .get_account(&sysvar::rewards::id())
        .expect("account not found");
    assert_matches!(Rewards::from(&rewards_account), Some(_));

    // Redeem the credit
    let bank_client = BankClient::new_shared(&bank);
    let message = Message::new_with_payer(
        vec![stake_instruction::redeem_vote_credits(
            &staker_pubkey,
            &vote_pubkey,
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(bank_client.send_message(&[&mint_keypair], message), Ok(_));

    // Test that balance increased, and calculate the rewards
    let rewards;
    let account = bank.get_account(&staker_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeState::Stake(stake) = stake_state {
        assert!(account.lamports > 20000);
        assert_eq!(stake.stake, 20000);
        rewards = account.lamports - 20000;
    } else {
        rewards = 0;
        assert!(false, "wrong account type found")
    }

    // Deactivate the stake
    let message = Message::new_with_payer(
        vec![stake_instruction::deactivate_stake(&staker_pubkey)],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .is_ok());

    // Test that we cannot withdraw staked lamports due to cooldown period
    let message = Message::new_with_payer(
        vec![stake_instruction::withdraw(
            &staker_pubkey,
            &Pubkey::new_rand(),
            20000,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .is_err());

    let old_epoch = bank.epoch();
    let slots = bank.get_slots_in_epoch(old_epoch);

    // Create a new bank at later epoch (within cooldown period)
    let bank = Bank::new_from_parent(&bank, &Pubkey::default(), slots + bank.slot());
    assert_ne!(old_epoch, bank.epoch());
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    let message = Message::new_with_payer(
        vec![stake_instruction::withdraw(
            &staker_pubkey,
            &Pubkey::new_rand(),
            20000,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .is_err());

    // Create a new bank at later epoch (to account for cooldown of stake)
    let mut bank = Bank::new_from_parent(
        &bank,
        &Pubkey::default(),
        genesis_block.slots_per_epoch * 4 + bank.slot(),
    );
    bank.add_instruction_processor(id(), process_instruction);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    // Test that we can withdraw now
    let message = Message::new_with_payer(
        vec![stake_instruction::withdraw(
            &staker_pubkey,
            &Pubkey::new_rand(),
            20000,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_message(&[&mint_keypair, &staker_keypair], message)
        .is_ok());

    // Test that balance and stake is updated correctly (we have withdrawn all lamports except rewards)
    let account = bank.get_account(&staker_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeState::Stake(stake) = stake_state {
        assert_eq!(account.lamports, rewards);
        assert_eq!(stake.stake, rewards);
    } else {
        assert!(false, "wrong account type found")
    }
}
