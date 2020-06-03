/**
* Fungible Token implementation with JSON serialization.
* NOTES:
*  - The maximum balance value is limited by U128 (2**128 - 1).
*  - JSON calls should pass U128 as a base-10 string. E.g. "100".
*  - The contract optimizes the inner trie structure by hashing account IDs. It will prevent some
*    abuse of deep tries. Shouldn't be an issue, once NEAR clients implement full hashing of keys.
*  - This contract doesn't optimize the amount of storage, since any account can create unlimited
*    amount of allowances to other accounts. It's unclear how to address this issue unless, this
*    contract limits the total number of different allowances possible at the same time.
*    And even if it limits the total number, it's still possible to transfer small amounts to
*    multiple accounts.
*/
use borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::Map;
use near_sdk::json_types::U128;
use near_sdk::{env, near_bindgen, AccountId, Balance, Promise, ext_contract};

#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

/// Contains balance and allowances information for one account.
#[derive(BorshDeserialize, BorshSerialize)]
pub struct Account {
    /// Current account balance.
    pub balance: Balance,
    /// Escrow Account ID hash to the allowance amount.
    /// Allowance is the amount of tokens the Escrow Account ID can spent on behalf of the account
    /// owner.
    pub allowances: Map<Vec<u8>, Balance>,
}

impl Account {
    /// Initializes a new Account with 0 balance and no allowances for a given `account_hash`.
    pub fn new(account_hash: Vec<u8>) -> Self {
        Self { balance: 0, allowances: Map::new(account_hash) }
    }

    /// Sets allowance for account `escrow_account_id` to `allowance`.
    pub fn set_allowance(&mut self, escrow_account_id: &AccountId, allowance: Balance) {
        let escrow_hash = env::sha256(escrow_account_id.as_bytes());
        if allowance > 0 {
            self.allowances.insert(&escrow_hash, &allowance);
        } else {
            self.allowances.remove(&escrow_hash);
        }
    }

    /// Returns the allowance of account `escrow_account_id`.
    pub fn get_allowance(&self, escrow_account_id: &AccountId) -> Balance {
        let escrow_hash = env::sha256(escrow_account_id.as_bytes());
        self.allowances.get(&escrow_hash).unwrap_or(0)
    }
}

//
#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize)]
pub struct FungibleToken {
    /// sha256(AccountID) -> Account details.
    pub accounts: Map<Vec<u8>, Account>,

    /// Total supply of the all token.
    pub total_supply: Balance,
    /// The account of the prover that we can use to prove
    pub prover_account: AccountId,
    /// Whether the contract can mint tokens without verification of the Ethereum PoW. Should be
    /// only set to `false` for testing and diagnostics purposes.
    pub verify_ethash: bool,
}

impl Default for FungibleToken {
    fn default() -> Self {
        panic!("Fun token should be initialized before usage")
    }
}

#[ext_contract(prover)]
pub trait Prover {
    #[result_serializer(borsh)]
    fn verify_log_entry(
        &self,
        #[serializer(borsh)] log_index: u64,
        #[serializer(borsh)] log_entry_data: Vec<u8>,
        #[serializer(borsh)] receipt_index: u64,
        #[serializer(borsh)] receipt_data: Vec<u8>,
        #[serializer(borsh)] header_data: Vec<u8>,
        #[serializer(borsh)] proof: Vec<Vec<u8>>,
        #[serializer(borsh)] skip_bridge_call: bool,
    ) -> bool;
}

#[derive(BorshDeserialize, BorshSerialize)]
pub struct Proof {
    log_index: u64,
    log_entry_data: Vec<u8>,
    receipt_index: u64,
    receipt_data: Vec<u8>,
    header_data: Vec<u8>,
    proof: Vec<Vec<u8>>,
}

#[ext_contract(ext_fungible_token)]
pub trait ExtFungibleToken {
    #[result_serializer(borsh)]
    fn finish_mint(&self,
                       #[callback]
                       #[serializer(borsh)] verification_success: bool,
                       #[serializer(borsh)] new_owner_id: AccountId,
                       #[serializer(borsh)] amount: U128) -> Promise;
}

#[near_bindgen]
impl FungibleToken {
    /// Initializes the contract with the given total supply owned by the given `owner_id`.
    #[init]
    pub fn new(owner_id: AccountId, total_supply: U128, prover_account: AccountId, verify_ethash: bool) -> Self {
        assert!(env::is_valid_account_id(owner_id.as_bytes()), "Owner's account ID is invalid");
        let total_supply = total_supply.into();
        assert!(!env::state_exists(), "Already initialized");
        let mut ft = Self { accounts: Map::new(b"a".to_vec()), total_supply, prover_account, verify_ethash };
        let mut account = ft.get_account(&owner_id);
        account.balance = total_supply;
        ft.set_account(&owner_id, &account);
        ft
    }

    /// Sets the `allowance` for `escrow_account_id` on the account of the caller of this contract
    /// (`predecessor_id`) who is the balance owner.
    pub fn set_allowance(&mut self, escrow_account_id: AccountId, allowance: U128) {
        assert!(
            env::is_valid_account_id(escrow_account_id.as_bytes()),
            "Escrow account ID is invalid"
        );
        let allowance = allowance.into();
        let owner_id = env::predecessor_account_id();
        if escrow_account_id == owner_id {
            env::panic(b"Can't set allowance for yourself");
        }
        let mut account = self.get_account(&owner_id);

        account.set_allowance(&escrow_account_id, allowance);
        self.set_account(&owner_id, &account);
    }

    /// Transfers the `amount` of tokens from `owner_id` to the `new_owner_id`.
    /// Requirements:
    /// * `amount` should be a positive integer.
    /// * `owner_id` should have balance on the account greater or equal than the transfer `amount`.
    /// * If this function is called by an escrow account (`owner_id != predecessor_account_id`),
    ///   then the allowance of the caller of the function (`predecessor_account_id`) on
    ///   the account of `owner_id` should be greater or equal than the transfer `amount`.
    pub fn transfer_from(&mut self, owner_id: AccountId, new_owner_id: AccountId, amount: U128) {
        assert!(env::is_valid_account_id(owner_id.as_bytes()), "Owner's account ID is invalid");
        assert!(
            env::is_valid_account_id(new_owner_id.as_bytes()),
            "New owner's account ID is invalid"
        );
        let amount = amount.into();
        if amount == 0 {
            env::panic(b"Can't transfer 0 tokens");
        }
        // Retrieving the account from the state.
        let mut account = self.get_account(&owner_id);

        // Checking and updating unlocked balance
        if account.balance < amount {
            env::panic(b"Not enough balance");
        }
        account.balance -= amount;

        // If transferring by escrow, need to check and update allowance.
        let escrow_account_id = env::predecessor_account_id();
        if escrow_account_id != owner_id {
            let allowance = account.get_allowance(&escrow_account_id);
            if allowance < amount {
                env::panic(b"Not enough allowance");
            }
            account.set_allowance(&escrow_account_id, allowance - amount);
        }

        // Saving the account back to the state.
        self.set_account(&owner_id, &account);

        // Deposit amount to the new owner and save the new account to the state.
        let mut new_account = self.get_account(&new_owner_id);
        new_account.balance += amount;
        self.set_account(&new_owner_id, &new_account);
    }

    /// Transfer `amount` of tokens from the caller of the contract (`predecessor_id`) to
    /// `new_owner_id`.
    /// Act the same was as `transfer_from` with `owner_id` equal to the caller of the contract
    /// (`predecessor_id`).
    pub fn transfer(&mut self, new_owner_id: AccountId, amount: U128) {
        // NOTE: New owner's Account ID checked in transfer_from
        self.transfer_from(env::predecessor_account_id(), new_owner_id, amount);
    }

    /// Mint the token, increasing the total supply given the proof that the mirror token was locked
    /// on the Ethereum blockchain.
    pub fn mint(&self,
                #[serializer(borsh)] new_owner_id: AccountId,
                #[serializer(borsh)] amount: U128,
                #[serializer(borsh)] proof: Proof) -> Promise {
        // TODO: Record events that were already used to mint the tokens.
        let Proof {
            log_index,
            log_entry_data,
            receipt_index,
            receipt_data,
            header_data,
            proof,
        } = proof;
        prover::verify_log_entry(
            log_index, log_entry_data, receipt_index, receipt_data, header_data, proof,
            !self.verify_ethash,
            &self.prover_account,
            0,
            env::prepaid_gas()/3
        ).then(
            ext_fungible_token::finish_mint(
                new_owner_id,
                amount,
                &env::current_account_id(),
                0,
                env::prepaid_gas()/3
            )
        )
    }

    /// Finish minting once the proof was successfully validated. Can only be called by the contract
    /// itself.
    pub fn finish_mint(&mut self,
                       #[callback] #[serializer(borsh)] verification_success: bool,
                       #[serializer(borsh)] new_owner_id: AccountId,
                       #[serializer(borsh)] amount: U128) {
        assert_eq!(env::predecessor_account_id(), env::current_account_id(),
                   "Finish transfer is only allowed to be called by the contract itself");
        assert!(verification_success, "Failed to verify the proof");

        let mut account = self.get_account(&new_owner_id);
        let amount: Balance = amount.into();
        account.balance += amount;
        self.total_supply += amount;
        self.set_account(&new_owner_id, &account);
    }

    /// Returns total supply of tokens.
    pub fn get_total_supply(&self) -> U128 {
        self.total_supply.into()
    }

    /// Returns balance of the `owner_id` account.
    pub fn get_balance(&self, owner_id: AccountId) -> U128 {
        self.get_account(&owner_id).balance.into()
    }

    /// Returns current allowance of `escrow_account_id` for the account of `owner_id`.
    ///
    /// NOTE: Other contracts should not rely on this information, because by the moment a contract
    /// receives this information, the allowance may already be changed by the owner.
    /// So this method should only be used on the front-end to see the current allowance.
    pub fn get_allowance(&self, owner_id: AccountId, escrow_account_id: AccountId) -> U128 {
        assert!(env::is_valid_account_id(owner_id.as_bytes()), "Owner's account ID is invalid");
        assert!(
            env::is_valid_account_id(escrow_account_id.as_bytes()),
            "Escrow account ID is invalid"
        );
        self.get_account(&owner_id).get_allowance(&escrow_account_id).into()
    }
}

impl FungibleToken {
    /// Helper method to get the account details for `owner_id`.
    fn get_account(&self, owner_id: &AccountId) -> Account {
        let account_hash = env::sha256(owner_id.as_bytes());
        self.accounts.get(&account_hash).unwrap_or_else(|| Account::new(account_hash))
    }

    /// Helper method to set the account details for `owner_id` to the state.
    fn set_account(&mut self, owner_id: &AccountId, account: &Account) {
        let account_hash = env::sha256(owner_id.as_bytes());
        self.accounts.insert(&account_hash, &account);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
mod tests {
    use near_sdk::MockedBlockchain;
    use near_sdk::{testing_env, VMContext};

    use super::*;

    fn alice() -> AccountId {
        "alice.near".to_string()
    }
    fn bob() -> AccountId {
        "bob.near".to_string()
    }
    fn carol() -> AccountId {
        "carol.near".to_string()
    }

    fn catch_unwind_silent<F: FnOnce() -> R + std::panic::UnwindSafe, R>(
        f: F,
    ) -> std::thread::Result<R> {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(f);
        std::panic::set_hook(prev_hook);
        result
    }

    fn get_context(predecessor_account_id: AccountId) -> VMContext {
        VMContext {
            current_account_id: alice(),
            signer_account_id: bob(),
            signer_account_pk: vec![0, 1, 2],
            predecessor_account_id,
            input: vec![],
            block_index: 0,
            block_timestamp: 0,
            account_balance: 0,
            account_locked_balance: 0,
            storage_usage: 10u64.pow(6),
            attached_deposit: 0,
            prepaid_gas: 10u64.pow(18),
            random_seed: vec![0, 1, 2],
            is_view: false,
            output_data_receivers: vec![],
            epoch_height: 0,
        }
    }

    #[test]
    fn test_new() {
        let context = get_context(carol());
        testing_env!(context);
        let total_supply = 1_000_000_000_000_000u128;
        let contract = FungibleToken::new(bob(), total_supply.into());
        assert_eq!(contract.get_total_supply().0, total_supply);
        assert_eq!(contract.get_balance(bob()).0, total_supply);
    }

    #[test]
    fn test_new_twice_fails() {
        let context = get_context(carol());
        testing_env!(context);
        let total_supply = 1_000_000_000_000_000u128;
        let _contract = FungibleToken::new(bob(), total_supply.into());
        catch_unwind_silent(|| {
            FungibleToken::new(bob(), total_supply.into());
        })
        .unwrap_err();
    }

    #[test]
    fn test_transfer() {
        let context = get_context(carol());
        testing_env!(context);
        let total_supply = 1_000_000_000_000_000u128;
        let mut contract = FungibleToken::new(carol(), total_supply.into());
        let transfer_amount = total_supply / 3;
        contract.transfer(bob(), transfer_amount.into());
        assert_eq!(contract.get_balance(carol()).0, (total_supply - transfer_amount));
        assert_eq!(contract.get_balance(bob()).0, transfer_amount);
    }

    #[test]
    fn test_self_allowance_fail() {
        let context = get_context(carol());
        testing_env!(context);
        let total_supply = 1_000_000_000_000_000u128;
        let mut contract = FungibleToken::new(carol(), total_supply.into());
        catch_unwind_silent(move || {
            contract.set_allowance(carol(), (total_supply / 2).into());
        })
        .unwrap_err();
    }

    #[test]
    fn test_carol_escrows_to_bob_transfers_to_alice() {
        // Acting as carol
        testing_env!(get_context(carol()));
        let total_supply = 1_000_000_000_000_000u128;
        let mut contract = FungibleToken::new(carol(), total_supply.into());
        assert_eq!(contract.get_total_supply().0, total_supply);
        let allowance = total_supply / 3;
        let transfer_amount = allowance / 3;
        contract.set_allowance(bob(), allowance.into());
        assert_eq!(contract.get_allowance(carol(), bob()).0, allowance);
        // Acting as bob now
        testing_env!(get_context(bob()));
        contract.transfer_from(carol(), alice(), transfer_amount.into());
        assert_eq!(contract.get_balance(carol()).0, total_supply - transfer_amount);
        assert_eq!(contract.get_balance(alice()).0, transfer_amount);
        assert_eq!(contract.get_allowance(carol(), bob()).0, allowance - transfer_amount);
    }

    #[test]
    fn test_carol_escrows_to_bob_locks_and_transfers_to_alice() {
        // Acting as carol
        testing_env!(get_context(carol()));
        let total_supply = 1_000_000_000_000_000u128;
        let mut contract = FungibleToken::new(carol(), total_supply.into());
        assert_eq!(contract.get_total_supply().0, total_supply);
        let allowance = total_supply / 3;
        let transfer_amount = allowance / 3;
        contract.set_allowance(bob(), allowance.into());
        assert_eq!(contract.get_allowance(carol(), bob()).0, allowance);
        // Acting as bob now
        testing_env!(get_context(bob()));
        assert_eq!(contract.get_balance(carol()).0, total_supply);
        contract.transfer_from(carol(), alice(), transfer_amount.into());
        assert_eq!(contract.get_balance(carol()).0, (total_supply - transfer_amount));
        assert_eq!(contract.get_balance(alice()).0, transfer_amount);
        assert_eq!(contract.get_allowance(carol(), bob()).0, allowance - transfer_amount);
    }
}
