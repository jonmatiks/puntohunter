//! V1 wire messages: line-delimited JSON-RPC 1.0.

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// The positional param at `index`, or null when absent.
    pub fn param(&self, index: usize) -> &Value {
        self.params.get(index).unwrap_or(&Value::Null)
    }
}

/// The conventional Stratum V1 error codes. Each rejection class answers with its own error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StratumError {
    Other,
    StaleJob,
    Duplicate,
    LowDifficulty,
    Unauthorized,
    NotSubscribed,
}

impl StratumError {
    fn code(self) -> i64 {
        match self {
            StratumError::Other => 20,
            StratumError::StaleJob => 21,
            StratumError::Duplicate => 22,
            StratumError::LowDifficulty => 23,
            StratumError::Unauthorized => 24,
            StratumError::NotSubscribed => 25,
        }
    }
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({"id": id, "result": result, "error": null})
}

pub fn error(id: &Value, error: StratumError, message: &str) -> Value {
    json!({"id": id, "result": null, "error": [error.code(), message, null]})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"id": null, "method": method, "params": params})
}
