# Catenary Release Makefile
# Usage:
#   make release-patch   # 0.5.5 -> 0.5.6
#   make release-minor   # 0.5.5 -> 0.6.0
#   make release-major   # 0.5.5 -> 1.0.0
#   make release V=0.6.0 # explicit version

.PHONY: build-release check deny docs test test-ignored test-scripts release release-patch release-minor release-major publish tag-current sync-public

# Get current version from Cargo.toml
CURRENT_VERSION := $(shell grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Files that contain the version
VERSION_FILES := Cargo.toml .claude-plugin/marketplace.json gemini-extension.json

# Default target: run all checks
build-release:
	@cargo build --release

check:
	@cargo fmt -- -l | sed 's/^/fmt: formatted /'
	@cargo clippy --tests --features mockls --quiet -- -D warnings
	@cargo deny --log-level error check
	@cargo nextest run --workspace --features mockls --no-fail-fast --status-level fail --final-status-level fail --cargo-quiet --show-progress only

# Build internal rustdoc (includes private items)
docs:
	@cargo doc --document-private-items --no-deps --quiet

# Run cargo-deny license and advisory checks
deny:
	@cargo deny --log-level error check

# Run Python script tests
test-scripts:
	@python3 -m pytest scripts/test_constrained_bash.py -v 2>/dev/null || python3 scripts/test_constrained_bash.py

# Run tests. Pass T= to filter, N= to repeat, e.g.: make test T=json_diagnostics N=5
# Prefix with ! to exclude: make test T=\!flaky_test
# Run ignored tests (e.g. requiring real LSP): make test-ignored T=ra_symbol_universe
CLEAN_T = $(subst \,,$(subst !,,$(T)))
test-ignored:
	@cargo nextest run --workspace --features mockls --run-ignored ignored-only --status-level all --final-status-level all --no-capture $(if $(T),-E 'test($(CLEAN_T))',)

test:
	@cargo nextest run --workspace --features mockls --status-level fail --final-status-level slow --cargo-quiet $(if $(N),--stress-count $(N),) $(if $(T),$(if $(findstring !,$(T)),-E 'not test($(CLEAN_T))',-E 'test($(T))'),)

# Verify we're in a good state for release
pre-release-check:
	@echo "Checking release prerequisites..."
	@# Clean working tree?
	@if [ -n "$$(git status --porcelain)" ]; then \
		echo "Error: Working tree is not clean. Commit or stash changes first."; \
		exit 1; \
	fi
	@# On main branch?
	@if [ "$$(git branch --show-current)" != "main" ]; then \
		echo "Error: Not on main branch."; \
		exit 1; \
	fi
	@# Up to date with remote?
	@git fetch origin main --quiet
	@if [ "$$(git rev-parse HEAD)" != "$$(git rev-parse origin/main)" ]; then \
		echo "Error: Local main is not up to date with origin/main."; \
		exit 1; \
	fi
	@echo "Prerequisites OK."

# Bump version in all files
bump-version:
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use V=x.y.z"; \
		exit 1; \
	fi
	@echo "Bumping version: $(CURRENT_VERSION) -> $(V)"
	@# Update Cargo.toml
	@sed -i 's/^version = "$(CURRENT_VERSION)"/version = "$(V)"/' Cargo.toml
	@# Update marketplace.json
	@sed -i 's/"version": "$(CURRENT_VERSION)"/"version": "$(V)"/' .claude-plugin/marketplace.json
	@# Update gemini-extension.json
	@sed -i 's/"version": "$(CURRENT_VERSION)"/"version": "$(V)"/' gemini-extension.json
	@# Update Cargo.lock
	@cargo check --quiet
	@echo "Version bumped to $(V)"

# Calculate next patch version (0.5.5 -> 0.5.6)
next-patch:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2"."$$3+1}'))

# Calculate next minor version (0.5.5 -> 0.6.0)
next-minor:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2+1".0"}'))

# Calculate next major version (0.5.5 -> 1.0.0)
next-major:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1+1".0.0"}'))

# Main release target (requires V=x.y.z)
# Rolls back the version bump if checks or commit fail, so it is safe
# to re-run after fixing the issue.
release: pre-release-check
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use 'make release V=x.y.z' or 'make release-patch'"; \
		exit 1; \
	fi
	@cargo update --quiet
	@$(MAKE) bump-version V=$(V)
	@if ! $(MAKE) check; then \
		echo "Checks failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json; \
		exit 1; \
	fi
	@git add Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json
	@if ! git commit -m "chore: Bump version to $(V)"; then \
		echo "Commit failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock .claude-plugin/marketplace.json gemini-extension.json; \
		exit 1; \
	fi
	@git tag -a "v$(V)" -m "Release v$(V)"
	@echo ""
	@echo "Release v$(V) prepared locally."
	@echo "Run 'make publish' to build, push, and create the release."

# Convenience targets
release-patch: pre-release-check next-patch
	@$(MAKE) release V=$(V)

release-minor: pre-release-check next-minor
	@$(MAKE) release V=$(V)

release-major: pre-release-check next-major
	@$(MAKE) release V=$(V)

# Build Linux binary, push to NAS, trigger macOS builds, upload, publish.
# Requires: release commit + tag already created (via make release-*)
GITHUB_REPO := MarkWells-Dev/Catenary
publish:
	@echo "Building Linux binary..."
	@cargo build --release
	@cp target/release/catenary catenary-linux-amd64
	@echo "Pushing to origin..."
	@git push && git push --tags
	@echo "Triggering macOS builds on Ghost..."
	@gh workflow run release.yml -f version=v$(CURRENT_VERSION) --repo $(GITHUB_REPO)
	@echo "Waiting for workflow to start..."
	@sleep 10
	@echo "Waiting for workflow to complete..."
	@gh run watch --repo $(GITHUB_REPO) $$(gh run list --repo $(GITHUB_REPO) --workflow=release.yml --limit=1 --json databaseId --jq '.[0].databaseId')
	@echo "Uploading Linux binary..."
	@gh release upload v$(CURRENT_VERSION) catenary-linux-amd64 --repo $(GITHUB_REPO)
	@echo "Publishing release..."
	@gh release edit v$(CURRENT_VERSION) --draft=false --repo $(GITHUB_REPO)
	@rm -f catenary-linux-amd64
	@echo ""
	@echo "Release v$(CURRENT_VERSION) published."
	@echo "https://github.com/$(GITHUB_REPO)/releases/tag/v$(CURRENT_VERSION)"

# Tag current version (for when you forgot to tag)
tag-current:
	@if git rev-parse "v$(CURRENT_VERSION)" >/dev/null 2>&1; then \
		echo "Tag v$(CURRENT_VERSION) already exists."; \
		exit 1; \
	fi
	@echo "Creating tag v$(CURRENT_VERSION) for current version..."
	@git tag -a "v$(CURRENT_VERSION)" -m "Release v$(CURRENT_VERSION)"
	@echo "Tag created. Run 'make publish' to build and release."

# Sync public files to the dist/ submodule and push to GitHub.
DIST := dist
DIST_FILES := \
	.claude-plugin/marketplace.json \
	plugins/catenary/.claude-plugin/plugin.json \
	plugins/catenary/.mcp.json \
	plugins/catenary/hooks/hooks.json \
	plugins/catenary/config.example.toml \
	plugins/catenary/README.md \
	hooks/hooks.json \
	gemini-extension.json \
	install.sh

sync-public:
	@echo "Syncing public files to dist/..."
	@for f in $(DIST_FILES); do \
		mkdir -p $(DIST)/$$(dirname $$f); \
		cp $$f $(DIST)/$$f; \
	done
	@rsync -a --delete docs/src/ $(DIST)/docs/src/
	@cd $(DIST) && git add -A && \
		if git diff --cached --quiet; then \
			echo "No changes to sync."; \
		else \
			git commit -m "sync: update public files" && \
			git push origin main && \
			echo "Public repo updated."; \
		fi

# Show current version info
version:
	@echo "Current version: $(CURRENT_VERSION)"
	@echo "Latest tag:      $$(git describe --tags --abbrev=0 2>/dev/null || echo 'none')"
	@echo ""
	@echo "Version in files:"
	@grep -H 'version' Cargo.toml | head -1
	@grep -H 'version' .claude-plugin/marketplace.json | grep -v schema
	@grep -H 'version' gemini-extension.json
