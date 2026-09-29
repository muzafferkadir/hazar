//! Structural checks on the fixture matrix: a case that cannot run is a bug in
//! the harness, not a capability gap, so it must fail loudly here.

use hazar_testmatrix::fixtures::{build, Expectation};

#[test]
fn every_case_has_a_page_and_expected_output() {
    let fixtures = build();
    let routes: Vec<&str> = fixtures
        .routes
        .iter()
        .map(|route| route.path.as_str())
        .collect();

    assert!(fixtures.cases.len() >= 25, "matrix shrank unexpectedly");

    for case in &fixtures.cases {
        assert!(
            routes.contains(&case.page_path.as_str()),
            "{}: page route {} is missing",
            case.name,
            case.page_path
        );
        match case.expectation {
            Expectation::Download { .. } => assert!(
                !case.expected_bytes.is_empty(),
                "{}: download case needs expected bytes",
                case.name
            ),
            Expectation::DetectOnly { .. } => assert!(
                case.expected_bytes.is_empty(),
                "{}: detect-only case should not compare bytes",
                case.name
            ),
            Expectation::Refused { contains } => {
                assert!(!contains.is_empty(), "{}: refusal needs a reason", case.name);
            }
        }
    }
}

#[test]
fn route_paths_are_unique() {
    let fixtures = build();
    let mut seen = std::collections::HashSet::new();
    for route in &fixtures.routes {
        assert!(
            seen.insert(route.path.clone()),
            "duplicate route {}",
            route.path
        );
    }
}

#[test]
fn matrix_covers_every_resolver_strategy() {
    let fixtures = build();
    let strategies: std::collections::HashSet<&str> = fixtures
        .cases
        .iter()
        .map(|case| case.strategy)
        .collect();
    for wanted in [
        "attribute-scan",
        "player-config",
        "json-ld",
        "js-unpack",
        "base64-decode",
        "manifest-enum",
        "segment-group",
        "iframe",
    ] {
        assert!(
            strategies.contains(wanted),
            "no fixture exercises the {wanted} strategy"
        );
    }
}

/// Every served body is fed through the resolvers' text strategies. A panic
/// here (UTF-8 boundary, index arithmetic) is a bug in the engine, not a
/// capability gap, so it must fail loudly.
#[test]
fn scanning_every_fixture_body_does_not_panic() {
    use hazar_engine::resolve::{decode_escapes, find_bare_urls, find_script_srcs, scan_body};
    use hazar_engine::ResolveReport;

    let fixtures = build();
    let mut scanned = 0usize;

    for route in &fixtures.routes {
        let interesting = route.content_type.starts_with("text/html")
            || route.content_type.contains("javascript")
            || route.content_type.contains("mpegurl")
            || route.content_type.contains("dash+xml");
        if !interesting || route.body.is_empty() {
            continue;
        }
        let body = String::from_utf8_lossy(&route.body).to_string();
        let mut report = ResolveReport {
            page_url: "http://fixture.test/page.html".to_string(),
            candidates: Vec::new(),
            visited: Vec::new(),
            errors: Vec::new(),
        };

        let decoded = decode_escapes(&body);
        assert!(!decoded.is_empty() || body.is_empty());
        scan_body("http://fixture.test/page.html", &body, &mut report);
        let _ = find_bare_urls(&body);
        let _ = find_script_srcs(&body);
        scanned += 1;
    }

    assert!(scanned >= 20, "expected to scan every page body, got {scanned}");
}
