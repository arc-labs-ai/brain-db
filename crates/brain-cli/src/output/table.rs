//! Two-column "key value" renderer for human-readable output.

/// Render a list of `(key, value)` pairs as a two-column table.
/// Keys are left-padded to a uniform width.
#[must_use]
pub fn render_kv(rows: &[(String, String)]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (k, v) in rows {
        // Format: "<key padded><gap><value>\n"
        out.push_str(&format!("{:<width$}  {v}\n", k, width = width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_aligns_columns() {
        let rows = vec![
            ("status".into(), "healthy".into()),
            ("admin_endpoint".into(), "127.0.0.1:9091".into()),
        ];
        let out = render_kv(&rows);
        // "admin_endpoint" is the widest key; "status" gets padded.
        assert!(out.contains("status          "));
        assert!(out.contains("admin_endpoint  127.0.0.1:9091"));
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(render_kv(&[]), "");
    }
}
