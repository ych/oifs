#!/usr/bin/env bash
# ==============================================================================
# OIFS Pre-Release Verification Script
# ==============================================================================
# Runs a comprehensive suite of static analysis, compilation, integration
# tests, concurrency permutations, and Kani formal verification proofs.
#
# Usage:
#   ./scripts/release_check.sh           # Full release check (all stages)
#   ./scripts/release_check.sh --quick   # Fast check (skip Kani & Shuttle)
#   ./scripts/release_check.sh --no-kani # Skip Kani formal verification
#   ./scripts/release_check.sh --help    # Show help and options
# ==============================================================================

set -eo pipefail

# ANSI Color Codes
BOLD="\033[1m"
GREEN="\033[0;32m"
RED="\033[0;31m"
YELLOW="\033[0;33m"
CYAN="\033[0;36m"
RESET="\033[0m"

START_TIME=$(date +%s)
FAILED_STEPS=()

# Configuration Flags
RUN_KANI=true
RUN_SHUTTLE=true
RUN_COVERAGE=true
# Number of parallel Kani workers (one CBMC process per harness).
KANI_JOBS="${KANI_JOBS:-8}"

print_banner() {
    echo -e "${BOLD}${CYAN}"
    echo "======================================================================"
    echo "            OIFS File System — Pre-Release Verification Suite        "
    echo "======================================================================"
    echo -e "${RESET}"
}

log_info() {
    echo -e "${CYAN}[INFO]${RESET} $1"
}

log_pass() {
    echo -e "${GREEN}[PASS]${RESET} $1"
}

log_fail() {
    echo -e "${RED}[FAIL]${RESET} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${RESET} $1"
}

show_help() {
    echo "Usage: $(basename "$0") [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --quick       Run fast verification (skips Kani formal proofs and Shuttle tests)"
    echo "  --no-kani     Skip Kani formal verification proofs"
    echo "  --no-coverage Skip the LLVM code-coverage report"
    echo "  --help, -h    Display this help message and exit"
    echo ""
    exit 0
}

# Parse Command-Line Arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --quick)
            RUN_KANI=false
            RUN_SHUTTLE=false
            RUN_COVERAGE=false
            shift
            ;;
        --no-kani)
            RUN_KANI=false
            shift
            ;;
        --no-coverage)
            RUN_COVERAGE=false
            shift
            ;;
        --help|-h)
            show_help
            ;;
        *)
            echo "Unknown argument: $1"
            show_help
            ;;
    esac
done

# Ensure we run from the project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

print_banner

log_info "Project root: ${PROJECT_ROOT}"
log_info "Rust version: $(rustc --version)"
log_info "Cargo version: $(cargo --version)"

# ------------------------------------------------------------------------------
# STEP 1: Code Formatting Check
# ------------------------------------------------------------------------------
echo ""
log_info "Step 1/8: Checking code formatting (cargo fmt --check)..."
if cargo fmt --check; then
    log_pass "Code formatting is clean."
else
    log_fail "Code formatting check failed. Run 'cargo fmt' to fix."
    FAILED_STEPS+=("Code Formatting (cargo fmt)")
fi

# ------------------------------------------------------------------------------
# STEP 2: Clippy Linting
# ------------------------------------------------------------------------------
echo ""
log_info "Step 2/8: Running clippy linter (cargo clippy --lib --bins -- -D warnings)..."
if cargo clippy --lib --bins -- -D warnings; then
    log_pass "Clippy passed with zero warnings."
else
    log_fail "Clippy linter found issues."
    FAILED_STEPS+=("Clippy Linter")
fi

# ------------------------------------------------------------------------------
# STEP 3: Compilation (Release & MCP Feature)
# ------------------------------------------------------------------------------
echo ""
log_info "Step 3/8: Building release binaries with MCP features (cargo build --release --all-targets --features mcp)..."
if cargo build --release --all-targets --features mcp; then
    log_pass "Release binaries and tests compiled successfully."
else
    log_fail "Release compilation failed."
    FAILED_STEPS+=("Release Compilation")
fi

# ------------------------------------------------------------------------------
# STEP 4: Full Test Suite (Release Mode)
# ------------------------------------------------------------------------------
echo ""
log_info "Step 4/8: Running full test suite (cargo test --release)..."
TEST_LOG=$(mktemp)
if cargo test --release > "${TEST_LOG}" 2>&1; then
    # Verify no FAILED markers inside test output per .agent/rules/cargo_test_verification.md
    if grep -q "FAILED" "${TEST_LOG}" || grep -q "test result: FAILED" "${TEST_LOG}"; then
        log_fail "One or more tests failed inside test runner."
        grep -E "FAILED|test result:" "${TEST_LOG}" | head -30
        FAILED_STEPS+=("Full Test Suite (cargo test --release)")
    else
        PASSED_COUNT=$(grep -E "^test result: ok\." "${TEST_LOG}" | wc -l | tr -d ' ')
        log_pass "All ${PASSED_COUNT} test suites passed cleanly."
    fi
else
    log_fail "cargo test --release exited with non-zero status."
    grep -E "FAILED|error\[" "${TEST_LOG}" | head -30
    FAILED_STEPS+=("Full Test Suite (cargo test --release)")
fi
rm -f "${TEST_LOG}"

# ------------------------------------------------------------------------------
# STEP 5: Shuttle Concurrency Permutation Tests
# ------------------------------------------------------------------------------
echo ""
if [ "${RUN_SHUTTLE}" = true ]; then
    log_info "Step 5/8: Running Shuttle concurrency permutation tests..."
    if cargo test --test shuttle_concurrency_test; then
        log_pass "Shuttle randomized concurrency scheduling tests passed."
    else
        log_fail "Shuttle concurrency test failed."
        FAILED_STEPS+=("Shuttle Concurrency Tests")
    fi
else
    log_warn "Step 5/8: Skipped Shuttle concurrency tests (--quick enabled)."
fi

# ------------------------------------------------------------------------------
# STEP 6: Kani Formal Verification Proofs
# ------------------------------------------------------------------------------
echo ""
if [ "${RUN_KANI}" = true ]; then
    if command -v cargo-kani >/dev/null 2>&1 || cargo kani --version >/dev/null 2>&1; then
        log_info "Step 6/8: Running the full Kani proof suite (${KANI_JOBS} parallel workers)..."
        # `-j` runs one CBMC process per harness; it requires the terse output
        # format. Fall back to the sequential run when the plugin is too old.
        if cargo kani -j "${KANI_JOBS}" --output-format=terse; then
            log_pass "All Kani formal proofs verified successfully."
        elif cargo kani; then
            log_pass "All Kani formal proofs verified successfully (sequential)."
        else
            log_fail "Kani formal verification failed."
            FAILED_STEPS+=("Kani Formal Verification")
        fi
    else
        log_warn "Step 6/8: cargo-kani not found in PATH. Skipping formal verification."
    fi
else
    log_warn "Step 6/8: Skipped Kani formal verification proofs (--no-kani or --quick enabled)."
fi

# ------------------------------------------------------------------------------
# STEP 7: LLVM Code Coverage Report
# ------------------------------------------------------------------------------
echo ""
if [ "${RUN_COVERAGE}" = true ]; then
    # The llvm-tools that ship with the active Rust toolchain understand the
    # raw profile format of that toolchain; the system copy is often older.
    LLVM_BIN_DIR="$(rustc --print sysroot 2>/dev/null)/lib/rustlib/$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')/bin"
    LLVM_PROFDATA=""
    LLVM_COV=""
    for candidate in \
        "${LLVM_BIN_DIR}/llvm-profdata" \
        "$(command -v llvm-profdata 2>/dev/null || true)" \
        "/Library/Developer/CommandLineTools/usr/bin/llvm-profdata"; do
        if [ -n "${candidate}" ] && [ -x "${candidate}" ]; then
            LLVM_PROFDATA="${candidate}"
            break
        fi
    done
    for candidate in \
        "${LLVM_BIN_DIR}/llvm-cov" \
        "$(command -v llvm-cov 2>/dev/null || true)" \
        "/Library/Developer/CommandLineTools/usr/bin/llvm-cov"; do
        if [ -n "${candidate}" ] && [ -x "${candidate}" ]; then
            LLVM_COV="${candidate}"
            break
        fi
    done

    if [ -n "${LLVM_PROFDATA}" ] && [ -n "${LLVM_COV}" ]; then
        PROF_DATA_DIR="$(mktemp -d)"
        log_info "Step 7/8: Collecting LLVM coverage counters..."
        # Build the instrumented test binaries first and remember their paths:
        # cargo's output layout differs between versions, so the paths are
        # taken from cargo itself.
        TEST_BINARIES=$(RUSTFLAGS="-C instrument-coverage" \
            LLVM_PROFILE_FILE="${PROF_DATA_DIR}/oifs-%p-%m.profraw" \
            cargo test --release --no-run --message-format=json 2>/dev/null | \
            python3 -c "
import sys, json
paths = []
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get('reason') == 'compiler-artifact' and msg.get('executable') \
            and msg.get('profile', {}).get('test'):
        paths.append(msg['executable'])
print(' '.join(paths))
")
        if RUSTFLAGS="-C instrument-coverage" \
            LLVM_PROFILE_FILE="${PROF_DATA_DIR}/oifs-%p-%m.profraw" \
            cargo test --release >/dev/null 2>&1; then
            PROF_DATA=$(find "${PROF_DATA_DIR}" -name '*.profraw' | tr '\n' ' ')
            if [ -n "${PROF_DATA}" ]; then
                MERGED="${PROF_DATA_DIR}/oifs.profdata"
                if "${LLVM_PROFDATA}" merge -sparse ${PROF_DATA} -o "${MERGED}" 2>/dev/null; then
                    if [ -z "${TEST_BINARIES}" ]; then
                        log_warn "No test executables found; skipping the coverage report."
                    else
                        COVERAGE_REPORT=$("${LLVM_COV}" report \
                            ${TEST_BINARIES} \
                            -instr-profile="${MERGED}" \
                            -ignore-filename-regex="rustc/|cargo/" 2>/dev/null | tail -6)
                        echo "${COVERAGE_REPORT}" | sed 's/^/    /'
                        log_pass "LLVM coverage report generated."
                    fi
                else
                    log_warn "llvm-profdata merge failed; skipping the coverage report."
                fi
            else
                log_warn "No coverage counters were produced; skipping the coverage report."
            fi
        else
            log_warn "The instrumented test run failed; skipping the coverage report."
        fi
        rm -rf "${PROF_DATA_DIR}"
    else
        log_warn "Step 7/8: no usable llvm-profdata found; skipping the coverage report."
        log_warn "         Install it with: rustup component add llvm-tools-preview"
    fi
else
    log_warn "Step 7/8: Skipped the coverage report (--no-coverage or --quick enabled)."
fi

# ------------------------------------------------------------------------------
# STEP 8: Git Working Tree Cleanliness Check
# ------------------------------------------------------------------------------
echo ""
log_info "Step 8/8: Checking git status for untracked or uncommitted changes..."
DIRTY_FILES=$(git status --porcelain 2>/dev/null | grep -v "^\?\? \.pi/" || true)
if [ -z "${DIRTY_FILES}" ]; then
    log_pass "Git working directory is clean."
else
    log_warn "Git working tree has uncommitted changes:"
    echo "${DIRTY_FILES}" | sed 's/^/    /'
fi

# ------------------------------------------------------------------------------
# Final Summary & Verdict
# ------------------------------------------------------------------------------
END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))

echo ""
echo -e "${BOLD}${CYAN}======================================================================${RESET}"
echo -e "${BOLD}                        VERIFICATION SUMMARY                          ${RESET}"
echo -e "${BOLD}${CYAN}======================================================================${RESET}"
echo -e "Total Time Elapsed: ${DURATION}s"

if [ ${#FAILED_STEPS[@]} -eq 0 ]; then
    echo -e "${BOLD}${GREEN}"
    echo "  [SUCCESS] All pre-release checks passed! Ready for release."
    echo -e "${RESET}"
    exit 0
else
    echo -e "${BOLD}${RED}"
    echo "  [FAILURE] The following checks failed:"
    for step in "${FAILED_STEPS[@]}"; do
        echo "    - ${step}"
    done
    echo -e "${RESET}"
    exit 1
fi
