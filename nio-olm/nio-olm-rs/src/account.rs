use nio_olm_sys::OlmAccount;

pub struct Account {
    account: *mut OlmAccount,
    buffer: Vec<u8>,
}

impl Account {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Account {
        let (acc_ptr, account_data) = unsafe {
            let account_size = nio_olm_sys::olm_account_size();
            let account_data: Vec<u8> = vec![0; account_size];
            let acc_ptr = nio_olm_sys::olm_account(account_data.as_ptr() as *mut _);
            (acc_ptr, account_data)
        };

        Account {
            account: acc_ptr,
            buffer: account_data
        }
    }
}

#[test]
fn create_account() {
    Account::new();
}
