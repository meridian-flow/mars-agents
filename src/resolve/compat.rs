use super::VersionConstraint;

/// Result of comparing two version constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityResult {
    /// Both constraints resolve to the same version.
    Compatible,
    /// Constraints might resolve differently (Latest vs pinned).
    PotentiallyConflicting,
    /// Constraints cannot resolve to the same version.
    Conflicting,
}

impl VersionConstraint {
    /// Check if this constraint is compatible with another.
    ///
    /// Matrix:
    /// - Latest + Latest => Compatible
    /// - Semver(same) + Semver(same) => Compatible
    /// - RefPin(same) + RefPin(same) => Compatible
    /// - Latest + Semver/RefPin => PotentiallyConflicting
    /// - Different Semver/RefPin => Conflicting
    /// - Semver + RefPin => Conflicting
    pub fn compatible_with(&self, other: &VersionConstraint) -> CompatibilityResult {
        use CompatibilityResult::{Compatible, Conflicting, PotentiallyConflicting};
        use VersionConstraint::{Latest, RefPin, Semver};

        match (self, other) {
            (Latest, Latest) => Compatible,
            (Latest, Semver(_) | RefPin(_)) | (Semver(_) | RefPin(_), Latest) => {
                PotentiallyConflicting
            }
            (Semver(lhs), Semver(rhs)) => {
                if lhs == rhs {
                    Compatible
                } else {
                    Conflicting
                }
            }
            (RefPin(lhs), RefPin(rhs)) => {
                if lhs == rhs {
                    Compatible
                } else {
                    Conflicting
                }
            }
            (Semver(_), RefPin(_)) | (RefPin(_), Semver(_)) => Conflicting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CompatibilityResult;
    use crate::resolve::VersionConstraint;

    fn semver(req: &str) -> VersionConstraint {
        VersionConstraint::Semver(req.parse().expect("valid semver requirement"))
    }

    #[test]
    fn latest_with_latest_is_compatible() {
        assert_eq!(
            VersionConstraint::Latest.compatible_with(&VersionConstraint::Latest),
            CompatibilityResult::Compatible
        );
    }

    #[test]
    fn same_semver_is_compatible() {
        assert_eq!(
            semver("^1.2").compatible_with(&semver("^1.2")),
            CompatibilityResult::Compatible
        );
    }

    #[test]
    fn same_ref_pin_is_compatible() {
        assert_eq!(
            VersionConstraint::RefPin("main".into())
                .compatible_with(&VersionConstraint::RefPin("main".into())),
            CompatibilityResult::Compatible
        );
    }

    #[test]
    fn latest_with_semver_is_potentially_conflicting() {
        assert_eq!(
            VersionConstraint::Latest.compatible_with(&semver(">=1.0.0")),
            CompatibilityResult::PotentiallyConflicting
        );
    }

    #[test]
    fn latest_with_ref_pin_is_potentially_conflicting() {
        assert_eq!(
            VersionConstraint::Latest
                .compatible_with(&VersionConstraint::RefPin("main".into())),
            CompatibilityResult::PotentiallyConflicting
        );
    }

    #[test]
    fn different_semver_is_conflicting() {
        assert_eq!(
            semver("^1.0").compatible_with(&semver("^2.0")),
            CompatibilityResult::Conflicting
        );
    }

    #[test]
    fn different_ref_pin_is_conflicting() {
        assert_eq!(
            VersionConstraint::RefPin("main".into())
                .compatible_with(&VersionConstraint::RefPin("release".into())),
            CompatibilityResult::Conflicting
        );
    }

    #[test]
    fn semver_with_ref_pin_is_conflicting() {
        assert_eq!(
            semver("^1.0").compatible_with(&VersionConstraint::RefPin("main".into())),
            CompatibilityResult::Conflicting
        );
    }
}
