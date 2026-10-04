//! Persistent records of invoices issued through LNURL.

use std::{path::Path, sync::Arc};

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const INVOICES: TableDefinition<&str, &[u8]> = TableDefinition::new("lnurl_invoices_v1");

/// Current on-disk representation version.
const SCHEMA_VERSION: u32 = 1;

/// An invoice created in response to an LNURL callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedInvoice {
    /// Version of this serialized record.
    pub schema_version: u32,
    /// BOLT11 payment hash returned by ldk-server.
    pub payment_hash: String,
    /// Encoded BOLT11 invoice given to the payer.
    pub invoice: String,
    /// Requested amount in millisatoshis.
    pub amount_msat: u64,
    /// LNURL username through which the invoice was requested.
    pub username: String,
    /// UNIX timestamp at which the invoice was issued.
    pub created_at: u64,
    /// UNIX timestamp at which the invoice expires.
    pub expires_at: u64,
}

impl IssuedInvoice {
    /// Construct a record using the current schema version.
    pub fn new(
        payment_hash: String,
        invoice: String,
        amount_msat: u64,
        username: String,
        created_at: u64,
        expires_at: u64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            payment_hash,
            invoice,
            amount_msat,
            username,
            created_at,
            expires_at,
        }
    }
}

/// Invoice persistence boundary.
pub trait InvoiceStore: Send + Sync + 'static {
    /// Atomically insert or replace an invoice, keyed by payment hash.
    fn insert(&self, invoice: &IssuedInvoice) -> Result<(), StoreError>;

    /// List all stored invoices, newest first.
    fn list(&self) -> Result<Vec<IssuedInvoice>, StoreError>;
}

/// A single-file redb invoice store.
#[derive(Debug, Clone)]
pub struct RedbInvoiceStore {
    database: Arc<Database>,
}

impl RedbInvoiceStore {
    /// Open or create the database and initialize its invoice table.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let database = Database::create(path)?;
        let write = database.begin_write()?;
        {
            write.open_table(INVOICES)?;
        }
        write.commit()?;
        Ok(Self {
            database: Arc::new(database),
        })
    }
}

impl InvoiceStore for RedbInvoiceStore {
    fn insert(&self, invoice: &IssuedInvoice) -> Result<(), StoreError> {
        let serialized = serde_json::to_vec(invoice)?;
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(INVOICES)?;
            table.insert(invoice.payment_hash.as_str(), serialized.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<IssuedInvoice>, StoreError> {
        let read = self.database.begin_read()?;
        let table = read.open_table(INVOICES)?;
        let mut invoices = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            let invoice: IssuedInvoice = serde_json::from_slice(value.value())?;
            invoices.push(invoice);
        }
        invoices.sort_unstable_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.payment_hash.cmp(&left.payment_hash))
        });
        Ok(invoices)
    }
}

/// Invoice storage failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// redb failed to open the database.
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    /// redb failed to begin a transaction.
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    /// redb failed to open a table.
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    /// redb failed to access table storage.
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    /// redb failed to commit a transaction.
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    /// A stored record could not be encoded or decoded.
    #[error("invoice serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoice(hash: &str, created_at: u64) -> IssuedInvoice {
        IssuedInvoice::new(
            hash.to_owned(),
            format!("lnbc-{hash}"),
            21_000,
            "alice".to_owned(),
            created_at,
            created_at + 600,
        )
    }

    #[test]
    fn records_survive_reopening_and_are_newest_first() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("invoices.redb");
        {
            let store = RedbInvoiceStore::open(&path).expect("database should open");
            store
                .insert(&invoice("older", 10))
                .expect("first insert should succeed");
            store
                .insert(&invoice("newer", 20))
                .expect("second insert should succeed");
        }

        let reopened = RedbInvoiceStore::open(path).expect("database should reopen");
        assert_eq!(
            reopened
                .list()
                .expect("stored invoices should be readable")
                .iter()
                .map(|invoice| invoice.payment_hash.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
    }
}
