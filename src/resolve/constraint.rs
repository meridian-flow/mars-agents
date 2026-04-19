use semver::{Version, VersionReq};

use super::VersionConstraint;

/// Parse a version string into a constraint.
///
/// - `None` / `"latest"` → Latest (any version, newest wins)
/// - `"v1.2.3"` → exact match
/// - `"v2"` → `>=2.0.0, <3.0.0` (major range)
/// - `"v2.1"` → `>=2.1.0, <2.2.0` (minor range)
/// - `">=0.5.0"`, `"^2.0"`, `"~1.2"` → semver requirement
/// - anything else → branch/commit ref pin
pub fn parse_version_constraint(version: Option<&str>) -> VersionConstraint {
    let version = match version {
        None => return VersionConstraint::Latest,
        Some(v) => v.trim(),
    };

    if version.is_empty() || version.eq_ignore_ascii_case("latest") {
        return VersionConstraint::Latest;
    }

    // Try "v"-prefixed versions: v1.2.3, v2, v2.1
    if let Some(stripped) = version.strip_prefix('v') {
        // Try exact semver: v1.2.3
        if let Ok(ver) = Version::parse(stripped) {
            let req = VersionReq::parse(&format!("={ver}")).expect("valid exact req");
            return VersionConstraint::Semver(req);
        }

        // Try major-only: v2 → >=2.0.0, <3.0.0
        if let Ok(major) = stripped.parse::<u64>() {
            let req = VersionReq::parse(&format!(">={major}.0.0, <{}.0.0", major + 1))
                .expect("valid major range req");
            return VersionConstraint::Semver(req);
        }

        // Try major.minor: v2.1 → >=2.1.0, <2.2.0
        let parts: Vec<&str> = stripped.split('.').collect();
        if parts.len() == 2
            && let (Ok(major), Ok(minor)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>())
        {
            let req = VersionReq::parse(&format!(">={major}.{minor}.0, <{major}.{}.0", minor + 1))
                .expect("valid minor range req");
            return VersionConstraint::Semver(req);
        }
    }

    // Try as semver requirement directly (>=0.5.0, ^2.0, ~1.2, =1.0.0, etc.)
    if let Ok(req) = VersionReq::parse(version) {
        return VersionConstraint::Semver(req);
    }

    // Otherwise it's a branch or commit ref pin
    VersionConstraint::RefPin(version.to_string())
}
