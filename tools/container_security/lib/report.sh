#!/usr/bin/env bash
# container_security レポート集約（JSON + JUnit サマリ）
set -euo pipefail

aggregate_reports() {
    local json_out="${RESULTS_DIR}/suite_summary.json"
    local junit_out="${RESULTS_DIR}/suite_summary_junit.xml"
    local ts
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    local -a phases=(
        "fuzz:fuzz_report.txt"
        "libfuzzer:libfuzzer_report.txt"
        "libfuzzer_asan:libfuzzer_asan_report.txt"
        "libfuzzer_tsan:libfuzzer_tsan_report.txt"
        "h2spec:h2spec_report.txt"
        "chaos:chaos_report.txt"
        "toxiproxy:toxiproxy_chaos_report.txt"
        "circuit_breaker:circuit_breaker_chaos_report.txt"
        "slowloris:slowloris_chaos_report.txt"
        "bad_backend:bad_backend_report.txt"
        "pumba:pumba_chaos_report.txt"
        "resource_exhaustion:resource_exhaustion_report.txt"
        "security:security_scan_report.txt"
        "testssl:testssl_report.txt"
        "semgrep:semgrep_report.txt"
        "sbom:sbom_report.txt"
        "zap:zap_report.txt"
        "gitleaks:gitleaks_report.txt"
        "smuggling:smuggling_report.txt"
        "differential:differential_report.txt"
        "cargo_audit:cargo_audit_report.txt"
        "cargo_deny:cargo_deny_report.txt"
        "trivy:trivy_report.txt"
        "kernel:kernel_capabilities.txt"
    )

    local passed=0 failed=0 skipped=0
    {
        printf '{\n  "generated_at": "%s",\n  "phases": [\n' "${ts}"
        local first=1
        for entry in "${phases[@]}"; do
            local name="${entry%%:*}"
            local file="${entry##*:}"
            local path="${RESULTS_DIR}/${file}"
            local status="missing"
            if [[ -f "${path}" ]]; then
                if grep -qE ': ok$|chaos: ok|security_scan: ok|testssl: ok|cargo_audit: ok|cargo_deny: ok|libfuzzer: ok' "${path}" 2>/dev/null; then
                    status="passed"
                    passed=$((passed + 1))
                elif grep -qiE 'skipped|skip' "${path}" 2>/dev/null; then
                    status="skipped"
                    skipped=$((skipped + 1))
                else
                    status="failed"
                    failed=$((failed + 1))
                fi
            else
                skipped=$((skipped + 1))
            fi
            [[ "${first}" -eq 1 ]] || printf ',\n'
            first=0
            printf '    {"name": "%s", "report": "%s", "status": "%s"}' "${name}" "${file}" "${status}"
        done
        printf '\n  ],\n  "summary": {"passed": %d, "failed": %d, "skipped": %d}\n}\n' \
            "${passed}" "${failed}" "${skipped}"
    } >"${json_out}"

    {
        printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>'
        printf '<testsuite name="container_security" tests="%d" failures="%d" skipped="%d" timestamp="%s">\n' \
            "$((passed + failed + skipped))" "${failed}" "${skipped}" "${ts}"
        for entry in "${phases[@]}"; do
            local name="${entry%%:*}"
            local file="${entry##*:}"
            local path="${RESULTS_DIR}/${file}"
            if [[ ! -f "${path}" ]]; then
                printf '  <testcase classname="container_security" name="%s"><skipped message="report missing"/></testcase>\n' "${name}"
                continue
            fi
            if grep -qE ': ok$|chaos: ok|security_scan: ok|testssl: ok|cargo_audit: ok|cargo_deny: ok|libfuzzer: ok' "${path}" 2>/dev/null; then
                printf '  <testcase classname="container_security" name="%s"/>\n' "${name}"
            elif grep -qiE 'skipped|skip' "${path}" 2>/dev/null; then
                printf '  <testcase classname="container_security" name="%s"><skipped/></testcase>\n' "${name}"
            else
                printf '  <testcase classname="container_security" name="%s"><failure message="phase failed"/></testcase>\n' "${name}"
            fi
        done
        printf '%s\n' '</testsuite>'
    } >"${junit_out}"

    log "レポート集約: ${json_out} ${junit_out}"
}