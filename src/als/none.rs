use anyhow::Result;

#[derive(Default)]
pub struct Als {}

impl Als {
    pub async fn get(&self) -> Result<u64> {
        Ok(0)
    }
}
