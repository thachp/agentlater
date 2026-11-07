//! The slice of SQLite the ledger needs. In the Worker this is Durable Object SQL storage;
//! in native tests it is rusqlite, so the same SQL is exercised in both places.

use serde::de::DeserializeOwned;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Null,
    Int(i64),
    Text(String),
}

impl From<i64> for Param {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<u32> for Param {
    fn from(v: u32) -> Self {
        Self::Int(v.into())
    }
}

impl From<u64> for Param {
    fn from(v: u64) -> Self {
        // Generations and fences stay far below i64::MAX.
        Self::Int(v as i64)
    }
}

impl From<bool> for Param {
    fn from(v: bool) -> Self {
        Self::Int(v.into())
    }
}

impl From<&str> for Param {
    fn from(v: &str) -> Self {
        Self::Text(v.into())
    }
}

impl From<String> for Param {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

#[derive(Debug)]
pub struct DbError(pub String);

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "storage error: {}", self.0)
    }
}

impl std::error::Error for DbError {}

pub trait Db {
    fn exec(&self, sql: &str, params: Vec<Param>) -> Result<(), DbError>;
    /// Rows are deserialized by column name.
    fn query<T: DeserializeOwned>(&self, sql: &str, params: Vec<Param>) -> Result<Vec<T>, DbError>;
}

#[cfg(test)]
pub(crate) mod sqlite {
    use super::*;
    use rusqlite::types::{Value as Sql, ValueRef};

    pub struct Memory(pub rusqlite::Connection);

    impl Memory {
        pub fn new() -> Self {
            let db = rusqlite::Connection::open_in_memory().unwrap();
            // Durable Object SQLite refuses compound SELECTs with more terms than this.
            db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_COMPOUND_SELECT, 5)
                .unwrap();
            Self(db)
        }
    }

    fn bind(params: Vec<Param>) -> Vec<Sql> {
        params
            .into_iter()
            .map(|p| match p {
                Param::Null => Sql::Null,
                Param::Int(i) => Sql::Integer(i),
                Param::Text(s) => Sql::Text(s),
            })
            .collect()
    }

    impl Db for Memory {
        fn exec(&self, sql: &str, params: Vec<Param>) -> Result<(), DbError> {
            self.0
                .execute(sql, rusqlite::params_from_iter(bind(params)))
                .map(|_| ())
                .map_err(|e| DbError(e.to_string()))
        }

        fn query<T: DeserializeOwned>(
            &self,
            sql: &str,
            params: Vec<Param>,
        ) -> Result<Vec<T>, DbError> {
            let err = |e: rusqlite::Error| DbError(e.to_string());
            let mut stmt = self.0.prepare(sql).map_err(err)?;
            let names: Vec<String> = stmt.column_names().map(str::to_string).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(bind(params)))
                .map_err(err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(err)? {
                let mut map = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    let v = match row.get_ref(i).map_err(err)? {
                        ValueRef::Null => serde_json::Value::Null,
                        ValueRef::Integer(n) => n.into(),
                        ValueRef::Real(f) => f.into(),
                        ValueRef::Text(t) => String::from_utf8_lossy(t).into(),
                        ValueRef::Blob(_) => return Err(DbError("unexpected blob".into())),
                    };
                    map.insert(name.clone(), v);
                }
                out.push(serde_json::from_value(map.into()).map_err(|e| DbError(e.to_string()))?);
            }
            Ok(out)
        }
    }
}
