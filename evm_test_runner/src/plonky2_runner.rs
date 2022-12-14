//! Handles feeding the parsed tests into `plonky2` and determining the result.
//! Essentially converts parsed tests into test results.

use std::{cell::RefCell, fmt::Display, panic};

use backtrace::Backtrace;
use common::types::ParsedTest;
use ethereum_types::H256;
use indicatif::{ProgressBar, ProgressStyle};
use log::trace;
use plonky2::{
    field::goldilocks_field::GoldilocksField, plonk::config::KeccakGoldilocksConfig,
    util::timing::TimingTree,
};
use plonky2_evm::{all_stark::AllStark, config::StarkConfig, prover::prove};

use crate::test_dir_reading::{ParsedTestGroup, ParsedTestSubGroup, Test};

// Inspired by: https://stackoverflow.com/a/73711057
thread_local! {
    static BACKTRACE: RefCell<Option<Backtrace>> = RefCell::new(None);
}

#[derive(Clone, Debug)]
pub(crate) enum TestStatus {
    Passed,
    EvmErr(String),
    EvmPanic(String),
    IncorrectAccountFinalState(TrieFinalStateDiff),
}

impl Display for TestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestStatus::Passed => write!(f, "Passed"),
            TestStatus::EvmErr(err) => write!(f, "Evm error: {}", err),
            TestStatus::EvmPanic(panic) => write!(f, "Evm panic: {}", panic),
            TestStatus::IncorrectAccountFinalState(diff) => {
                write!(f, "Expected trie hash mismatch: {}", diff)
            }
        }
    }
}

/// If one or more trie hashes are different from the expected, then we return a
/// diff showing which tries where different.
#[derive(Clone, Debug)]
pub(crate) struct TrieFinalStateDiff {
    state: TrieComparisonResult,
    receipt: TrieComparisonResult,
    transaction: TrieComparisonResult,
}

/// A result of comparing the actual outputted `plonky2` trie to the one
/// expected by the test.
#[derive(Clone, Debug)]
enum TrieComparisonResult {
    Correct,
    Difference(H256, H256),
}

impl Display for TrieComparisonResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Correct => write!(f, "Correct"),
            Self::Difference(actual, expected) => {
                write!(f, "Difference (Actual: {}, Expected: {})", actual, expected)
            }
        }
    }
}

impl Display for TrieFinalStateDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "State: {}, Receipt: {}, Transaction: {}",
            self.state, self.receipt, self.transaction
        )
    }
}

impl TestStatus {
    pub(crate) fn passed(&self) -> bool {
        matches!(self, TestStatus::Passed)
    }
}

#[derive(Debug)]
pub(crate) struct TestGroupRunResults {
    pub(crate) name: String,
    pub(crate) sub_group_res: Vec<TestSubGroupRunResults>,
}

fn num_tests_in_groups<'a>(groups: impl Iterator<Item = &'a ParsedTestGroup> + 'a) -> u64 {
    groups
        .map(|g| {
            g.sub_groups
                .iter()
                .flat_map(|sub_g| sub_g.tests.iter())
                .count() as u64
        })
        .sum()
}

#[derive(Debug)]
pub(crate) struct TestSubGroupRunResults {
    pub(crate) name: String,
    pub(crate) test_res: Vec<TestRunResult>,
}

#[derive(Debug)]
pub(crate) struct TestRunResult {
    pub(crate) name: String,
    pub(crate) status: TestStatus,
}

pub(crate) fn run_plonky2_tests(parsed_tests: Vec<ParsedTestGroup>) -> Vec<TestGroupRunResults> {
    let num_tests = num_tests_in_groups(parsed_tests.iter());
    let mut prog_bar = ProgressBar::new(num_tests).with_style(
        ProgressStyle::with_template("ETA: [{eta_precise}] | Test: {msg}\n{wide_bar} {pos}/{len}")
            .unwrap(),
    );

    let orig_panic_hook = panic::take_hook();

    // When we catch panics from `plonky2`, they still print to `stderr` even though
    // they are captured. To avoid polluting `stderr`, we temporarily replace the
    // hook so that it captures the backtrace so we are able to include this in the
    // test output.
    panic::set_hook(Box::new(|_| {
        let trace = Backtrace::new();
        BACKTRACE.with(move |b| b.borrow_mut().replace(trace));
    }));

    let res = parsed_tests
        .into_iter()
        .map(|g| run_test_group(g, &mut prog_bar))
        .collect();
    panic::set_hook(orig_panic_hook);

    res
}

fn run_test_group(group: ParsedTestGroup, bar: &mut ProgressBar) -> TestGroupRunResults {
    TestGroupRunResults {
        name: group.name,
        sub_group_res: group
            .sub_groups
            .into_iter()
            .map(|sub_g| run_test_sub_group(sub_g, bar))
            .collect(),
    }
}

fn run_test_sub_group(
    sub_group: ParsedTestSubGroup,
    bar: &mut ProgressBar,
) -> TestSubGroupRunResults {
    TestSubGroupRunResults {
        name: sub_group.name,
        test_res: sub_group
            .tests
            .into_iter()
            .map(|sub_g| run_test(sub_g, bar))
            .collect(),
    }
}

fn run_test(test: Test, bar: &mut ProgressBar) -> TestRunResult {
    trace!("Running test {}...", test.name);

    bar.set_message(test.name.to_string());
    let res = run_test_and_get_test_result(test.info);
    bar.inc(1);

    TestRunResult {
        name: test.name,
        status: res,
    }
}

/// Run a test against `plonky2` and output a result based on what happens.
fn run_test_and_get_test_result(test: ParsedTest) -> TestStatus {
    let proof_run_res = panic::catch_unwind(|| {
        prove::<GoldilocksField, KeccakGoldilocksConfig, 2>(
            &AllStark::default(),
            &StarkConfig::standard_fast_config(),
            test.plonky2_inputs,
            &mut TimingTree::default(),
        )
    });

    let proof_run_output = match proof_run_res {
        Ok(Ok(res)) => res,
        Ok(Err(err)) => return TestStatus::EvmErr(err.to_string()),
        Err(err) => {
            let panic_str = match err.downcast::<String>() {
                Ok(panic_str) => *panic_str,
                Err(_) => "Unknown panic reason.".to_string(),
            };

            let panic_backtrace = BACKTRACE.with(|b| b.borrow_mut().take()).unwrap();
            let panic_with_backtrace_str =
                format!("panic: {}\nBacktrace: {:?}", panic_str, panic_backtrace);

            return TestStatus::EvmPanic(panic_with_backtrace_str);
        }
    };

    let actual_state_trie_hash = proof_run_output.public_values.trie_roots_after.state_root;

    if let Some(expected_state_trie_hash) = test.expected_final_account_states && actual_state_trie_hash != expected_state_trie_hash {
        let trie_diff = TrieFinalStateDiff {
            state: TrieComparisonResult::Difference(actual_state_trie_hash, expected_state_trie_hash),
            receipt: TrieComparisonResult::Correct, // TODO...
            transaction: TrieComparisonResult::Correct, // TODO...
        };

        return TestStatus::IncorrectAccountFinalState(trie_diff)
    }

    // TODO: Also check receipt and txn hashes once these are provided by the
    // parser...

    TestStatus::Passed
}
