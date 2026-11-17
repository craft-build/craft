mod convert;
mod probes;
mod process;
mod runner;
mod stubs;

#[test]
fn corpus_meets_phase_zero_gate() {
    let report = runner::run_corpus();
    let regular_rate = 100.0 * report.regular_passed as f64 / report.regular_total as f64;
    println!(
        "codemode phase 0: {}/{} regular ({regular_rate:.1}%), {}/{} adversarial; origins {:?}",
        report.regular_passed,
        report.regular_total,
        report.adversarial_passed,
        report.adversarial_total,
        report.origins,
    );
    for failure in &report.failures {
        eprintln!("{failure}");
    }
    assert!(
        regular_rate >= 90.0,
        "regular corpus pass rate {regular_rate:.1}% is below 90% ({} failures)",
        report.failures.len(),
    );
    assert_eq!(
        report.adversarial_passed, report.adversarial_total,
        "every adversarial case must fail with its expected class",
    );
}
