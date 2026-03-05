use super::types::StoredTryst;
use crate::{error::LibError, protocols::tryst::crypto::EpochSecret};
use anyhow::anyhow;
use iroh::EndpointId;
use sqlx::SqlitePool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct RawTryst {
    id: String,
    account_id: Option<String>,
    current_secret: Vec<u8>,
    previous_secret: Option<Vec<u8>>,
    last_updated_epoch: i64,
    is_verified: bool,
}

#[derive(Debug, Clone)]
pub struct TrystRepository {
    pool: SqlitePool,
}

impl TrystRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn save(&self, tryst: &StoredTryst) -> Result<(), LibError> {
        let id = tryst.id.to_string();
        let account_id = tryst.account_id.map(|id| id.to_string());
        let current_secret = tryst.current_secret.to_bytes().to_vec();
        let previous_secret = tryst.previous_secret.map(|s| s.to_bytes().to_vec());
        let last_updated_epoch = tryst.last_updated_epoch as i64;
        let is_verified = tryst.is_verified;

        sqlx::query!(
            r#"
            INSERT INTO tryst
            (id, account_id, current_secret, previous_secret, last_updated_epoch, is_verified)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                current_secret = excluded.current_secret,
                previous_secret = excluded.previous_secret,
                last_updated_epoch = excluded.last_updated_epoch,
                is_verified = excluded.is_verified
            "#,
            id,
            account_id,
            current_secret,
            previous_secret,
            last_updated_epoch,
            is_verified
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<StoredTryst>, LibError> {
        let id_str = id.to_string();

        let tryst_opt = sqlx::query_as!(
            RawTryst,
            r#"
            SELECT id, account_id, current_secret, previous_secret, last_updated_epoch, is_verified
            FROM tryst WHERE id = ?
            "#,
            id_str
        )
        .fetch_optional(&self.pool)
        .await?
        .and_then(|r| match StoredTryst::try_from(r) {
            Ok(tryst) => Some(tryst),
            Err(err) => {
                tracing::warn!(?err, "Failed to map raw tryst record from db.");
                None
            }
        });

        match tryst_opt {
            Some(mut tryst) => {
                self.ratchet_and_save_tryst(&mut tryst).await;

                Ok(Some(tryst))
            }
            None => Ok(None),
        }
    }

    pub async fn get_by_account_id(
        &self,
        account_id: EndpointId,
    ) -> Result<Option<StoredTryst>, LibError> {
        let peer_id_str = account_id.to_string();

        let tryst_opt = sqlx::query_as!(
            RawTryst,
            r#"
            SELECT id, account_id, current_secret, previous_secret, last_updated_epoch, is_verified
            FROM tryst WHERE account_id = ?
            "#,
            peer_id_str
        )
        .fetch_optional(&self.pool)
        .await?
        .and_then(|r| match StoredTryst::try_from(r) {
            Ok(tryst) => Some(tryst),
            Err(err) => {
                tracing::warn!(?err, "Failed to map raw tryst record from db.");
                None
            }
        });

        match tryst_opt {
            Some(mut tryst) => {
                self.ratchet_and_save_tryst(&mut tryst).await;

                Ok(Some(tryst))
            }
            None => Ok(None),
        }
    }

    pub async fn get_all(&self) -> Result<Vec<StoredTryst>, LibError> {
        let mut trysts = sqlx::query_as!(
            RawTryst,
            r#"
            SELECT id, account_id, current_secret, previous_secret, last_updated_epoch, is_verified
            FROM tryst
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .filter_map(|r| match StoredTryst::try_from(r) {
            Ok(tryst) => Some(tryst),
            Err(err) => {
                tracing::warn!(?err, "Failed to map raw tryst record from db.");
                None
            }
        })
        .collect::<Vec<_>>();

        for tryst in trysts.iter_mut() {
            self.ratchet_and_save_tryst(tryst).await;
        }

        Ok(trysts)
    }

    async fn ratchet_and_save_tryst(&self, tryst: &mut StoredTryst) {
        if tryst.ratchet_if_needed()
            && let Err(err) = self.save(tryst).await
        {
            tracing::error!(
                tryst_id = ?tryst.id,
                tryst_account_id = ?tryst.account_id,
                ?err,
                "Failed to save updated tryst record after ratcheting epoch secret."
            );
        }
    }
}

impl TryFrom<RawTryst> for StoredTryst {
    type Error = LibError;

    fn try_from(row: RawTryst) -> Result<Self, Self::Error> {
        let id = Uuid::from_str(&row.id)
            .map_err(|_| LibError::Unexpected(anyhow!("Invalid UUID in DB")))?;

        let account_id = match row.account_id {
            Some(s) => Some(EndpointId::from_str(&s).map_err(|_| {
                LibError::Unexpected(anyhow!("Invalid EndpointId in DB"))
            })?),
            None => None,
        };

        let current_secret_bytes: [u8; 32] = row
            .current_secret
            .try_into()
            .map_err(|_| LibError::Unexpected(anyhow!("Invalid secret length")))?;

        let current_secret = EpochSecret::from(current_secret_bytes);

        let previous_secret = match row.previous_secret {
            Some(bytes) => {
                let secret_bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                    LibError::Unexpected(anyhow!("Invalid previous secret length"))
                })?;
                Some(EpochSecret::from(secret_bytes))
            }
            None => None,
        };

        Ok(StoredTryst {
            id,
            account_id,
            current_secret,
            previous_secret,
            last_updated_epoch: row.last_updated_epoch as u64,
            is_verified: row.is_verified,
        })
    }
}
