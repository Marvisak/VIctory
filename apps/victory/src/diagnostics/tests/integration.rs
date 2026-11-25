use super::super::{compare, copy, coverage, render};

#[test]
fn full_synthetic_comparison_reports_window_statistics() {
    let a = coverage::parse(include_str!("../fixtures/census-baseline.txt")).unwrap();
    let b = coverage::parse(include_str!("../fixtures/census-improved.txt")).unwrap();
    let gpu = (
        copy::parse(include_str!("../fixtures/copy-baseline.txt")).unwrap(),
        copy::parse(include_str!("../fixtures/copy-candidate.txt")).unwrap(),
    );
    let report = compare::build(&a, &b, Some(&gpu), Some(0.0));
    assert!(report.passed());
    let output = render::text(&report);
    assert!(output.contains("GPU copy windows: 3 -> 3"));
    assert!(output.contains("title copy MB/frame"));
    assert!(output.contains("-50.00%"));
    assert!(output.contains("not individual frame times"));
    assert!(output.contains("Result: PASS"));
}

#[test]
fn translator_summary_round_trips_through_report_parser() {
    use coverage::{Counts, Stage};

    let rows = [[8, 10, 7, 6], [3, 5, 2, 1], [0, 0, 0, 0]];
    let output = crate::translate::dxil_summary(rows).join("\n");
    let parsed = coverage::parse(&output).unwrap();
    for (stage, [emitted, total, validated, pipelines]) in Stage::ALL.into_iter().zip(rows) {
        assert_eq!(
            parsed.get(stage),
            Counts {
                emitted: emitted as u64,
                total: total as u64,
                validated: validated as u64,
                pipelines: pipelines as u64,
            }
        );
    }
}

#[test]
fn gpu_summary_contract_matches_report_parser() {
    use copy::Metric;

    let line = include_str!("../../../../../crates/victory-gpu/tests/copystat-summary.txt");
    let parsed = copy::parse(line).unwrap();
    assert_eq!(parsed.len(), 1);
    for (metric, expected) in Metric::ALL.into_iter().zip([2.5, 20.0, 10.0, 4.0, 500.0, 2.0]) {
        assert_eq!(parsed.values(metric), [expected]);
    }
}
