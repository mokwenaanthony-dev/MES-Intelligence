//! The messages between the edge agent and the MES (ISA-95 Levels 2 and 3).
//!
//! They live in the Unified Namespace beside the ERP contract's topics, but the ERP never reads
//! them; they are documented in `docs/EDGE_CONTRACT.md` of this repository. All of them are
//! **retained state**, so publishing the same message twice changes nothing and a component that
//! restarts gets the current picture at once:
//!
//! | Topic | Publisher | Schema | Meaning |
//! |---|---|---|---|
//! | `<cell>/schedule/current-job` | MES | `job/v1` | the job this cell should run now (empty = none) |
//! | `<cell>/state/job` | edge agent | `job-progress/v1` | how far the cell has got with that job |
//! | `<cell>/state/status` | edge agent | `equipment-state/v1` | what the cell is doing (ERP contract v1) |
//!
//! Every message uses the contract's envelope: `{schema, event_id, timestamp, source, data}`.
//! Quantities and hours are decimal strings, as in the ERP contract.

use crate::json::{self, Value};

/// Schema of the MES's job message.
pub const JOB: &str = "job/v1";
/// Schema of the agent's progress message.
pub const JOB_PROGRESS: &str = "job-progress/v1";
/// Schema of the cell state message (ERP contract v1).
pub const EQUIPMENT_STATE: &str = "equipment-state/v1";

/// Where the MES puts the cell's current job.
pub fn current_job_topic(cell: &str) -> String {
    format!("{cell}/schedule/current-job")
}

/// Where the agent reports job progress.
pub fn job_progress_topic(cell: &str) -> String {
    format!("{cell}/state/job")
}

/// Where the cell's state is kept (ERP contract v1).
pub fn state_topic(cell: &str) -> String {
    format!("{cell}/state/status")
}

/// A message in the contract envelope, as bytes. Retained state carries no event ID.
pub fn envelope(schema: &str, source: &str, data: Value) -> Vec<u8> {
    Value::object(vec![
        ("schema", Value::str(schema)),
        ("event_id", Value::Null),
        ("timestamp", Value::str(crate::time::now_iso())),
        ("source", Value::str(source)),
        ("data", data),
    ])
    .to_json()
    .into_bytes()
}

/// The job the MES wants the cell to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// `<order>-<operation>` in lower case, e.g. `mo00000001-0010`: unique per operation.
    pub job_id: String,
    /// The production order.
    pub order: String,
    /// The operation of that order.
    pub operation_number: i64,
    /// Good parts to make: the quantity rounded up to a whole part.
    pub target: u32,
}

/// Whole parts for a decimal quantity such as `"100.000"`: a fraction of a part still needs a part.
pub fn parts_for(quantity: &str) -> Option<u32> {
    let (whole, fraction) = quantity.split_once('.').unwrap_or((quantity, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None; // negative, exponent or not a number: not a quantity of parts
    }
    let whole: u32 = whole.parse().ok()?;
    let round_up = fraction.bytes().any(|b| b != b'0');
    whole.checked_add(u32::from(round_up))
}

/// Read a `job/v1` message. `Ok(None)` for an empty payload (the MES cleared the job).
pub fn parse_job(payload: &[u8]) -> Result<Option<Job>, String> {
    if payload.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(payload).map_err(|_| "payload is not UTF-8".to_string())?;
    let message = json::parse(text).map_err(|e| e.to_string())?;
    let schema = message
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != JOB {
        return Err(format!("unknown schema {schema:?}"));
    }
    let data = message.get("data").ok_or("no data object")?;
    let field = |name: &str| data.get(name).ok_or_else(|| format!("missing {name}"));
    let job_id = field("job_id")?
        .as_str()
        .ok_or("job_id must be a string")?
        .to_string();
    if job_id.is_empty() || job_id.len() > 64 {
        return Err("job_id must have 1 to 64 characters".into());
    }
    let quantity = field("quantity")?
        .as_decimal_text()
        .ok_or("quantity must be a decimal")?;
    let target = parts_for(quantity)
        .ok_or_else(|| format!("quantity {quantity} is not a number of parts"))?;
    Ok(Some(Job {
        job_id,
        order: field("order")?
            .as_str()
            .ok_or("order must be a string")?
            .to_string(),
        operation_number: field("operation_number")?
            .as_i64()
            .ok_or("operation_number must be a whole number")?,
        target,
    }))
}

/// Machine seconds as hours with three decimals, rounded half up: `37800` gives `"10.500"`.
pub fn hours_text(seconds: u32) -> String {
    let thousandths = (u64::from(seconds) * 1_000 + 1_800) / 3_600;
    format!("{}.{:03}", thousandths / 1_000, thousandths % 1_000)
}

/// How far the cell has got with a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The job.
    pub job_id: String,
    /// Its order.
    pub order: String,
    /// Its operation.
    pub operation_number: i64,
    /// `RUNNING` or `COMPLETE`.
    pub status: String,
    /// Good parts so far.
    pub good: u32,
    /// Scrap so far.
    pub scrap: u32,
    /// Machine seconds so far.
    pub run_seconds: u32,
}

impl Progress {
    /// The `data` object of a `job-progress/v1` message.
    pub fn to_data(&self) -> Value {
        Value::object(vec![
            ("job_id", Value::str(&self.job_id)),
            ("order", Value::str(&self.order)),
            ("operation_number", Value::int(self.operation_number)),
            ("status", Value::str(&self.status)),
            ("good_quantity", Value::str(self.good.to_string())),
            ("scrap_quantity", Value::str(self.scrap.to_string())),
            ("run_hours", Value::str(hours_text(self.run_seconds))),
        ])
    }

    /// As `key=value` lines, for the agent's state file.
    pub fn to_lines(&self) -> String {
        format!(
            "job_id={}\norder={}\noperation_number={}\nstatus={}\ngood={}\nscrap={}\nrun_seconds={}\n",
            self.job_id, self.order, self.operation_number, self.status, self.good, self.scrap, self.run_seconds
        )
    }

    /// Back from `key=value` lines; `None` if anything is missing or wrong.
    pub fn from_lines(text: &str) -> Option<Progress> {
        let get = |key: &str| {
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(key)
                        .and_then(|rest| rest.strip_prefix('='))
                })
                .map(str::to_string)
        };
        Some(Progress {
            job_id: get("job_id")?,
            order: get("order")?,
            operation_number: get("operation_number")?.parse().ok()?,
            status: get("status")?,
            good: get("good")?.parse().ok()?,
            scrap: get("scrap")?.parse().ok()?,
            run_seconds: get("run_seconds")?.parse().ok()?,
        })
    }
}

/// The `data` object of an `equipment-state/v1` message (ERP contract v1).
pub fn state_data(state: &str, job: Option<(&str, i64)>, since: &str) -> Value {
    let (order, operation) = match job {
        Some((order, operation)) => (Value::str(order), Value::int(operation)),
        None => (Value::Null, Value::Null),
    };
    Value::object(vec![
        ("state", Value::str(state)),
        ("order", order),
        ("operation_number", operation),
        ("since", Value::str(since)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_message(quantity: &str) -> Vec<u8> {
        format!(
            r#"{{"schema":"job/v1","event_id":null,"timestamp":"2026-06-01T08:00:00+00:00","source":"mes-intelligence",
               "data":{{"job_id":"mo00000001-0010","order":"MO00000001","operation_number":10,"quantity":"{quantity}",
               "unit":"EA","planned_activities":{{"MACH":"10.500"}}}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn reads_the_mes_job() {
        let job = parse_job(&job_message("100.000")).unwrap().unwrap();
        assert_eq!(
            job,
            Job {
                job_id: "mo00000001-0010".into(),
                order: "MO00000001".into(),
                operation_number: 10,
                target: 100
            }
        );
        assert_eq!(parse_job(&job_message("2.5")).unwrap().unwrap().target, 3); // half a part is a part
        assert_eq!(parse_job(b"").unwrap(), None);
    }

    #[test]
    fn refuses_bad_jobs() {
        assert!(parse_job(&job_message("-1")).is_err());
        assert!(parse_job(&job_message("1e3")).is_err());
        assert!(parse_job(&job_message("99999999999")).is_err());
        assert!(parse_job(br#"{"schema":"job/v9","data":{}}"#)
            .unwrap_err()
            .contains("unknown schema"));
        assert!(parse_job(br#"{"schema":"job/v1","data":{"job_id":"x"}}"#)
            .unwrap_err()
            .contains("missing quantity"));
        assert!(parse_job(b"not json").is_err());
        assert!(parse_job(&[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn quantities_and_hours() {
        assert_eq!(parts_for("100"), Some(100));
        assert_eq!(parts_for("100.000"), Some(100));
        assert_eq!(parts_for("0.001"), Some(1));
        assert_eq!(parts_for(".5"), None);
        assert_eq!(hours_text(37_800), "10.500");
        assert_eq!(hours_text(0), "0.000");
        assert_eq!(hours_text(1), "0.000"); // 0.28 thousandths rounds down
        assert_eq!(hours_text(2), "0.001"); // 0.56 thousandths rounds up
        assert_eq!(hours_text(u32::MAX), "1193046.471");
    }

    #[test]
    fn progress_survives_the_state_file() {
        let progress = Progress {
            job_id: "mo00000001-0010".into(),
            order: "MO00000001".into(),
            operation_number: 10,
            status: "COMPLETE".into(),
            good: 100,
            scrap: 2,
            run_seconds: 37_800,
        };
        assert_eq!(
            Progress::from_lines(&progress.to_lines()),
            Some(progress.clone())
        );
        assert_eq!(Progress::from_lines("job_id=x\n"), None);
        let data = progress.to_data();
        assert_eq!(
            data.get("run_hours").and_then(Value::as_str),
            Some("10.500")
        );
        assert_eq!(
            data.get("good_quantity").and_then(Value::as_str),
            Some("100")
        );
    }

    #[test]
    fn envelope_shape() {
        let bytes = envelope(JOB_PROGRESS, "mes-edge/press3", Value::object(vec![]));
        let message = json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(
            message.get("schema").and_then(Value::as_str),
            Some("job-progress/v1")
        );
        assert_eq!(message.get("event_id"), Some(&Value::Null));
        assert_eq!(
            message.get("source").and_then(Value::as_str),
            Some("mes-edge/press3")
        );
        let state = state_data("RUNNING", Some(("MO1", 10)), "t");
        assert_eq!(
            state.to_json(),
            r#"{"state":"RUNNING","order":"MO1","operation_number":10,"since":"t"}"#
        );
    }
}
