use monty::RunProgress;
use serde_json::json;

use crate::runner::execute;

#[test]
fn top_level_await_and_positional_arguments_work() {
    let outcome = execute("value = await read('src/alpha.rs', 2, 1)\nvalue");
    assert_eq!(outcome.failure, None, "{}", outcome.detail);
    assert_eq!(outcome.value, Some(json!("2:     println!(\"alpha\");\n")));
    assert_eq!(
        outcome.calls[0].args,
        vec![json!("src/alpha.rs"), json!(2), json!(1)]
    );
}

#[test]
fn asyncio_gather_surfaces_one_failure_without_losing_the_sibling_call() {
    let outcome = execute(
        "import asyncio\ntry:\n    values = await asyncio.gather(read_text('src/alpha.rs'), read_text('missing'))\nexcept RuntimeError as error:\n    values = str(error)\nvalues",
    );
    assert_eq!(outcome.failure, None, "{}", outcome.detail);
    assert!(
        outcome
            .value
            .unwrap()
            .as_str()
            .unwrap()
            .contains("missing not found")
    );
    assert_eq!(outcome.calls.len(), 2);
    assert_eq!(outcome.calls[1].name, "read_text");
}

#[test]
fn typed_dict_results_support_subscripting() {
    let outcome = execute("matches = await grep('alpha')\nmatches[0]['path']");
    assert_eq!(outcome.failure, None, "{}", outcome.detail);
    assert_eq!(outcome.value, Some(json!("src/alpha.rs")));
}

#[test]
fn missing_await_is_a_runtime_error_without_type_checking() {
    let outcome = execute("await read_text('src/alpha.rs').strip()");
    assert_eq!(outcome.failure.as_deref(), Some("runtime"));
    assert!(outcome.calls.is_empty());
    assert!(
        outcome.detail.contains("main.py\", line 1"),
        "{}",
        outcome.detail
    );
}

#[test]
fn run_progress_can_cross_an_async_worker_boundary() {
    fn assert_send<T: Send>() {}
    assert_send::<RunProgress>();
}
