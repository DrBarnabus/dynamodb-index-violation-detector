use std::hint::black_box;
use std::io::{self, Sink};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dynamodb_violation_detector::domain::{AttributeValue, Item, KeyAttribute, TypeCode};
use dynamodb_violation_detector::export::{CsvWriter, ExportWriter, FanOutWriter, NdjsonWriter};
use dynamodb_violation_detector::rules::{ItemViolations, Target, Violation, ViolationCategory};

const BATCH: usize = 1_000;

type WriterFactory = fn() -> Box<dyn ExportWriter>;

fn key(name: &str, value: AttributeValue) -> KeyAttribute {
    KeyAttribute {
        name: name.to_string(),
        value,
    }
}

fn violation(row: usize) -> Violation {
    match row % 3 {
        0 => Violation {
            target: Target::Gsi("GSI0".to_string()),
            category: ViolationCategory::TypeMismatch,
            attribute: Some("gsi0_pk".to_string()),
            actual_value: Some(row.to_string()),
            actual_type: Some("N".to_string()),
            expected_type: Some(TypeCode::S),
            size_bytes: None,
        },
        1 => Violation {
            target: Target::Gsi("GSI0".to_string()),
            category: ViolationCategory::SizeExceeded,
            attribute: Some("gsi0_sk".to_string()),
            actual_value: Some("x".repeat(1_100)),
            actual_type: Some("S".to_string()),
            expected_type: Some(TypeCode::S),
            size_bytes: Some(1_100),
        },
        _ => Violation {
            target: Target::Ttl,
            category: ViolationCategory::TtlMsMagnitude,
            attribute: Some("expires_at".to_string()),
            actual_value: Some("1790000000000".to_string()),
            actual_type: Some("N".to_string()),
            expected_type: None,
            size_bytes: None,
        },
    }
}

/// Alternates string and binary keys so both key renderings are exercised.
fn group(row: usize, violations_per_item: usize) -> ItemViolations {
    let pk = if row.is_multiple_of(2) {
        key("id", AttributeValue::S(format!("order#{row}")))
    } else {
        key("id", AttributeValue::B(row.to_be_bytes().to_vec()))
    };
    let mut item = Item::new();
    item.insert(pk.name.clone(), pk.value.clone());
    item.insert("payload".to_string(), AttributeValue::S("x".repeat(256)));

    ItemViolations {
        table: "orders".to_string(),
        pk,
        sk: Some(key("sk", AttributeValue::S("meta".to_string()))),
        item,
        violations: (row..row + violations_per_item).map(violation).collect(),
        detected_at: 1_790_000_000,
    }
}

fn csv_writer() -> Box<dyn ExportWriter> {
    Box::new(CsvWriter::new(io::sink()).expect("writing a CSV header to io::sink cannot fail"))
}

fn ndjson_writer() -> Box<dyn ExportWriter> {
    Box::new(NdjsonWriter::<Sink>::new(io::sink()))
}

fn fan_out_writer() -> Box<dyn ExportWriter> {
    Box::new(FanOutWriter::new(vec![csv_writer(), ndjson_writer()]))
}

fn bench_writers(c: &mut Criterion) {
    let writers: [(&str, WriterFactory); 3] = [
        ("csv", csv_writer),
        ("ndjson", ndjson_writer),
        ("fan_out", fan_out_writer),
    ];

    for violations_per_item in [1, 3] {
        let groups: Vec<ItemViolations> = (0..BATCH)
            .map(|row| group(row, violations_per_item))
            .collect();
        let mut bench_group = c.benchmark_group(format!("export_{violations_per_item}_per_item"));
        bench_group.throughput(Throughput::Elements(BATCH as u64));
        for (label, make_writer) in writers {
            bench_group.bench_with_input(
                BenchmarkId::from_parameter(label),
                &groups,
                |b, groups| {
                    let mut writer = make_writer();
                    b.iter(|| {
                        for group in groups {
                            writer
                                .write(black_box(group))
                                .expect("writing to io::sink cannot fail");
                        }
                    });
                },
            );
        }
        bench_group.finish();
    }
}

criterion_group!(benches, bench_writers);
criterion_main!(benches);
