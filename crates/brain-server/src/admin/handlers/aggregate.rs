//! `GET /v1/aggregates` — operator view of the aggregate counters across
//! every shard and tenant.
//!
//! `MEMORY_AGGREGATE` on the wire is always pinned to the caller's own
//! `(namespace, space)`. This admin route is the cross-tenant view: it fans
//! out to every shard, sums each shard's counters, and merges.
//!
//! Query parameters (all optional, comma-separated lists allowed):
//! - `namespace=<u32>` — one namespace id
//! - `space=<32 hex chars | uuid>` — one space id
//! - `kind=episodic,semantic,consolidated` (or `0,1,2`)
//! - `status=active,tombstoned,hard_forgotten` (or `0,1,2`); default active
//! - `group_by=namespace,space,kind,status`
//!
//! Live and exact (counters, not the snapshot). 503 while any shard is
//! still building its counters after an upgrade.

use std::collections::BTreeMap;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_metadata::tables::agg::{AggFilter, AggGroup, AggGroupBy, AggTotal};
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

const KINDS: [&str; 3] = ["episodic", "semantic", "consolidated"];
const STATUSES: [&str; 3] = ["active", "tombstoned", "hard_forgotten"];

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let (filter, group_by) = match parse(req.uri().query().unwrap_or("")) {
        Ok(p) => p,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    let mut merged: BTreeMap<AggGroup, AggTotal> = BTreeMap::new();
    for shard in state.shards.iter() {
        match shard.aggregate_counters(filter.clone(), group_by).await {
            Ok(Some(totals)) => {
                for (g, a) in totals {
                    let m = merged.entry(g).or_default();
                    m.count += a.count;
                    m.salience_sum += a.salience_sum;
                    m.access_sum += a.access_sum;
                }
            }
            Ok(None) => {
                return Ok(text_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!(
                        "aggregate counters on shard {} are still being built; retry shortly\n",
                        shard.shard_id()
                    ),
                ))
            }
            Err(e) => {
                warn!(shard = shard.shard_id(), error = %e, "aggregate counters read failed");
                return Ok(text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "aggregate counters read failed\n",
                ));
            }
        }
    }
    let ungrouped = !(group_by.namespace || group_by.space || group_by.kind || group_by.status);
    if ungrouped && merged.is_empty() {
        merged.insert(AggGroup::default(), AggTotal::default());
    }
    Ok(json_response(StatusCode::OK, render(&merged)))
}

fn render(groups: &BTreeMap<AggGroup, AggTotal>) -> String {
    let rows: Vec<serde_json::Value> = groups
        .iter()
        .map(|(g, a)| {
            let mut o = serde_json::Map::new();
            if let Some(ns) = g.namespace_id {
                o.insert("namespace_id".into(), ns.into());
            }
            if let Some(sp) = g.space_id {
                o.insert(
                    "space_id".into(),
                    uuid::Uuid::from_bytes(sp).to_string().into(),
                );
            }
            if let Some(k) = g.kind {
                o.insert("kind".into(), name(&KINDS, k).into());
            }
            if let Some(s) = g.status {
                o.insert("status".into(), name(&STATUSES, s).into());
            }
            o.insert("count".into(), a.count.into());
            o.insert("salience_sum".into(), a.salience_sum.into());
            o.insert(
                "salience_avg".into(),
                a.avg_salience().map_or(serde_json::Value::Null, Into::into),
            );
            o.insert("access_count_sum".into(), a.access_sum.into());
            serde_json::Value::Object(o)
        })
        .collect();
    format!("{}\n", serde_json::json!({ "groups": rows }))
}

fn name(names: &[&'static str], b: u8) -> String {
    names
        .get(usize::from(b))
        .map_or_else(|| b.to_string(), |s| (*s).to_owned())
}

/// Parse one enum value by name or number.
fn enum_value(names: &[&str], v: &str, what: &str) -> Result<u8, String> {
    if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(v)) {
        return Ok(u8::try_from(i).unwrap_or(u8::MAX));
    }
    v.parse::<u8>()
        .ok()
        .filter(|b| usize::from(*b) < names.len())
        .ok_or_else(|| format!("unknown {what} {v:?}; expected one of {}", names.join(", ")))
}

fn parse(query: &str) -> Result<(AggFilter, AggGroupBy), String> {
    let mut filter = AggFilter::default();
    let mut group_by = AggGroupBy::default();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let values = v.split(',').filter(|x| !x.is_empty());
        match k {
            "namespace" => {
                filter.namespace_id = Some(
                    v.parse()
                        .map_err(|_| format!("namespace must be a u32 id, got {v:?}"))?,
                );
            }
            "space" => filter.space_id = Some(parse_space(v)?),
            "kind" => {
                for x in values {
                    filter.kinds.push(enum_value(&KINDS, x, "kind")?);
                }
            }
            "status" => {
                for x in values {
                    filter.statuses.push(enum_value(&STATUSES, x, "status")?);
                }
            }
            "group_by" => {
                for x in values {
                    match x {
                        "namespace" => group_by.namespace = true,
                        "space" => group_by.space = true,
                        "kind" => group_by.kind = true,
                        "status" => group_by.status = true,
                        other => {
                            return Err(format!(
                            "unknown group_by {other:?}; expected namespace, space, kind, status"
                        ))
                        }
                    }
                }
            }
            other => return Err(format!("unknown parameter {other:?}")),
        }
    }
    Ok((filter, group_by))
}

fn parse_space(v: &str) -> Result<[u8; 16], String> {
    uuid::Uuid::parse_str(v)
        .map(|u| *u.as_bytes())
        .map_err(|_| format!("space must be a uuid or 32 hex chars, got {v:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters_and_groupings() {
        let (f, g) = parse(
            "namespace=7&space=000102030405060708090a0b0c0d0e0f&kind=semantic,0\
             &status=active,2&group_by=kind,status",
        )
        .unwrap();
        assert_eq!(f.namespace_id, Some(7));
        assert_eq!(f.space_id.unwrap()[15], 0x0f);
        assert_eq!(f.kinds, vec![1, 0]);
        assert_eq!(f.statuses, vec![0, 2]);
        assert!(g.kind && g.status && !g.namespace && !g.space);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("kind=fruit").is_err());
        assert!(parse("status=9").is_err());
        assert!(parse("group_by=color").is_err());
        assert!(parse("namespace=-1").is_err());
        assert!(parse("space=nope").is_err());
        assert!(parse("bogus=1").is_err());
    }

    #[test]
    fn renders_names_and_null_avg() {
        let mut m = BTreeMap::new();
        m.insert(
            AggGroup {
                kind: Some(1),
                status: Some(0),
                ..AggGroup::default()
            },
            AggTotal {
                count: 2,
                salience_sum: 1.0,
                access_sum: 3,
            },
        );
        m.insert(AggGroup::default(), AggTotal::default());
        let out = render(&m);
        assert!(out.contains("\"kind\":\"semantic\""), "{out}");
        assert!(out.contains("\"status\":\"active\""), "{out}");
        assert!(out.contains("\"salience_avg\":0.5"), "{out}");
        assert!(out.contains("\"salience_avg\":null"), "{out}");
    }
}
