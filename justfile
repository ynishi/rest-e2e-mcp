# rest-e2e-mcp / development recipes
# Allow-agent recipes are callable via task-mcp.

# List recipes
default:
    @just --list

# Run the full test suite
[group('allow-agent')]
test:
    cargo test

# Verify the published tarball contents (lists files, no upload)
[group('allow-agent')]
package-list:
    cargo package --list --allow-dirty

# cargo publish --dry-run: builds the .crate, runs verification, no upload
[group('allow-agent')]
publish-dry-run:
    cargo publish --dry-run --allow-dirty
