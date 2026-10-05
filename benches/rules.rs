use std::hint::black_box;

use criterion::{Bencher, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dynamodb_violation_detector::domain::{AttributeValue, Item, KeySchemaElement, TypeCode};
use dynamodb_violation_detector::rules::{GsiRule, LsiRule, RuleSet, TtlRule, check_item};

const BATCH: usize = 1_000;
const NOW_EPOCH_SECS: i64 = 1_790_000_000;

fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
    KeySchemaElement {
        name: name.to_string(),
        type_code,
    }
}

fn gsi_rule(index: usize) -> GsiRule {
    GsiRule {
        name: format!("GSI{index}"),
        hypothetical: false,
        pk: element(&format!("gsi{index}_pk"), TypeCode::S),
        sk: Some(element(&format!("gsi{index}_sk"), TypeCode::N)),
        check_missing: true,
    }
}

fn rule_set(gsi_count: usize) -> RuleSet {
    RuleSet {
        table: "orders".to_string(),
        gsis: (0..gsi_count).map(gsi_rule).collect(),
        lsis: vec![LsiRule {
            name: "LSI0".to_string(),
            sort_key: element("lsi_sk", TypeCode::S),
            check_missing: true,
        }],
        ttl: Some(TtlRule {
            attribute: "expires_at".to_string(),
            check_missing: true,
            check_wrong_type: true,
            check_ms_magnitude: true,
            check_malformed: true,
            check_past_5_years: true,
        }),
    }
}

fn clean_item(row: usize, gsi_count: usize) -> Item {
    let mut item = Item::new();
    item.insert("id".to_string(), AttributeValue::S(format!("order#{row}")));
    item.insert("sk".to_string(), AttributeValue::S("meta".to_string()));
    item.insert(
        "lsi_sk".to_string(),
        AttributeValue::S(format!("2026-10-{:02}", row % 28 + 1)),
    );
    item.insert(
        "expires_at".to_string(),
        AttributeValue::N((NOW_EPOCH_SECS + 86_400).to_string()),
    );
    item.insert("payload".to_string(), AttributeValue::S("x".repeat(256)));
    for index in 0..gsi_count {
        item.insert(
            format!("gsi{index}_pk"),
            AttributeValue::S(format!("customer#{}", row % 97)),
        );
        item.insert(format!("gsi{index}_sk"), AttributeValue::N(row.to_string()));
    }

    item
}

/// Each item breaks one rule, cycling through every violation category.
fn violating_item(row: usize, gsi_count: usize) -> Item {
    let mut item = clean_item(row, gsi_count);
    match row % 8 {
        0 => item.insert("gsi0_pk".to_string(), AttributeValue::N("42".to_string())),
        1 => item.insert("gsi0_sk".to_string(), AttributeValue::S("x".repeat(1_100))),
        2 => item.remove("gsi0_pk"),
        3 => item.remove("lsi_sk"),
        4 => item.remove("expires_at"),
        5 => item.insert(
            "expires_at".to_string(),
            AttributeValue::S("tomorrow".to_string()),
        ),
        6 => item.insert(
            "expires_at".to_string(),
            AttributeValue::N((NOW_EPOCH_SECS * 1_000).to_string()),
        ),
        _ => item.insert(
            "expires_at".to_string(),
            AttributeValue::N("1.5e9".to_string()),
        ),
    };

    item
}

fn check_batch(b: &mut Bencher, items: &[Item], rules: &RuleSet) {
    b.iter(|| {
        for item in items {
            black_box(check_item(black_box(item), rules, NOW_EPOCH_SECS));
        }
    });
}

fn bench_check_item(c: &mut Criterion) {
    let rules = rule_set(2);
    let clean: Vec<Item> = (0..BATCH).map(|row| clean_item(row, 2)).collect();
    let violating: Vec<Item> = (0..BATCH).map(|row| violating_item(row, 2)).collect();

    let mut group = c.benchmark_group("check_item");
    group.throughput(Throughput::Elements(BATCH as u64));
    for (label, items) in [("clean", &clean), ("violating", &violating)] {
        group.bench_with_input(BenchmarkId::from_parameter(label), items, |b, items| {
            check_batch(b, items, &rules)
        });
    }
    group.finish();
}

fn bench_gsi_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("check_item_by_gsi_count");
    group.throughput(Throughput::Elements(BATCH as u64));
    for gsi_count in [1, 5, 20] {
        let rules = rule_set(gsi_count);
        let items: Vec<Item> = (0..BATCH).map(|row| clean_item(row, gsi_count)).collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(gsi_count),
            &items,
            |b, items| check_batch(b, items, &rules),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_check_item, bench_gsi_count);
criterion_main!(benches);
