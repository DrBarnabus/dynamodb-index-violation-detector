//! Violation drill-in: compare an item re-fetched by key with the item the scan
//! saw.
//!
//! The rolling window keeps only each item's key and a [`fingerprint`] of its
//! attributes, so the detail view can tell whether the item has since been
//! deleted or changed, and re-run the rules to tell whether the violation still
//! holds.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::domain::{AttributeValue, Item, KeyAttribute};
use crate::rules::{RuleSet, Violation, check_item};
use crate::state::RecentViolation;

/// A hash of an item's attributes, independent of map and set ordering.
pub fn fingerprint(item: &Item) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_map(item, &mut hasher);
    hasher.finish()
}

/// The primary key of the item keyed by `pk` and `sk`, for a `GetItem`.
pub fn primary_key(pk: &KeyAttribute, sk: Option<&KeyAttribute>) -> Item {
    pk_and_sk(pk, sk)
        .map(|key| (key.name.clone(), key.value.clone()))
        .collect()
}

fn pk_and_sk<'a>(
    pk: &'a KeyAttribute,
    sk: Option<&'a KeyAttribute>,
) -> impl Iterator<Item = &'a KeyAttribute> {
    std::iter::once(pk).chain(sk)
}

/// The current state of a violating item, re-fetched after the scan.
#[derive(Debug, Clone, PartialEq)]
pub enum Inspection {
    /// The item has been deleted since the scan.
    Gone,
    Present {
        item: Item,
        /// The item's attributes differ from those the scan saw.
        changed: bool,
        /// Re-running the rules on the current item still reports the violation.
        still_violating: bool,
    },
}

/// Compare `current`, the item re-fetched by `recent`'s key, against what the
/// scan saw, re-checking it against `rules` at `now_epoch_secs`.
pub fn inspect(
    recent: &RecentViolation,
    current: Option<Item>,
    rules: &RuleSet,
    now_epoch_secs: i64,
) -> Inspection {
    let Some(item) = current else {
        return Inspection::Gone;
    };

    let still_violating = check_item(&item, rules, now_epoch_secs)
        .iter()
        .any(|violation| same_violation(violation, &recent.violation));
    Inspection::Present {
        changed: fingerprint(&item) != recent.fingerprint,
        item,
        still_violating,
    }
}

/// The same rule broken on the same attribute, whatever the offending value.
fn same_violation(a: &Violation, b: &Violation) -> bool {
    a.target == b.target && a.category == b.category && a.attribute == b.attribute
}

fn hash_map<H: Hasher>(map: &Item, hasher: &mut H) {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_unstable_by_key(|(name, _)| *name);
    entries.len().hash(hasher);
    for (name, value) in entries {
        name.hash(hasher);
        hash_value(value, hasher);
    }
}

fn hash_value<H: Hasher>(value: &AttributeValue, hasher: &mut H) {
    std::mem::discriminant(value).hash(hasher);
    match value {
        AttributeValue::S(s) | AttributeValue::N(s) => s.hash(hasher),
        AttributeValue::B(b) => b.hash(hasher),
        AttributeValue::Bool(b) | AttributeValue::Null(b) => b.hash(hasher),
        AttributeValue::M(m) => hash_map(m, hasher),
        AttributeValue::L(l) => {
            l.len().hash(hasher);
            for element in l {
                hash_value(element, hasher);
            }
        }
        AttributeValue::Ss(s) | AttributeValue::Ns(s) => hash_set(s, hasher),
        AttributeValue::Bs(b) => hash_set(b, hasher),
    }
}

fn hash_set<T: Hash + Ord, H: Hasher>(set: &[T], hasher: &mut H) {
    let mut members: Vec<_> = set.iter().collect();
    members.sort_unstable();
    members.hash(hasher);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{KeySchemaElement, TypeCode};
    use crate::rules::{GsiRule, Target, ViolationCategory};

    fn s(value: &str) -> AttributeValue {
        AttributeValue::S(value.to_string())
    }

    fn item(pairs: &[(&str, AttributeValue)]) -> Item {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    fn key(name: &str, value: AttributeValue) -> KeyAttribute {
        KeyAttribute {
            name: name.to_string(),
            value,
        }
    }

    fn rules() -> RuleSet {
        RuleSet {
            table: "users".to_string(),
            gsis: vec![GsiRule {
                name: "byEmail".to_string(),
                hypothetical: true,
                pk: KeySchemaElement {
                    name: "email".to_string(),
                    type_code: TypeCode::S,
                },
                sk: None,
                check_missing: false,
            }],
            lsis: Vec::new(),
            ttl: None,
        }
    }

    fn type_mismatch_on_email(scanned: &Item) -> RecentViolation {
        RecentViolation {
            pk: key("id", s("u-1")),
            sk: None,
            fingerprint: fingerprint(scanned),
            violation: Violation {
                target: Target::Gsi("byEmail".to_string()),
                category: ViolationCategory::TypeMismatch,
                attribute: Some("email".to_string()),
                actual_value: Some("7".to_string()),
                actual_type: Some("N".to_string()),
                expected_type: Some(TypeCode::S),
                size_bytes: None,
            },
        }
    }

    #[test]
    fn fingerprint_ignores_set_member_order() {
        let a = item(&[(
            "tags",
            AttributeValue::Ss(vec!["x".to_string(), "y".to_string()]),
        )]);
        let b = item(&[(
            "tags",
            AttributeValue::Ss(vec!["y".to_string(), "x".to_string()]),
        )]);

        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_distinguishes_value_and_type_changes() {
        let base = item(&[("id", s("u-1")), ("n", AttributeValue::N("1".to_string()))]);
        let edited = item(&[("id", s("u-1")), ("n", AttributeValue::N("2".to_string()))]);
        let retyped = item(&[("id", s("u-1")), ("n", s("1"))]);
        let nested = item(&[("id", s("u-1")), ("n", AttributeValue::L(vec![s("1")]))]);

        let fingerprints = [&base, &edited, &retyped, &nested].map(fingerprint);
        for (i, a) in fingerprints.iter().enumerate() {
            for b in &fingerprints[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn primary_key_holds_partition_and_sort_key() {
        let pk = key("id", s("u-1"));
        let sk = key("ts", AttributeValue::N("42".to_string()));

        assert_eq!(primary_key(&pk, None), item(&[("id", s("u-1"))]));
        assert_eq!(
            primary_key(&pk, Some(&sk)),
            item(&[
                ("id", s("u-1")),
                ("ts", AttributeValue::N("42".to_string()))
            ])
        );
    }

    #[test]
    fn missing_item_is_gone() {
        let scanned = item(&[
            ("id", s("u-1")),
            ("email", AttributeValue::N("7".to_string())),
        ]);
        let recent = type_mismatch_on_email(&scanned);

        assert_eq!(inspect(&recent, None, &rules(), 0), Inspection::Gone);
    }

    #[test]
    fn unchanged_item_still_violates() {
        let scanned = item(&[
            ("id", s("u-1")),
            ("email", AttributeValue::N("7".to_string())),
        ]);
        let recent = type_mismatch_on_email(&scanned);

        assert_eq!(
            inspect(&recent, Some(scanned.clone()), &rules(), 0),
            Inspection::Present {
                item: scanned,
                changed: false,
                still_violating: true,
            }
        );
    }

    #[test]
    fn changed_item_can_still_violate_with_another_value() {
        let scanned = item(&[
            ("id", s("u-1")),
            ("email", AttributeValue::N("7".to_string())),
        ]);
        let current = item(&[
            ("id", s("u-1")),
            ("email", AttributeValue::N("8".to_string())),
        ]);
        let recent = type_mismatch_on_email(&scanned);

        let Inspection::Present {
            changed,
            still_violating,
            ..
        } = inspect(&recent, Some(current), &rules(), 0)
        else {
            panic!("expected the item to be present");
        };
        assert!(changed);
        assert!(still_violating);
    }

    #[test]
    fn fixed_item_no_longer_violates() {
        let scanned = item(&[
            ("id", s("u-1")),
            ("email", AttributeValue::N("7".to_string())),
        ]);
        let current = item(&[("id", s("u-1")), ("email", s("a@example.com"))]);
        let recent = type_mismatch_on_email(&scanned);

        let Inspection::Present {
            changed,
            still_violating,
            ..
        } = inspect(&recent, Some(current), &rules(), 0)
        else {
            panic!("expected the item to be present");
        };
        assert!(changed);
        assert!(!still_violating);
    }
}
