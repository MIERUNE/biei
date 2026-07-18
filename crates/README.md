# Shared crates

This directory is reserved for small cross-service primitives whose behavior
has first been aligned and covered by shared contract tests. It is not a home
for service policy or a generic `node` grab bag.

The Biei and Ishikari workspaces may depend on a crate here by path while
retaining separate lockfiles and feature resolution.
