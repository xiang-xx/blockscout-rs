pub mod mock;
pub mod new_blocks;
pub mod total_blocks;

use async_trait::async_trait;
use sea_orm::{DatabaseConnection, DbErr};
use thiserror::Error;

#[async_trait]
pub trait UpdaterTrait {
    async fn update(
        &self,
        db: &DatabaseConnection,
        blockscout: &DatabaseConnection,
    ) -> Result<(), UpdateError>;

    fn name(&self) -> &str;
}

#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("database error {0}")]
    DB(#[from] DbErr),
    #[error("chart {0} not found")]
    NotFound(String),
}