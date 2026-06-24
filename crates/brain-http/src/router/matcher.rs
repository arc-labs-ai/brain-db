//! Path matching primitives.
//!
//! The router supports two route kinds:
//!
//! - **Exact**: `(Method, &'static str)` matches when `req.uri().path()`
//!   equals the registered path verbatim.
//! - **Prefix**: `(Method, &'static str)` matches when `req.uri().path()`
//!   starts with the registered prefix.
//!
//! Match precedence: exact wins over prefix. Within each kind, the
//! first registered route wins (order of insertion).
//!
//! Path-param extraction is the handler's responsibility — pass a
//! prefix like `"/v1/snapshots/"` and parse the segment after the
//! prefix inside the handler. Mirrors the brain-server admin
//! dispatch pattern.

use http::Method;

/// Result of matching one `(method, path)` pair against the registered
/// routes. Indices into the corresponding handler list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchOutcome {
    /// An exact-match route fired; index into the `exact_handlers` vec.
    Exact(usize),
    /// A prefix-match route fired; index into the `prefix_handlers` vec.
    Prefix(usize),
    /// Path matched some route but with the wrong method. Triggers
    /// `405 Method Not Allowed` in the router.
    MethodMismatch,
    /// Nothing matched.
    None,
}

/// Spec of one exact-match route — stored in `Router` as a `Vec`.
#[derive(Debug, Clone)]
pub(crate) struct ExactSpec {
    pub method: Method,
    pub path: &'static str,
}

/// Spec of one prefix-match route.
#[derive(Debug, Clone)]
pub(crate) struct PrefixSpec {
    pub method: Method,
    pub prefix: &'static str,
}

pub(crate) fn match_route(
    exact: &[ExactSpec],
    prefix: &[PrefixSpec],
    method: &Method,
    path: &str,
) -> MatchOutcome {
    let mut path_matched_method_wrong = false;

    for (i, spec) in exact.iter().enumerate() {
        if spec.path == path {
            if spec.method == *method {
                return MatchOutcome::Exact(i);
            }
            path_matched_method_wrong = true;
        }
    }

    for (i, spec) in prefix.iter().enumerate() {
        if path.starts_with(spec.prefix) {
            if spec.method == *method {
                return MatchOutcome::Prefix(i);
            }
            path_matched_method_wrong = true;
        }
    }

    if path_matched_method_wrong {
        MatchOutcome::MethodMismatch
    } else {
        MatchOutcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(method: Method, path: &'static str) -> ExactSpec {
        ExactSpec { method, path }
    }

    fn prefix(method: Method, prefix: &'static str) -> PrefixSpec {
        PrefixSpec { method, prefix }
    }

    #[test]
    fn exact_route_matches_identical_method_and_path() {
        let e = vec![exact(Method::GET, "/healthz")];
        assert_eq!(
            match_route(&e, &[], &Method::GET, "/healthz"),
            MatchOutcome::Exact(0)
        );
    }

    #[test]
    fn exact_wins_over_prefix() {
        let e = vec![exact(Method::GET, "/v1/x")];
        let p = vec![prefix(Method::GET, "/v1/")];
        assert_eq!(
            match_route(&e, &p, &Method::GET, "/v1/x"),
            MatchOutcome::Exact(0)
        );
    }

    #[test]
    fn prefix_match_after_exact_miss() {
        let e: Vec<ExactSpec> = vec![];
        let p = vec![prefix(Method::POST, "/v1/snapshots")];
        assert_eq!(
            match_route(&e, &p, &Method::POST, "/v1/snapshots/abc/delete"),
            MatchOutcome::Prefix(0)
        );
    }

    #[test]
    fn first_prefix_wins() {
        let p = vec![
            prefix(Method::GET, "/v1/a"),
            prefix(Method::GET, "/v1/abc"), // never matches because /v1/a fires first
        ];
        assert_eq!(
            match_route(&[], &p, &Method::GET, "/v1/abc"),
            MatchOutcome::Prefix(0)
        );
    }

    #[test]
    fn method_mismatch_on_exact() {
        let e = vec![exact(Method::POST, "/v1/snap")];
        assert_eq!(
            match_route(&e, &[], &Method::GET, "/v1/snap"),
            MatchOutcome::MethodMismatch
        );
    }

    #[test]
    fn method_mismatch_on_prefix() {
        let p = vec![prefix(Method::POST, "/v1/snap")];
        assert_eq!(
            match_route(&[], &p, &Method::GET, "/v1/snap/x"),
            MatchOutcome::MethodMismatch
        );
    }

    #[test]
    fn no_match_returns_none() {
        let e = vec![exact(Method::GET, "/healthz")];
        let p = vec![prefix(Method::GET, "/v1/snap")];
        assert_eq!(
            match_route(&e, &p, &Method::GET, "/totally-fake"),
            MatchOutcome::None
        );
    }

    #[test]
    fn case_sensitive_path() {
        let e = vec![exact(Method::GET, "/Healthz")];
        // /healthz lowercase must NOT match /Healthz.
        assert_eq!(
            match_route(&e, &[], &Method::GET, "/healthz"),
            MatchOutcome::None
        );
    }
}
