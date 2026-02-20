# Catenary Release Makefile
# Usage:
#   make release-patch   # 0.5.5 -> 0.5.6
#   make release-minor   # 0.5.5 -> 0.6.0
#   make release-major   # 0.5.5 -> 1.0.0
#   make release V=0.6.0 # explicit version

.PHONY: check test release release-patch release-minor release-major tag-current

# Get current version from Cargo.toml
CURRENT_VERSION := $(shell grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Files that contain the version
VERSION_FILES := Cargo.toml .claude-plugin/marketplace.json gemini-extension.json

# Default target: run all checks
check:
	@cargo fmt
	@cargo clippy --tests --quiet -- -D warnings
	@cargo deny --log-level error check >/dev/null 2>&1
	@cargo nextest run --status-level fail --final-status-level fail --cargo-quiet --show-progress only

# Run tests. Pass T= to filter, e.g.: make test T=json_diagnostics
test:
	@cargo nextest run --status-level fail --final-status-level slow --cargo-quiet $(if $(T),-E 'test($(T))',)

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
	@echo "Run 'git push && git push --tags' to trigger CD pipeline."

# Convenience targets
release-patch: pre-release-check next-patch
	@$(MAKE) release V=$(V)

release-minor: pre-release-check next-minor
	@$(MAKE) release V=$(V)

release-major: pre-release-check next-major
	@$(MAKE) release V=$(V)

# Tag current version (for when you forgot to tag)
tag-current:
	@if git rev-parse "v$(CURRENT_VERSION)" >/dev/null 2>&1; then \
		echo "Tag v$(CURRENT_VERSION) already exists."; \
		exit 1; \
	fi
	@echo "Creating tag v$(CURRENT_VERSION) for current version..."
	@git tag -a "v$(CURRENT_VERSION)" -m "Release v$(CURRENT_VERSION)"
	@echo "Tag created. Run 'git push --tags' to trigger CD pipeline."

# Show current version info
version:
	@echo "Current version: $(CURRENT_VERSION)"
	@echo "Latest tag:      $$(git describe --tags --abbrev=0 2>/dev/null || echo 'none')"
	@echo ""
	@echo "Version in files:"
	@grep -H 'version' Cargo.toml | head -1
	@grep -H 'version' .claude-plugin/marketplace.json | grep -v schema
	@grep -H 'version' gemini-extension.json
