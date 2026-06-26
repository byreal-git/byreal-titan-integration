# Byreal Titan integration — developer entry points.
#
#   make build-program   run anchor build for the route program
#   make check-structure lib tests + scorecard assertion + enum parity
#   make test-venue      Byreal venue suite (live checks skip without RPC)
#   make scorecard       print the integration scorecard only
#
# Each phase reports one of:
#   ok       ran and passed
#   skipped  could not run — missing SOLANA_RPC_URL, or route execution is not
#            wired because LiteSVM conflicts with this dependency line
#   red      ran and failed
#   FAILED   a structural check failed
#
# To actually run the RPC-gated off-chain tests:
#   export SOLANA_RPC_URL=https://...   &&   make test-venue
#   make test-venue

PROGRAM := --manifest-path program/Cargo.toml
RELEASE_PROFILE := --release
# Used only for the construction allocation guard. Quote-speed runs in release.
ASSERT_PROFILE := --profile release-debug
SCORECARD = cargo test --quiet $(RELEASE_PROFILE) --test scorecard -- --nocapture 2>/dev/null | sed -n '/^====/,/^====/p'

.PHONY: build-program check-structure test-venue scorecard _unit-phase _venue-phase

# --- always-on checks (no RPC): unit tests, scorecard assertion, enum parity ---
_unit-phase:
	@mkdir -p target
	@printf '\n================ Byreal Titan structural checks =============\n\n'
	@printf '  %-24s  %-8s  %s\n' 'Check' 'Status' 'Detail'
	@printf '  %-24s  %-8s  %s\n' '------------------------' '--------' '----------------------------------------'
	@log=target/log-unit.txt; \
		if cargo test --quiet $(RELEASE_PROFILE) --lib --test scorecard >$$log 2>&1 \
			&& cargo test --quiet $(PROGRAM) --release --lib --test venue_parity >>$$log 2>&1; \
		then st=ok; dt='lib tests + scorecard + enum parity'; \
		else st=FAILED; dt='see log below'; fi; \
		printf '  %-24s  %-8s  %s\n' 'Unit + structure' "$$st" "$$dt"; \
		if [ $$st = FAILED ]; then echo; cat $$log; exit 1; fi

# --- Byreal venue: skips live checks without RPC -------------------------------
_venue-phase:
	@mkdir -p target
	@printf '\n================ Byreal Titan venue checks ==================\n\n'
	@printf '  %-24s  %-8s  %s\n' 'Check' 'Status' 'Detail'
	@printf '  %-24s  %-8s  %s\n' '------------------------' '--------' '----------------------------------------'
	@log=target/log-venue-off.txt; \
		cargo test --quiet $(RELEASE_PROFILE) --test byreal_clmm -- --skip construction --nocapture >$$log 2>&1; rc1=$$?; \
		cargo test --quiet $(ASSERT_PROFILE) --test byreal_clmm -- construction --nocapture >>$$log 2>&1; rc2=$$?; \
		cargo test --quiet $(RELEASE_PROFILE) --test byreal_clmm_creation -- --nocapture >>$$log 2>&1; rc3=$$?; \
		if [ $$rc1 -ne 0 ] || [ $$rc2 -ne 0 ] || [ $$rc3 -ne 0 ]; then st=red; dt='see log below'; \
		elif grep -q 'set SOLANA_RPC_URL' $$log; then st=skipped; dt='set SOLANA_RPC_URL'; \
		elif grep -q 'set BYREAL_CLMM_POOL' $$log; then st=skipped; dt='set BYREAL_CLMM_POOL'; \
		else st=ok; dt='venue suite passed'; fi; \
		printf '  %-24s  %-8s  %s\n' 'Off-chain' "$$st" "$$dt"; \
		if [ $$st = red ]; then echo; cat $$log; exit 1; fi
	@log=target/log-venue-prog.txt; \
		cargo test --quiet $(PROGRAM) --release --test byreal_clmm_route -- --nocapture >$$log 2>&1; rc=$$?; \
		if [ $$rc -ne 0 ]; then st=red; dt='see log below'; \
		elif grep -q 'SKIP' $$log; then st=skipped; dt='route execution not wired in this integration'; \
		else st=ok; dt='route suite passed'; fi; \
		printf '  %-24s  %-8s  %s\n' 'On-chain program' "$$st" "$$dt"; \
		if [ $$st = red ]; then echo; cat $$log; exit 1; fi

# --- public targets -----------------------------------------------------------
build-program:
	@cd program && anchor build

check-structure: _unit-phase

test-venue: _venue-phase
	@echo
	@SCORECARD_SECTION=venue $(SCORECARD)

scorecard:
	@SCORECARD_SECTION=both $(SCORECARD)
