#[data]
pub struct Padded {
    pub tag: u16,
    pub value: i32,
    pub flag: u8,
}

#[data]
pub enum Item {
    Padded(Padded),
    Empty,
}

#[export]
pub fn items() -> Vec<Item> {
    Vec::new()
}
