pub struct Ledger {
    balance: i64,
}

#[export]
impl Ledger {
    pub fn new(balance: i64) -> Self {
        Self { balance }
    }

    pub fn balance(&self) -> i64 {
        self.balance
    }
}
