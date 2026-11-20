#!/usr/bin/env bash
# Offline supply-chain checks; all downloads are intercepted by a fake curl.
set -euo pipefail

SCRIPT="${1:-$(cd "$(dirname "$0")" && pwd)/install.sh}"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/bin" "$ROOT/good" "$ROOT/bad"
printf '#!/bin/sh\nprintf "safe-binary\\n"\n' > "$ROOT/good/memoria"
printf '#!/bin/sh\nprintf "tampered-binary\\n"\n' > "$ROOT/bad/memoria"
chmod +x "$ROOT/good/memoria" "$ROOT/bad/memoria"
# Ensure an already-installed Memoria binary cannot skip the download path.
printf '#!/bin/sh\necho unknown-version\n' > "$ROOT/bin/memoria"
chmod +x "$ROOT/bin/memoria"
tar -czf "$ROOT/good.tgz" -C "$ROOT/good" memoria
tar -czf "$ROOT/bad.tgz" -C "$ROOT/bad" memoria

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64)  target=x86_64-unknown-linux-musl ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *) echo "unsupported test host" >&2; exit 1 ;;
esac
ASSET="memoria-${target}.tar.gz"
hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"
  else shasum -a 256 "$1"; fi
}
printf '%s  %s\n' "$(hash_file "$ROOT/good.tgz" | awk '{print $1}')" "$ASSET" > "$ROOT/good-sums"
printf '%s  %s\n' "$(hash_file "$ROOT/bad.tgz" | awk '{print $1}')" "$ASSET" > "$ROOT/bad-sums"

# Fake curl parses the file output target and URL; never makes a real request.
cat > "$ROOT/bin/curl" <<'CURL'
#!/usr/bin/env bash
set -euo pipefail
out=""; url=""; has_deadline=0
while (($#)); do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -w|--max-time|--connect-timeout|--retry) if [[ "$1" == --max-time ]]; then has_deadline=1; fi; shift 2 ;;
    --max-time=* ) has_deadline=1; shift ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
printf '%s|deadline=%s\n' "$url" "$has_deadline" >> "$CURL_CALLS"
[[ -n "$out" ]] || exit 0
if [[ "$url" == https://github.com/*/SHA256SUMS.txt ]]; then
  case "$SCENARIO" in
    checksum_missing|mirror_checksum_missing) exit 22 ;;
    checksum_mismatch) cp "$BAD_SUMS" "$out" ;;
    *) cp "$GOOD_SUMS" "$out" ;;
  esac
elif [[ "$url" == https://github.com/*/*.tar.gz ]]; then
  case "$SCENARIO" in
    direct_fail|mirror_clean|mirror_tamper|mirror_checksum_missing|default_no_proxy) exit 22 ;;
    slow_direct) if [[ "$has_deadline" == 1 ]]; then exit 28; fi; cp "$GOOD_TGZ" "$out" ;;
    *) cp "$GOOD_TGZ" "$out" ;;
  esac
elif [[ "$url" == https://mirror.example.test/* || "$url" == https://ghfast.top/* ]]; then
  if [[ "$url" == */SHA256SUMS.txt ]]; then
    case "$SCENARIO" in
      mirror_tamper|default_no_proxy) cp "$BAD_SUMS" "$out" ;;
      *) cp "$GOOD_SUMS" "$out" ;;
    esac
  else
    case "$SCENARIO" in
      mirror_tamper|default_no_proxy) cp "$BAD_TGZ" "$out" ;;
      *) cp "$GOOD_TGZ" "$out" ;;
    esac
  fi
else
  echo "unexpected URL $url" >&2
  exit 22
fi
CURL
chmod +x "$ROOT/bin/curl"

export GOOD_TGZ="$ROOT/good.tgz" BAD_TGZ="$ROOT/bad.tgz"
export GOOD_SUMS="$ROOT/good-sums" BAD_SUMS="$ROOT/bad-sums"
export PATH="$ROOT/bin:$PATH" MEMORIA_NO_TELEMETRY=1
passed=0 failed=0
run_test() {
  local id="$1" scenario="$2" proxy="$3" expect_success="$4" expect_good="$5"
  local folder="$ROOT/$id" rc=0
  mkdir -p "$folder/install"
  export SCENARIO="$scenario" CURL_CALLS="$folder/curls.log"
  if [[ "$proxy" == unset ]]; then unset MEMORIA_GHPROXY; else export MEMORIA_GHPROXY="$proxy"; fi
  sh "$SCRIPT" -v v1.0.0 -d "$folder/install" -y --no-telemetry > "$folder/out" 2>&1 || rc=$?
  local failure=''
  if [[ "$expect_success" == yes ]]; then
    [[ "$rc" == 0 ]] || failure="wanted success, got rc=$rc"
    [[ -x "$folder/install/memoria" ]] || failure="successful run did not install"
  else
    [[ "$rc" != 0 ]] || failure="expected rejection but installer succeeded"
    [[ ! -e "$folder/install/memoria" ]] || failure="failed run installed a binary"
  fi
  if [[ "$expect_good" == yes && "$expect_success" == yes ]]; then
    cmp -s "$ROOT/good/memoria" "$folder/install/memoria" || failure="installed bytes not the official archive"
  fi
  if [[ -z "$failure" && "$proxy" == unset ]]; then
    if grep -Eq '^https://(ghfast\.top|mirror\.example\.test)/' "$folder/curls.log"; then failure='used third-party mirror without opt-in'; fi
  fi
  if [[ -z "$failure" && ( "$scenario" == mirror_tamper || "$scenario" == mirror_checksum_missing ) ]]; then
    if ! grep -q '^https://github.com/.*SHA256SUMS.txt' "$folder/curls.log"; then failure='did not fetch official checksum'; fi
  fi
  if [[ -z "$failure" ]]; then
    printf 'PASS %s\n' "$id"; passed=$((passed+1))
  else
    printf 'FAIL %s: %s\n' "$id" "$failure"; failed=$((failed+1))
    tail -6 "$folder/out" | sed 's/^/    /'
  fi
}
run_test verified_direct direct_valid unset yes yes
run_test missing_checksum checksum_missing unset no no
run_test wrong_checksum checksum_mismatch unset no no
run_test no_implicit_mirror default_no_proxy unset no no
run_test mirrored_tampering mirror_tamper https://mirror.example.test no no
run_test clean_opt_in_mirror mirror_clean https://mirror.example.test yes yes
run_test mirror_official_checksum_unavailable mirror_checksum_missing https://mirror.example.test no no
run_test slow_direct_no_total_timeout slow_direct unset yes yes

export SCENARIO=direct_valid CURL_CALLS="$ROOT/dry-run-curls.log"
unset MEMORIA_GHPROXY
sh "$SCRIPT" -v v1.0.0 -n > "$ROOT/dry-run" 2>&1
if grep -q 'Mirror: disabled' "$ROOT/dry-run" && grep -q 'SHA256SUMS.txt' "$ROOT/dry-run"; then
  echo 'PASS dry_run_discloses_trust_sources'; passed=$((passed+1))
else
  echo 'FAIL dry_run_discloses_trust_sources'; failed=$((failed+1))
fi

export SCENARIO=direct_valid CURL_CALLS="$ROOT/opt-in-dry-run-curls.log"
export MEMORIA_GHPROXY=https://mirror.example.test/
sh "$SCRIPT" -v v1.0.0 -n > "$ROOT/opt-in-dry-run" 2>&1
if grep -q '^Mirror: https://mirror.example.test$' "$ROOT/opt-in-dry-run"; then
  echo 'PASS opt_in_mirror_disclosed'; passed=$((passed+1))
else
  echo 'FAIL opt_in_mirror_disclosed'; failed=$((failed+1))
fi

export MEMORIA_GHPROXY=http://mirror.example.test
if sh "$SCRIPT" -v v1.0.0 -n > "$ROOT/insecure-mirror" 2>&1; then
  echo 'FAIL rejects_insecure_mirror'; failed=$((failed+1))
else
  if grep -q 'HTTPS URL' "$ROOT/insecure-mirror"; then
    echo 'PASS rejects_insecure_mirror'; passed=$((passed+1))
  else
    echo 'FAIL rejects_insecure_mirror_no_diagnostic'; failed=$((failed+1))
  fi
fi

printf 'RESULT: %s passed, %s failed\n' "$passed" "$failed"
((failed == 0))
